//! Shared task mutation service — the single layer below CLI and MCP, so the
//! two interfaces cannot disagree.
//!
//! Guarantees:
//! - read-modify-write operations hold a project lock (`.nest/lock`), so
//!   concurrent CLI/MCP processes cannot silently overwrite each other, and
//!   id allocation for new tasks is race-free;
//! - every logical update is validated as a complete record, then written
//!   with one atomic same-directory replacement (no partial saves);
//! - unknown/legacy fields in existing files are preserved across mutations
//!   (no data loss on a read/serialize/write cycle);
//! - unsupported and protected fields are rejected, never silently dropped;
//! - external Git side effects run outside the atomic-file claim and never
//!   undo a successful transition.

use anyhow::{bail, Context, Result};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{now_ts, Priority, Task, TaskAction, TaskStatus};
use crate::project::{self, NEST_DIR};
use crate::tasks;

// ===== project lock =====

/// OS-backed advisory lock on a persistent file. The kernel releases ownership
/// when the file closes or the process exits; never unlink a lock another
/// process may already have opened, and never infer ownership from file age.
#[derive(Debug)]
pub struct ProjectLock {
    _file: std::fs::File,
}

const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

impl ProjectLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        Self::acquire_with_timeout(root, DEFAULT_LOCK_TIMEOUT)
    }

    pub fn acquire_with_timeout(root: &Path, timeout: Duration) -> Result<Self> {
        std::fs::create_dir_all(root.join(NEST_DIR))?;
        let path = root.join(NEST_DIR).join("lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= timeout {
                        bail!(
                            "project is locked by another jay process ({}); retry shortly",
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// Canonical ordering also collapses aliases of the same project.
    pub fn acquire_all(roots: &[&Path]) -> Result<Vec<ProjectLock>> {
        let mut sorted: Vec<PathBuf> = roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<std::io::Result<_>>()?;
        sorted.sort();
        sorted.dedup();
        sorted.iter().map(|p| Self::acquire(p)).collect()
    }
}

// ===== patch semantics =====

/// Three-state patch field distinguishing *absent* (unchanged), explicit
/// *null* (clear a nullable field) and a *value*.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchField<T> {
    Absent,
    Null,
    Value(T),
}

// manual impl: derive would add an unnecessary T: Default bound
#[allow(clippy::derivable_impls)]
impl<T> Default for PatchField<T> {
    fn default() -> Self {
        PatchField::Absent
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = PatchField<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a value or null")
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(PatchField::Null)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(PatchField::Null)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                T::deserialize(d).map(PatchField::Value)
            }
        }
        deserializer.deserialize_option(V(PhantomData))
    }
}

/// Fields that generic patches must never touch: lifecycle state, timestamps,
/// actor and report evidence are controlled exclusively by lifecycle and
/// report operations.
pub const PROTECTED_TASK_FIELDS: &[&str] = &[
    "id",
    "status",
    "blocked",
    "block_reason",
    "created_at",
    "started_at",
    "done_at",
    "updated_at",
    "actor",
    "report_result",
    "report_problems",
    "report_ideas",
    "report_decisions",
    "report_validation",
    "git_diagnostics",
    "history",
    "follow_up_of",
];

/// A partial task edit. Omitted fields are unchanged; empty lists clear
/// lists; explicit null clears nullable fields; null for required fields is
/// rejected; protected and unknown fields are rejected.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPatch {
    #[serde(default)]
    pub title: PatchField<String>,
    #[serde(default)]
    pub description: PatchField<String>,
    #[serde(default)]
    pub acceptance: PatchField<Vec<String>>,
    #[serde(default)]
    pub context: PatchField<String>,
    #[serde(default)]
    pub assignee: PatchField<String>,
    #[serde(default)]
    pub priority: PatchField<String>,
    #[serde(default)]
    pub labels: PatchField<Vec<String>>,
    #[serde(default)]
    pub estimate_points: PatchField<i64>,
    #[serde(default)]
    pub estimate_hours: PatchField<f64>,
    #[serde(default)]
    pub deadline: PatchField<String>,
    #[serde(default)]
    pub links: PatchField<Vec<String>>,
    #[serde(default)]
    pub parent_id: PatchField<i64>,
    #[serde(default)]
    pub epic_id: PatchField<i64>,
    #[serde(default)]
    pub milestone_id: PatchField<i64>,
    #[serde(default)]
    pub depends_on: PatchField<Vec<i64>>,
}

/// Parses a patch from JSON text (or a `serde_json::Value`), rejecting
/// protected and unsupported fields with clear messages.
pub fn parse_task_patch(text: &str) -> Result<TaskPatch> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("patch must be valid JSON")?;
    parse_task_patch_value(&value)
}

pub fn parse_task_patch_value(value: &serde_json::Value) -> Result<TaskPatch> {
    let obj = value
        .as_object()
        .context("patch must be a JSON object of task fields")?;
    for key in obj.keys() {
        if PROTECTED_TASK_FIELDS.contains(&key.as_str()) {
            bail!(
                "field '{key}' is protected: status, timestamps, actor and report evidence are controlled by lifecycle/report operations (start/review/done/complete/report), not by patches"
            );
        }
    }
    serde_json::from_value(value.clone()).context("invalid patch payload")
}

fn set_required<T>(name: &str, pf: PatchField<T>, slot: &mut T) -> Result<()> {
    match pf {
        PatchField::Absent => Ok(()),
        PatchField::Null => bail!("field '{name}' is required and cannot be null"),
        PatchField::Value(v) => {
            *slot = v;
            Ok(())
        }
    }
}

fn set_nullable<T>(pf: PatchField<T>, slot: &mut Option<T>) {
    match pf {
        PatchField::Absent => {}
        PatchField::Null => *slot = None,
        PatchField::Value(v) => *slot = Some(v),
    }
}

fn apply_patch(task: &mut Task, p: TaskPatch) -> Result<()> {
    set_required("title", p.title, &mut task.title)?;
    set_required("description", p.description, &mut task.description)?;
    set_required("acceptance", p.acceptance, &mut task.acceptance)?;
    set_required("context", p.context, &mut task.context)?;
    set_nullable(p.assignee, &mut task.assignee);
    match p.priority {
        PatchField::Absent => {}
        PatchField::Null => bail!("field 'priority' is required and cannot be null"),
        PatchField::Value(s) => task.priority = s.parse::<Priority>()?,
    }
    set_required("labels", p.labels, &mut task.labels)?;
    set_nullable(p.estimate_points, &mut task.estimate_points);
    set_nullable(p.estimate_hours, &mut task.estimate_hours);
    set_nullable(p.deadline, &mut task.deadline);
    set_required("links", p.links, &mut task.links)?;
    set_nullable(p.parent_id, &mut task.parent_id);
    set_nullable(p.epic_id, &mut task.epic_id);
    set_nullable(p.milestone_id, &mut task.milestone_id);
    set_required("depends_on", p.depends_on, &mut task.depends_on)?;
    Ok(())
}

/// Legacy PUT-style whole-record replacement (the old MCP `update_task`
/// semantics, kept as a documented legacy operation).
pub struct TaskReplace {
    pub title: String,
    pub description: String,
    pub acceptance: Vec<String>,
    pub context: String,
    pub assignee: Option<String>,
    pub priority: Priority,
    pub labels: Vec<String>,
    pub estimate_points: Option<i64>,
    pub estimate_hours: Option<f64>,
    pub deadline: Option<String>,
    pub links: Vec<String>,
    pub parent_id: Option<i64>,
    pub epic_id: Option<i64>,
    pub milestone_id: Option<i64>,
}

// ===== typed report updates =====

/// A typed report update: any subset of the five report sections in one
/// call. Omitted sections stay unchanged. Lists are real arrays (callers
/// never encode JSON inside strings).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportUpdate {
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub validation: Option<String>,
    #[serde(default)]
    pub problems: Option<Vec<String>>,
    #[serde(default)]
    pub ideas: Option<Vec<String>>,
    #[serde(default)]
    pub decisions: Option<Vec<String>>,
}

impl ReportUpdate {
    fn is_empty(&self) -> bool {
        self.result.is_none()
            && self.validation.is_none()
            && self.problems.is_none()
            && self.ideas.is_none()
            && self.decisions.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportMode {
    /// Omitted sections unchanged; provided sections overwrite.
    Replace,
    /// Provided text sections are appended (separated by a blank line);
    /// provided list sections are appended in order.
    Append,
}

impl std::str::FromStr for ReportMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "replace" => Ok(ReportMode::Replace),
            "append" => Ok(ReportMode::Append),
            other => bail!("unknown report mode: {other} (use replace|append)"),
        }
    }
}

fn append_text(old: Option<&str>, new: &str) -> String {
    match old {
        None => new.to_string(),
        Some(o) if o.trim().is_empty() => new.to_string(),
        Some(o) => format!("{o}\n\n{new}"),
    }
}

fn apply_report(task: &mut Task, upd: &ReportUpdate, mode: ReportMode) -> Result<()> {
    if upd.is_empty() {
        bail!("report update is empty: provide at least one of result/validation/problems/ideas/decisions");
    }
    match mode {
        ReportMode::Replace => {
            if let Some(r) = &upd.result {
                task.report_result = Some(r.clone());
            }
            if let Some(v) = &upd.validation {
                task.report_validation = Some(v.clone());
            }
            if let Some(p) = &upd.problems {
                task.report_problems = p.clone();
            }
            if let Some(i) = &upd.ideas {
                task.report_ideas = i.clone();
            }
            if let Some(d) = &upd.decisions {
                task.report_decisions = d.clone();
            }
        }
        ReportMode::Append => {
            if let Some(r) = &upd.result {
                if !r.trim().is_empty() {
                    task.report_result = Some(append_text(task.report_result.as_deref(), r));
                }
            }
            if let Some(v) = &upd.validation {
                if !v.trim().is_empty() {
                    task.report_validation =
                        Some(append_text(task.report_validation.as_deref(), v));
                }
            }
            if let Some(p) = &upd.problems {
                task.report_problems.extend(p.iter().cloned());
            }
            if let Some(i) = &upd.ideas {
                task.report_ideas.extend(i.iter().cloned());
            }
            if let Some(d) = &upd.decisions {
                task.report_decisions.extend(d.iter().cloned());
            }
        }
    }
    Ok(())
}

// ===== record validation =====

/// Validates a complete proposed record before anything is written.
pub fn validate_record(task: &Task) -> Result<()> {
    if task.title.trim().is_empty() {
        bail!("task title must not be blank");
    }
    for (name, value) in [
        ("created_at", Some(task.created_at.as_str())),
        ("updated_at", Some(task.updated_at.as_str())),
        ("started_at", task.started_at.as_deref()),
        ("done_at", task.done_at.as_deref()),
    ] {
        if let Some(v) = value {
            if crate::diag::parse_ts(v).is_none() {
                bail!("{name} = \"{v}\" is not a valid timestamp");
            }
        }
    }
    if task.status == TaskStatus::Closed {
        let blank = |o: &Option<String>| o.as_deref().unwrap_or("").trim().is_empty();
        if task.done_at.is_none() || blank(&task.report_result) || blank(&task.report_validation) {
            bail!("closed tasks require done_at, a nonblank report_result and report_validation");
        }
    }
    Ok(())
}

// ===== core mutation plumbing =====

/// Loads the raw TOML and typed task, applies `f`, validates the complete
/// record and writes it back atomically — preserving unknown/legacy fields.
/// Caller must hold the project lock (or accept single-process semantics).
fn mutate_task_locked<T>(
    root: &Path,
    id: i64,
    f: impl FnOnce(&mut Task) -> Result<T>,
) -> Result<T> {
    let path = tasks::task_file(root, id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("task {id} not found (expected {})", path.display()))?;
    let raw: toml::Value = toml::from_str(&text).with_context(|| {
        format!(
            "task file {} is malformed; refusing to rewrite it",
            path.display()
        )
    })?;
    let mut task: Task = toml::from_str(&text)
        .with_context(|| format!("task file {} is not a valid task record", path.display()))?;
    let out = f(&mut task)?;
    validate_record(&task)?;
    task.updated_at = now_ts();
    write_task_preserving(&path, &task, &raw)?;
    Ok(out)
}

/// Serializes a task while preserving unknown/legacy keys from the original
/// raw document (e.g. `closed_at`), then writes atomically.
fn write_task_preserving(path: &Path, task: &Task, raw: &toml::Value) -> Result<()> {
    let mut new_value = toml::Value::try_from(task)?;
    if let (Some(orig), Some(tbl)) = (raw.as_table(), new_value.as_table_mut()) {
        for (k, v) in orig {
            let known = crate::diag::KNOWN_TASK_FIELDS.contains(&k.as_str());
            if !known && !tbl.contains_key(k) {
                tbl.insert(k.clone(), v.clone());
            }
        }
    }
    project::atomic_write(path, &toml::to_string_pretty(&new_value)?)
}

/// Loads a task together with its raw document (under no lock; for callers
/// that hold one already).
fn load_raw_and_task(root: &Path, id: i64) -> Result<(toml::Value, Task)> {
    let path = tasks::task_file(root, id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("task {id} not found (expected {})", path.display()))?;
    let raw: toml::Value = toml::from_str(&text)?;
    let task: Task = toml::from_str(&text)?;
    Ok((raw, task))
}

// ===== dependencies and readiness =====

/// Validates a task's `depends_on` edges against the rest of the project:
/// no duplicates, no self-dependency, no missing ids, no cycles. `others`
/// must be every other task in the project (excluding `task` itself).
pub fn check_dependency_edges(task: &Task, others: &[Task]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for d in &task.depends_on {
        if *d == task.id {
            bail!("task {} cannot depend on itself", task.id);
        }
        if !seen.insert(*d) {
            bail!("duplicate dependency #{d} on task {}", task.id);
        }
        if !others.iter().any(|o| o.id == *d) {
            bail!(
                "task {} depends on #{d}, which does not exist in this project (dependencies are local; remove the edge explicitly)",
                task.id
            );
        }
    }
    // cycle detection over the projected graph (task's new edges + existing ones)
    let deps = |id: i64| -> Vec<i64> {
        if id == task.id {
            task.depends_on.clone()
        } else {
            others
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.depends_on.clone())
                .unwrap_or_default()
        }
    };
    let mut path = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    if let Some(cycle) = find_cycle(task.id, &deps, &mut visited, &mut path) {
        bail!(
            "dependency cycle: {}",
            cycle
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }
    Ok(())
}

fn find_cycle(
    id: i64,
    deps: &impl Fn(i64) -> Vec<i64>,
    visited: &mut std::collections::BTreeSet<i64>,
    path: &mut Vec<i64>,
) -> Option<Vec<i64>> {
    if path.contains(&id) {
        let start = path.iter().position(|x| *x == id).unwrap();
        let mut cycle = path[start..].to_vec();
        cycle.push(id);
        return Some(cycle);
    }
    if !visited.insert(id) {
        return None;
    }
    path.push(id);
    for d in deps(id) {
        if let Some(c) = find_cycle(d, deps, visited, path) {
            return Some(c);
        }
    }
    path.pop();
    None
}

/// Validates a task's dependency edges against the on-disk project state.
pub fn validate_dependencies(root: &Path, task: &Task) -> Result<()> {
    let others: Vec<Task> = tasks::load_tasks(root)?
        .into_iter()
        .filter(|t| t.id != task.id)
        .collect();
    check_dependency_edges(task, &others)
}

/// Ids of `t`'s dependencies that are not closed. Cancelled prerequisites do
/// NOT count as completed; missing ids count as unmet.
pub fn unmet_dependencies(all: &[Task], t: &Task) -> Vec<i64> {
    t.depends_on
        .iter()
        .filter(|d| {
            !all.iter()
                .any(|o| o.id == **d && o.status == TaskStatus::Closed)
        })
        .copied()
        .collect()
}

/// Ready work: open, unblocked tasks whose dependencies are all closed,
/// sorted by priority rank then id. (Does not touch the manual blocked flag.)
pub fn ready_tasks(root: &Path) -> Result<Vec<Task>> {
    let all = tasks::load_tasks(root)?;
    let mut ready: Vec<Task> = all
        .iter()
        .filter(|t| t.status == TaskStatus::Open && !t.blocked)
        .filter(|t| unmet_dependencies(&all, t).is_empty())
        .cloned()
        .collect();
    ready.sort_by(|a, b| {
        a.priority
            .rank()
            .cmp(&b.priority.rank())
            .then(a.id.cmp(&b.id))
    });
    Ok(ready)
}

// ===== public operations =====

/// Creates a task with race-free id allocation (holds the project lock).
pub fn create_task(root: &Path, build: impl FnOnce(i64) -> Task) -> Result<Task> {
    let _lock = ProjectLock::acquire(root)?;
    let id = tasks::next_id(root)?;
    let t = build(id);
    validate_record(&t)?;
    validate_dependencies(root, &t)?;
    tasks::save_task(root, &t)?;
    Ok(t)
}

/// Applies a partial patch. Omitted fields unchanged; validation happens on
/// the complete proposed record before the single atomic write.
pub fn patch_task(root: &Path, id: i64, patch: TaskPatch, actor: &str) -> Result<Task> {
    let _lock = ProjectLock::acquire(root)?;
    mutate_task_locked(root, id, |t| {
        apply_patch(t, patch)?;
        validate_dependencies(root, t)?;
        t.actor = Some(actor.to_string());
        Ok(())
    })?;
    tasks::load_task(root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))
}

/// Legacy PUT-style replacement of all editable fields (old MCP `update_task`
/// semantics), routed through the shared validation and atomic write.
pub fn replace_task(root: &Path, id: i64, r: TaskReplace, actor: &str) -> Result<Task> {
    let _lock = ProjectLock::acquire(root)?;
    mutate_task_locked(root, id, |t| {
        t.title = r.title;
        t.description = r.description;
        t.acceptance = r.acceptance;
        t.context = r.context;
        t.assignee = r.assignee;
        t.priority = r.priority;
        t.labels = r.labels;
        t.estimate_points = r.estimate_points;
        t.estimate_hours = r.estimate_hours;
        t.deadline = r.deadline;
        t.links = r.links;
        t.parent_id = r.parent_id;
        t.epic_id = r.epic_id;
        t.milestone_id = r.milestone_id;
        t.actor = Some(actor.to_string());
        Ok(())
    })?;
    tasks::load_task(root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))
}

/// One typed update of any subset of the five report sections.
pub fn update_report(
    root: &Path,
    id: i64,
    upd: ReportUpdate,
    mode: ReportMode,
    actor: &str,
) -> Result<Task> {
    let _lock = ProjectLock::acquire(root)?;
    mutate_task_locked(root, id, |t| {
        apply_report(t, &upd, mode)?;
        t.actor = Some(actor.to_string());
        Ok(())
    })?;
    tasks::load_task(root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))
}

/// Legacy single-section replacement (old MCP `update_report_section`
/// semantics, including the historical quirk that list sections accept a
/// JSON-array-encoded string), routed through shared validation.
pub fn update_report_section_legacy(
    root: &Path,
    id: i64,
    section: &str,
    content: &str,
    actor: &str,
) -> Result<Task> {
    let list = |s: &str| -> Vec<String> {
        serde_json::from_str(s).unwrap_or_else(|_| vec![s.to_string()])
    };
    let upd = match section {
        "result" => ReportUpdate {
            result: Some(content.to_string()),
            ..Default::default()
        },
        "validation" => ReportUpdate {
            validation: Some(content.to_string()),
            ..Default::default()
        },
        "problems" => ReportUpdate {
            problems: Some(list(content)),
            ..Default::default()
        },
        "ideas" => ReportUpdate {
            ideas: Some(list(content)),
            ..Default::default()
        },
        "decisions" => ReportUpdate {
            decisions: Some(list(content)),
            ..Default::default()
        },
        other => bail!(
            "unknown report section: {other} (use result|validation|problems|ideas|decisions)"
        ),
    };
    update_report(root, id, upd, ReportMode::Replace, actor)
}

/// Saves the report and closes a task already in review, in one local atomic
/// mutation. Review is never silently skipped; a validation failure changes
/// nothing (the original bytes stay intact).
pub fn complete_task(root: &Path, id: i64, upd: ReportUpdate, actor: &str) -> Result<Task> {
    let _lock = ProjectLock::acquire(root)?;
    let path = tasks::task_file(root, id);
    let (raw, mut task) = load_raw_and_task(root, id)?;
    if task.status != TaskStatus::Review {
        bail!(
            "complete requires a task in review (task {id} is {}); move it to review first — review is never silently skipped",
            task.status.as_str()
        );
    }
    apply_report(&mut task, &upd, ReportMode::Replace)?;
    // enforces completion evidence; records the lifecycle event
    task.apply(
        TaskAction::Done,
        &crate::model::LifecycleCtx::new(actor, None),
    )?;
    validate_record(&task)?;
    task.actor = Some(actor.to_string());
    task.updated_at = now_ts();
    write_task_preserving(&path, &task, &raw)?;
    Ok(task)
}

/// Lifecycle entry point shared by CLI and MCP: state-machine transition plus
/// best-effort git integration (only when the project is in `auto` mode).
/// Integration failures never undo a successful transition.
pub fn apply_action(
    root: &Path,
    id: i64,
    action: TaskAction,
    actor: &str,
    reason: Option<&str>,
) -> Result<Task> {
    let task = {
        let _lock = ProjectLock::acquire(root)?;
        if action == TaskAction::Start {
            let all = tasks::load_tasks(root)?;
            if let Some(t) = all.iter().find(|t| t.id == id) {
                let unmet = unmet_dependencies(&all, t);
                if !unmet.is_empty() {
                    let list = unmet
                        .iter()
                        .map(|i| format!("#{i}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!("cannot start task {id}: unmet dependencies {list} (a dependency must be closed; cancelled does not count)");
                }
            }
        }
        mutate_task_locked(root, id, |t| {
            t.apply(action, &crate::model::LifecycleCtx::new(actor, reason))?;
            Ok(())
        })?;
        tasks::load_task(root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))?
    };
    // External side effects run outside the lock/atomic-file claim.
    let config = project::load_config(root)?;
    if config.effective_git_integration() == project::GitIntegration::Auto {
        match action {
            TaskAction::Start => crate::gitflow::auto_branch(root, id)?,
            TaskAction::Review => crate::gitflow::auto_pr(root, id)?,
            _ => {}
        }
        return tasks::load_task(root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"));
    }
    Ok(task)
}

/// Creates an explicit follow-up task linked to `original_id` via
/// `follow_up_of`. The original task is left unchanged (it stays closed when
/// closed); a normal open task is created with the supplied title. No
/// dependency is implied unless one is requested separately.
pub fn create_follow_up(
    root: &Path,
    original_id: i64,
    title: String,
    description: String,
    actor: &str,
) -> Result<Task> {
    if title.trim().is_empty() {
        bail!("a follow-up requires a supplied nonblank title");
    }
    if tasks::load_task(root, original_id)?.is_none() {
        bail!("task {original_id} not found");
    }
    create_task(root, |id| {
        let mut t = Task::new(id, title, description);
        t.follow_up_of = Some(original_id);
        t.actor = Some(actor.to_string());
        t
    })
}

/// Moves a task to another project (new id, unknown fields preserved).
/// Both projects are locked in a deterministic order.
pub fn move_task(root: &Path, id: i64, dest: &Path, actor: &str) -> Result<Task> {
    let _locks = ProjectLock::acquire_all(&[root, dest])?;
    let (raw, task) = load_raw_and_task(root, id)?;
    if !task.depends_on.is_empty() {
        bail!(
            "task {id} depends on {} in this project; remove those dependency edges explicitly before moving it across projects (dependencies are project-local)",
            task.depends_on.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
        );
    }
    let all_source = tasks::load_tasks(root)?;
    let dependents: Vec<i64> = all_source
        .iter()
        .filter(|t| t.depends_on.contains(&id))
        .map(|t| t.id)
        .collect();
    if !dependents.is_empty() {
        bail!(
            "tasks {} depend on task {id} in this project; remove those dependency edges explicitly before moving it (no dangling ids are created)",
            dependents.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(orig) = task.follow_up_of {
        bail!(
            "task {id} is a follow-up of task #{orig} in this project; clear follow_up_of explicitly before moving it (references must not silently break)"
        );
    }
    let followers: Vec<i64> = all_source
        .iter()
        .filter(|t| t.follow_up_of == Some(id))
        .map(|t| t.id)
        .collect();
    if !followers.is_empty() {
        bail!(
            "tasks {} are follow-ups of task {id}; clear their follow_up_of references explicitly before moving it",
            followers.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
        );
    }
    let new_id = tasks::next_id(dest)?;
    let mut moved = task.clone();
    moved.id = new_id;
    moved.actor = Some(actor.to_string());
    moved.updated_at = now_ts();
    validate_record(&moved)?;
    let dest_path = tasks::task_file(dest, new_id);
    if let Some(dir) = dest_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_task_preserving(&dest_path, &moved, &raw)?;
    tasks::delete_task(root, id)?;
    Ok(moved)
}

/// Combined project context: summary + provenance + freshness, task counts,
/// active/blocked tasks and ready work. One shared implementation behind
/// `jay status` and the MCP `project_context` tool (CLI/MCP parity).
pub fn project_context(root: &Path) -> Result<serde_json::Value> {
    let cfg = project::load_config(root)?;
    let all = tasks::load_tasks(root)?;
    let mut counts = serde_json::Map::new();
    for t in &all {
        *counts
            .entry(t.status.as_str().to_string())
            .or_insert(serde_json::Value::from(0i64)) = serde_json::Value::from(
            counts
                .get(t.status.as_str())
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + 1,
        );
    }
    let brief = |t: &Task| {
        serde_json::json!({
            "id": t.id,
            "title": t.title,
            "priority": t.priority.as_str(),
        })
    };
    let active: Vec<_> = all
        .iter()
        .filter(|t| t.status == TaskStatus::Started)
        .map(brief)
        .collect();
    let blocked: Vec<_> = all
        .iter()
        .filter(|t| t.blocked)
        .map(|t| {
            let mut b = brief(t);
            b["block_reason"] = serde_json::Value::from(t.block_reason.clone());
            b
        })
        .collect();
    let ready: Vec<_> = ready_tasks(root)?.iter().map(brief).collect();

    let summary = match crate::knowledge::current_status(root)? {
        Some(cs) => {
            let freshness = crate::knowledge::summary_freshness(root, &cs)?;
            Some(serde_json::json!({
                "entry": cs.entry,
                "designated": cs.designated,
                "designation": cs.designation,
                "note": cs.note,
                "freshness": freshness,
            }))
        }
        None => None,
    };

    Ok(serde_json::json!({
        "project": cfg.name,
        "root": root.display().to_string(),
        "git_integration": cfg.effective_git_integration().as_str(),
        "counts": counts,
        "active": active,
        "blocked": blocked,
        "ready": ready,
        "summary": summary,
    }))
}

/// Appends a link to a task (deduplicated), under the project lock.
pub fn add_task_link(root: &Path, task_id: i64, link: &str) -> Result<()> {
    let _lock = ProjectLock::acquire(root)?;
    mutate_task_locked(root, task_id, |t| {
        if !t.links.iter().any(|l| l == link) {
            t.links.push(link.to_string());
        }
        Ok(())
    })?;
    Ok(())
}

/// Appends a structured integration diagnostic, under the project lock.
pub fn add_git_diagnostic(root: &Path, task_id: i64, operation: &str, msg: &str) -> Result<()> {
    let _lock = ProjectLock::acquire(root)?;
    mutate_task_locked(root, task_id, |t| {
        t.git_diagnostics.push(crate::model::GitDiagnostic {
            operation: operation.to_string(),
            message: msg.to_string(),
            at: now_ts(),
        });
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KnowledgeEntry, KnowledgeKind, TaskAction, TaskStatus};
    use crate::project::init_project;
    use crate::{knowledge, tasks};

    fn proj() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        init_project(base.path(), None).unwrap();
        base
    }

    fn seeded(root: &Path) -> i64 {
        let t = create_task(root, |id| {
            let mut t = Task::new(id, "Seed task".into(), "seed desc".into());
            t.labels = vec!["keep".into()];
            t.estimate_points = Some(3);
            t.links = vec!["https://example.com".into()];
            t.assignee = Some("ronny".into());
            t.actor = Some("human".into());
            t
        })
        .unwrap();
        t.id
    }

    fn raw_file(root: &Path, id: i64) -> String {
        std::fs::read_to_string(tasks::task_file(root, id)).unwrap()
    }

    #[test]
    fn patch_only_description_preserves_everything_else() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let t = patch_task(
            r,
            id,
            parse_task_patch(r#"{"description":"new desc"}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert_eq!(t.description, "new desc");
        assert_eq!(t.title, "Seed task");
        assert_eq!(t.labels, vec!["keep"]);
        assert_eq!(t.estimate_points, Some(3));
        assert_eq!(t.links, vec!["https://example.com"]);
        assert_eq!(t.assignee.as_deref(), Some("ronny"));
    }

    #[test]
    fn patch_distinguishes_absent_null_and_empty_from_real_json() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        // explicit null clears a nullable field
        let t = patch_task(
            r,
            id,
            parse_task_patch(r#"{"assignee":null}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert_eq!(t.assignee, None);
        // empty list clears the list
        let t = patch_task(
            r,
            id,
            parse_task_patch(r#"{"labels":[]}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert!(t.labels.is_empty());
        // null for a required field is rejected at apply time
        assert!(patch_task(
            r,
            id,
            parse_task_patch(r#"{"title":null}"#).unwrap(),
            "human"
        )
        .is_err());
        assert!(patch_task(
            r,
            id,
            parse_task_patch(r#"{"description":null}"#).unwrap(),
            "human"
        )
        .is_err());
        // absent fields stay untouched
        let t = patch_task(
            r,
            id,
            parse_task_patch(r#"{"context":"ctx"}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert_eq!(t.title, "Seed task");
        assert_eq!(t.context, "ctx");
    }

    #[test]
    fn patch_rejects_protected_and_unknown_fields() {
        let e = parse_task_patch(r#"{"status":"open"}"#).unwrap_err();
        assert!(e.to_string().contains("protected"), "{e}");
        let e = parse_task_patch(r#"{"report_result":"x"}"#).unwrap_err();
        assert!(e.to_string().contains("protected"), "{e}");
        let e = parse_task_patch(r#"{"done_at":"2026-01-01T00:00:00Z"}"#).unwrap_err();
        assert!(e.to_string().contains("protected"), "{e}");
        assert!(parse_task_patch(r#"{"bogus_field":1}"#).is_err());
    }

    #[test]
    fn mutation_preserves_unknown_file_fields() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        // hand-edit: add legacy/unknown fields
        let path = tasks::task_file(r, id);
        let mut text = raw_file(r, id);
        text.push_str("closed_at = \"2026-09-13T18:45:00Z\"\nschema_version = 99\n");
        std::fs::write(&path, text).unwrap();

        patch_task(
            r,
            id,
            parse_task_patch(r#"{"description":"edited"}"#).unwrap(),
            "human",
        )
        .unwrap();
        let after = raw_file(r, id);
        assert!(
            after.contains("closed_at"),
            "legacy field must survive: {after}"
        );
        assert!(
            after.contains("schema_version = 99"),
            "unknown field must survive: {after}"
        );
        assert!(after.contains("edited"));
    }

    #[test]
    fn report_update_replace_and_append_typed() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let upd: ReportUpdate = serde_json::from_str(
            r#"{"result":"first","problems":["p1"],"ideas":["i1"],"decisions":["d1"]}"#,
        )
        .unwrap();
        let t = update_report(r, id, upd, ReportMode::Replace, "human").unwrap();
        assert_eq!(t.report_result.as_deref(), Some("first"));
        assert_eq!(t.report_problems, vec!["p1"]);

        let upd2: ReportUpdate =
            serde_json::from_str(r#"{"result":"second","problems":["p2"],"validation":"v"}"#)
                .unwrap();
        let t = update_report(r, id, upd2, ReportMode::Append, "human").unwrap();
        assert_eq!(t.report_result.as_deref(), Some("first\n\nsecond"));
        assert_eq!(t.report_problems, vec!["p1", "p2"]);
        assert_eq!(t.report_validation.as_deref(), Some("v"));
        // omitted sections (ideas/decisions) unchanged
        assert_eq!(t.report_ideas, vec!["i1"]);
        assert_eq!(t.report_decisions, vec!["d1"]);
    }

    #[test]
    fn report_update_rejects_unknown_sections_and_empty_payload() {
        assert!(serde_json::from_str::<ReportUpdate>(r#"{"bogus":"x"}"#).is_err());
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        assert!(
            update_report(r, id, ReportUpdate::default(), ReportMode::Replace, "human").is_err()
        );
    }

    #[test]
    fn complete_task_saves_report_and_closes_atomically() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        apply_action(r, id, TaskAction::Start, "human", None).unwrap();
        apply_action(r, id, TaskAction::Review, "human", None).unwrap();
        let upd: ReportUpdate = serde_json::from_str(
            r#"{"result":"delivered","validation":"tests pass","problems":[],"ideas":["later"],"decisions":[]}"#,
        )
        .unwrap();
        let t = complete_task(r, id, upd, "human").unwrap();
        assert_eq!(t.status, TaskStatus::Closed);
        assert_eq!(t.report_result.as_deref(), Some("delivered"));
        assert_eq!(t.report_validation.as_deref(), Some("tests pass"));
        assert_eq!(t.report_ideas, vec!["later"]);
        assert!(t.done_at.is_some());
    }

    #[test]
    fn complete_task_requires_review_and_leaves_bytes_intact_on_failure() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let upd: ReportUpdate = serde_json::from_str(r#"{"result":"r","validation":"v"}"#).unwrap();

        // not in review (open): refused, bytes unchanged
        let before = raw_file(r, id);
        let e = complete_task(r, id, upd.clone(), "human").unwrap_err();
        assert!(e.to_string().contains("review"), "{e}");
        assert_eq!(raw_file(r, id), before);

        // in review but missing evidence: refused, bytes unchanged
        apply_action(r, id, TaskAction::Start, "human", None).unwrap();
        apply_action(r, id, TaskAction::Review, "human", None).unwrap();
        let before = raw_file(r, id);
        let partial: ReportUpdate = serde_json::from_str(r#"{"result":"only result"}"#).unwrap();
        assert!(complete_task(r, id, partial, "human").is_err());
        assert_eq!(raw_file(r, id), before);
        let t = tasks::load_task(r, id).unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Review);
    }

    #[test]
    fn done_action_also_requires_evidence() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        apply_action(r, id, TaskAction::Start, "human", None).unwrap();
        apply_action(r, id, TaskAction::Review, "human", None).unwrap();
        let e = apply_action(r, id, TaskAction::Done, "human", None).unwrap_err();
        assert!(e.to_string().contains("evidence"), "{e}");
    }

    #[test]
    fn legacy_report_section_api_keeps_replacement_semantics() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let t = update_report_section_legacy(r, id, "result", "v1", "human").unwrap();
        assert_eq!(t.report_result.as_deref(), Some("v1"));
        let t = update_report_section_legacy(r, id, "result", "v2", "human").unwrap();
        assert_eq!(t.report_result.as_deref(), Some("v2"));
        // legacy quirk: JSON-array-encoded string becomes a list
        let t = update_report_section_legacy(r, id, "problems", r#"["a","b"]"#, "human").unwrap();
        assert_eq!(t.report_problems, vec!["a", "b"]);
        let t = update_report_section_legacy(r, id, "ideas", "plain text", "human").unwrap();
        assert_eq!(t.report_ideas, vec!["plain text"]);
        assert!(update_report_section_legacy(r, id, "bogus", "x", "human").is_err());
    }

    #[test]
    fn legacy_replace_task_replaces_all_editable_fields() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let t = replace_task(
            r,
            id,
            TaskReplace {
                title: "Replaced".into(),
                description: String::new(),
                acceptance: Vec::new(),
                context: String::new(),
                assignee: None,
                priority: Priority::High,
                labels: Vec::new(),
                estimate_points: None,
                estimate_hours: None,
                deadline: None,
                links: Vec::new(),
                parent_id: None,
                epic_id: None,
                milestone_id: None,
            },
            "agent",
        )
        .unwrap();
        assert_eq!(t.title, "Replaced");
        assert!(t.labels.is_empty(), "legacy PUT clears omitted fields");
        assert_eq!(t.estimate_points, None);
        assert_eq!(t.priority, Priority::High);
    }

    #[test]
    fn lock_blocks_second_writer_and_is_released_on_drop() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let lock = ProjectLock::acquire(r).unwrap();
        // second acquire with a short timeout fails instead of clobbering
        let e = ProjectLock::acquire_with_timeout(r, Duration::from_millis(100)).unwrap_err();
        assert!(e.to_string().contains("locked"), "{e}");
        drop(lock);
        // after release, mutation succeeds
        patch_task(
            r,
            id,
            parse_task_patch(r#"{"context":"ok"}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert!(
            r.join(NEST_DIR).join("lock").exists(),
            "lock file stays in place to preserve its identity"
        );
    }

    #[test]
    fn concurrent_conflicting_updates_do_not_silently_overwrite() {
        // Two processes/threads patching different fields of the same task
        // concurrently must both land: the lock serializes read-modify-write,
        // so neither update is lost.
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let mut handles = Vec::new();
        for patch_json in [
            r#"{"labels":["concurrent"]}"#.to_string(),
            r#"{"context":"concurrent"}"#.to_string(),
        ] {
            let root_owned = r.to_path_buf();
            handles.push(std::thread::spawn(move || {
                let patch = parse_task_patch(&patch_json).unwrap();
                patch_task(&root_owned, id, patch, "human").unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let t = tasks::load_task(r, id).unwrap().unwrap();
        assert_eq!(t.labels, vec!["concurrent"], "labels update lost");
        assert_eq!(t.context, "concurrent", "context update lost");
    }

    #[test]
    fn create_task_allocates_sequential_ids_under_lock() {
        let root = proj();
        let r = root.path();
        let a = create_task(r, |id| Task::new(id, "a".into(), String::new())).unwrap();
        let b = create_task(r, |id| Task::new(id, "b".into(), String::new())).unwrap();
        assert_eq!(a.id + 1, b.id);
        assert_eq!(tasks::load_tasks(r).unwrap().len(), 2);
    }

    #[test]
    fn move_task_preserves_unknown_fields_and_reassigns_id() {
        let base = tempfile::tempdir().unwrap();
        let src = base.path().join("src");
        let dst = base.path().join("dst");
        init_project(&src, None).unwrap();
        init_project(&dst, None).unwrap();
        let id = seeded(&src);
        let path = tasks::task_file(&src, id);
        let mut text = raw_file(&src, id);
        text.push_str("custom = \"kept\"\n");
        std::fs::write(&path, text).unwrap();

        let moved = move_task(&src, id, &dst, "human").unwrap();
        assert_eq!(moved.id, 1);
        assert!(tasks::load_task(&src, id).unwrap().is_none());
        let dest_text = raw_file(&dst, moved.id);
        assert!(dest_text.contains("custom = \"kept\""), "{dest_text}");
    }

    #[test]
    fn validation_rejects_bad_records_before_write() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        let before = raw_file(r, id);
        // blank title rejected
        assert!(patch_task(
            r,
            id,
            parse_task_patch(r#"{"title":"  "}"#).unwrap(),
            "human"
        )
        .is_err());
        assert_eq!(raw_file(r, id), before);
    }

    #[test]
    fn knowledge_entries_still_roundtrip() {
        // smoke: KB writes remain compatible after atomic-write switch
        let root = proj();
        let r = root.path();
        let e = KnowledgeEntry::new(0, KnowledgeKind::Note, "n".into(), "c".into());
        let saved = knowledge::add_entry(r, e).unwrap();
        assert_eq!(saved.id, 1);
    }

    // ===== stage 4: dependencies =====

    fn task_with_deps(root: &Path, title: &str, deps: Vec<i64>) -> Task {
        create_task(root, move |id| {
            let mut t = Task::new(id, title.into(), String::new());
            t.depends_on = deps;
            t
        })
        .unwrap()
    }

    fn close(root: &Path, id: i64) {
        apply_action(root, id, TaskAction::Start, "human", None).unwrap();
        apply_action(root, id, TaskAction::Review, "human", None).unwrap();
        let upd: ReportUpdate = serde_json::from_str(r#"{"result":"r","validation":"v"}"#).unwrap();
        complete_task(root, id, upd, "human").unwrap();
    }

    #[test]
    fn dependency_chain_gates_ready_and_start() {
        let root = proj();
        let r = root.path();
        let a = task_with_deps(r, "a", vec![]);
        let b = task_with_deps(r, "b", vec![a.id]);
        let c = task_with_deps(r, "c", vec![b.id]);

        // only A is ready
        let ready = ready_tasks(r).unwrap();
        assert_eq!(ready.iter().map(|t| t.id).collect::<Vec<_>>(), vec![a.id]);

        // starting B with unmet deps is refused with the relevant ids
        let e = apply_action(r, b.id, TaskAction::Start, "human", None).unwrap_err();
        assert!(e.to_string().contains(&format!("#{}", a.id)), "{e}");

        // close A -> B ready; C still not
        close(r, a.id);
        let ready = ready_tasks(r).unwrap();
        assert_eq!(ready.iter().map(|t| t.id).collect::<Vec<_>>(), vec![b.id]);
        apply_action(r, b.id, TaskAction::Start, "human", None).unwrap();
        assert!(ready_tasks(r).unwrap().is_empty());
        let _ = c;
    }

    #[test]
    fn cancelled_dependency_does_not_count_as_completed() {
        let root = proj();
        let r = root.path();
        let a = task_with_deps(r, "a", vec![]);
        let b = task_with_deps(r, "b", vec![a.id]);
        apply_action(r, a.id, TaskAction::Cancel, "human", None).unwrap();
        assert!(ready_tasks(r).unwrap().is_empty());
        let e = apply_action(r, b.id, TaskAction::Start, "human", None).unwrap_err();
        assert!(e.to_string().contains(&format!("#{}", a.id)), "{e}");
        assert!(e.to_string().contains("cancelled does not count"), "{e}");
    }

    #[test]
    fn dependency_edges_reject_self_duplicate_missing_and_cycles() {
        let root = proj();
        let r = root.path();
        let a = task_with_deps(r, "a", vec![]);
        let b = task_with_deps(r, "b", vec![a.id]);

        // self dependency via patch
        let p = parse_task_patch(&format!(r#"{{"depends_on":[{}]}}"#, b.id)).unwrap();
        assert!(patch_task(r, b.id, p, "human").is_err());
        // duplicate
        let p = parse_task_patch(&format!(r#"{{"depends_on":[{},{}]}}"#, a.id, a.id)).unwrap();
        assert!(patch_task(r, b.id, p, "human").is_err());
        // missing
        let p = parse_task_patch(r#"{"depends_on":[999]}"#).unwrap();
        assert!(patch_task(r, b.id, p, "human").is_err());
        // cycle: a -> b while b -> a
        let p = parse_task_patch(&format!(r#"{{"depends_on":[{}]}}"#, b.id)).unwrap();
        let e = patch_task(r, a.id, p, "human").unwrap_err();
        assert!(e.to_string().contains("cycle"), "{e}");
        // on create too
        let e = create_task(r, |id| {
            let mut t = Task::new(id, "loop".into(), String::new());
            t.depends_on = vec![id];
            t
        })
        .unwrap_err();
        assert!(e.to_string().contains("itself"), "{e}");
    }

    #[test]
    fn ready_sorts_by_priority_then_id() {
        let root = proj();
        let r = root.path();
        let low = create_task(r, |id| {
            let mut t = Task::new(id, "low".into(), String::new());
            t.priority = Priority::Low;
            t
        })
        .unwrap();
        let high1 = create_task(r, |id| {
            let mut t = Task::new(id, "high1".into(), String::new());
            t.priority = Priority::High;
            t
        })
        .unwrap();
        let high0 = create_task(r, |id| {
            let mut t = Task::new(id, "high0".into(), String::new());
            t.priority = Priority::High;
            t
        })
        .unwrap();
        let ready = ready_tasks(r).unwrap();
        assert_eq!(
            ready.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![high1.id, high0.id, low.id]
        );
    }

    #[test]
    fn manual_block_excludes_from_ready_without_touching_deps() {
        let root = proj();
        let r = root.path();
        let a = task_with_deps(r, "a", vec![]);
        apply_action(r, a.id, TaskAction::Block, "human", Some("waiting")).unwrap();
        assert!(ready_tasks(r).unwrap().is_empty());
        apply_action(r, a.id, TaskAction::Unblock, "human", None).unwrap();
        assert_eq!(ready_tasks(r).unwrap().len(), 1);
    }

    #[test]
    fn cross_project_move_refused_while_dependency_edges_exist() {
        let base = tempfile::tempdir().unwrap();
        let src = base.path().join("src");
        let dst = base.path().join("dst");
        init_project(&src, None).unwrap();
        init_project(&dst, None).unwrap();
        let a = task_with_deps(&src, "a", vec![]);
        let b = task_with_deps(&src, "b", vec![a.id]);

        // outgoing edge
        let e = move_task(&src, b.id, &dst, "human").unwrap_err();
        assert!(e.to_string().contains("depend"), "{e}");
        // incoming edge
        let e = move_task(&src, a.id, &dst, "human").unwrap_err();
        assert!(e.to_string().contains("depend"), "{e}");
        // neither repo modified
        assert!(tasks::load_task(&src, a.id).unwrap().is_some());
        assert!(tasks::load_task(&src, b.id).unwrap().is_some());
        assert!(tasks::load_tasks(&dst).unwrap().is_empty());

        // after explicitly removing the edge, the move succeeds
        let p = parse_task_patch(r#"{"depends_on":[]}"#).unwrap();
        patch_task(&src, b.id, p, "human").unwrap();
        let moved = move_task(&src, b.id, &dst, "human").unwrap();
        assert_eq!(moved.id, 1);
        assert!(tasks::load_task(&src, b.id).unwrap().is_none());
    }

    #[test]
    fn legacy_files_read_with_empty_dependency_list() {
        let root = proj();
        let r = root.path();
        std::fs::write(
            r.join(".nest/tasks/1.toml"),
            r#"id = 1
title = "legacy"
description = ""
status = "open"
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-11T01:13:03"
updated_at = "2026-09-11T01:13:03"
"#,
        )
        .unwrap();
        let t = tasks::load_task(r, 1).unwrap().unwrap();
        assert!(t.depends_on.is_empty());
        assert_eq!(ready_tasks(r).unwrap().len(), 1);
    }

    #[test]
    fn doctor_flags_dependency_problems_in_hand_edited_data() {
        let root = proj();
        let r = root.path();
        // hand-edited cycle 1 -> 2 -> 1, and active dependent of open prereq
        std::fs::write(
            r.join(".nest/tasks/1.toml"),
            r#"id = 1
title = "one"
description = ""
status = "started"
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
depends_on = [2]
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-11T01:13:03"
started_at = "2026-09-11T01:14:03"
updated_at = "2026-09-11T01:14:03"
"#,
        )
        .unwrap();
        std::fs::write(
            r.join(".nest/tasks/2.toml"),
            r#"id = 2
title = "two"
description = ""
status = "open"
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
depends_on = [1, 1, 99]
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-11T01:13:03"
updated_at = "2026-09-11T01:13:03"
"#,
        )
        .unwrap();
        let f = crate::diag::inspect(r);
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.dependency_cycle"), "{codes:?}");
        assert!(codes.contains(&"task.depends_on_duplicate"), "{codes:?}");
        assert!(codes.contains(&"task.depends_on_missing"), "{codes:?}");
        assert!(
            codes.contains(&"task.dependency_inconsistency"),
            "{codes:?}"
        );
    }

    // ===== stage 6: reopen + follow-ups =====

    #[test]
    fn closed_reopen_without_reason_fails_with_reason_starts_fresh_cycle() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        close(r, id);
        assert!(apply_action(r, id, TaskAction::Reopen, "human", None).is_err());
        let t = apply_action(r, id, TaskAction::Reopen, "human", Some("review fix")).unwrap();
        assert_eq!(t.status, TaskStatus::Open);
        assert!(t.report_result.is_none());
        assert_eq!(
            t.history
                .last()
                .unwrap()
                .prior_report
                .as_ref()
                .unwrap()
                .result
                .as_deref(),
            Some("r")
        );
        // reclosing needs fresh evidence
        apply_action(r, id, TaskAction::Start, "human", None).unwrap();
        apply_action(r, id, TaskAction::Review, "human", None).unwrap();
        assert!(complete_task(
            r,
            id,
            serde_json::from_str(r#"{"result":"  ","validation":"v"}"#).unwrap(),
            "human"
        )
        .is_err());
        let t = complete_task(
            r,
            id,
            serde_json::from_str(r#"{"result":"fresh","validation":"fresh v"}"#).unwrap(),
            "human",
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Closed);
        assert_eq!(t.report_result.as_deref(), Some("fresh"));
        // problems/ideas/decisions from cycle 1 were preserved through reopen
        // (seeded has none; verify via the history snapshot instead)
        assert!(t.history.iter().any(|e| e.prior_report.is_some()));
    }

    #[test]
    fn follow_up_leaves_original_unchanged() {
        let root = proj();
        let r = root.path();
        let id = seeded(r);
        close(r, id);
        let before = raw_file(r, id);
        let fu =
            create_follow_up(r, id, "Polish edge cases".into(), "desc".into(), "human").unwrap();
        assert_eq!(fu.follow_up_of, Some(id));
        assert_eq!(fu.status, TaskStatus::Open);
        assert!(fu.depends_on.is_empty(), "no implied dependency");
        // original closed task byte-identical
        assert_eq!(raw_file(r, id), before);
        // blank title refused
        assert!(create_follow_up(r, id, " ".into(), String::new(), "human").is_err());
        // missing original refused
        assert!(create_follow_up(r, 999, "x".into(), String::new(), "human").is_err());
    }

    #[test]
    fn patches_cannot_touch_history_or_follow_up_links() {
        assert!(parse_task_patch(r#"{"history":[]}"#).is_err());
        assert!(parse_task_patch(r#"{"follow_up_of":1}"#).is_err());
    }

    #[test]
    fn move_refused_while_follow_up_references_exist() {
        let base = tempfile::tempdir().unwrap();
        let src = base.path().join("src");
        let dst = base.path().join("dst");
        init_project(&src, None).unwrap();
        init_project(&dst, None).unwrap();
        let id = seeded(&src);
        let fu = create_follow_up(&src, id, "fu".into(), String::new(), "human").unwrap();
        // outgoing reference
        assert!(move_task(&src, fu.id, &dst, "human").is_err());
        // incoming reference
        assert!(move_task(&src, id, &dst, "human").is_err());
        // nothing moved
        assert!(tasks::load_task(&src, id).unwrap().is_some());
        assert!(tasks::load_tasks(&dst).unwrap().is_empty());
    }

    #[test]
    fn reopened_prerequisite_flags_dependents_in_doctor_not_rewound() {
        let root = proj();
        let r = root.path();
        let a = task_with_deps(r, "a", vec![]);
        let b = task_with_deps(r, "b", vec![a.id]);
        close(r, a.id);
        close(r, b.id);
        // reopen the prerequisite: dependent b must NOT be rewound
        apply_action(r, a.id, TaskAction::Reopen, "human", Some("regression")).unwrap();
        let b_after = tasks::load_task(r, b.id).unwrap().unwrap();
        assert_eq!(b_after.status, TaskStatus::Closed);
        // doctor surfaces the inconsistency
        let f = crate::diag::inspect(r);
        assert!(
            f.iter().any(|x| x.code == "task.dependency_inconsistency"),
            "{f:?}"
        );
    }
}
