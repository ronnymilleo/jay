//! Project diagnostics (doctor) and explicit repair.
//!
//! Every config, task, milestone and KB file is inspected independently:
//! a malformed file never hides findings in later files. Findings carry a
//! file, optional field, severity, a stable code and a suggested action.
//! Repair is explicit (dry run by default), preserves unknown/user data by
//! operating on raw TOML values, backs up before applying and is idempotent.
//!
//! Policy: legacy naive timestamps (`YYYY-MM-DDTHH:MM:SS`) are retained and
//! never compared against offset-bearing ones; empty problems/ideas/decisions
//! arrays are valid; missing completion evidence is never fabricated.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{KnowledgeEntry, KnowledgeKind, Milestone, Task};
use crate::project::{self, GitIntegration};

/// Known (current-version) fields of each persisted record. Unknown keys are
/// reported but preserved — they may come from a newer jay version.
pub const KNOWN_TASK_FIELDS: &[&str] = &[
    "id",
    "title",
    "description",
    "status",
    "blocked",
    "block_reason",
    "acceptance",
    "context",
    "assignee",
    "priority",
    "labels",
    "estimate_points",
    "estimate_hours",
    "deadline",
    "links",
    "parent_id",
    "epic_id",
    "milestone_id",
    "depends_on",
    "report_result",
    "report_problems",
    "report_ideas",
    "report_decisions",
    "report_validation",
    "created_at",
    "started_at",
    "done_at",
    "actor",
    "updated_at",
    "follow_up_of",
    "git_diagnostics",
    "history",
];

/// Legacy task fields recognized for explicit repair (not "unknown").
pub const LEGACY_TASK_FIELDS: &[&str] = &["closed_at"];

pub const KNOWN_CONFIG_FIELDS: &[&str] = &[
    "name",
    "description",
    "goal",
    "git_repo",
    "branch_template",
    "git_integration",
    "links",
];

pub const KNOWN_KB_ENTRY_FIELDS: &[&str] = &[
    "id",
    "kind",
    "title",
    "content",
    "tags",
    "related_task",
    "supersedes",
    "commit",
    "actor",
    "created_at",
    "updated_at",
];

pub const KNOWN_MILESTONE_FIELDS: &[&str] = &["id", "name", "target_date", "done_at", "created_at"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One diagnostic finding.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Path relative to the project root (e.g. `.nest/tasks/17.toml`).
    pub file: String,
    /// Field the finding refers to, when applicable.
    pub field: Option<String>,
    pub severity: Severity,
    /// Stable machine-readable code (e.g. `task.closed_missing_done_at`).
    pub code: &'static str,
    pub message: String,
    /// Suggested action.
    pub suggestion: String,
}

impl Finding {
    fn error(file: &str, code: &'static str, message: String, suggestion: &str) -> Self {
        Finding {
            file: file.to_string(),
            field: None,
            severity: Severity::Error,
            code,
            message,
            suggestion: suggestion.to_string(),
        }
    }

    fn warning(file: &str, code: &'static str, message: String, suggestion: &str) -> Self {
        Finding {
            file: file.to_string(),
            field: None,
            severity: Severity::Warning,
            code,
            message,
            suggestion: suggestion.to_string(),
        }
    }

    fn with_field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// ===== timestamps =====

/// A parsed timestamp: either legacy naive-local or RFC 3339 with offset.
#[derive(Debug, Clone)]
pub enum Timestamp {
    Naive(chrono::NaiveDateTime),
    Offset(chrono::DateTime<chrono::FixedOffset>),
}

/// Parses a persisted timestamp. Accepts legacy naive local
/// (`YYYY-MM-DDTHH:MM:SS`) and RFC 3339 with offset. Never invents offsets.
pub fn parse_ts(s: &str) -> Option<Timestamp> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(Timestamp::Offset(dt));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(Timestamp::Naive(dt));
    }
    None
}

/// Compares two timestamps only when they are comparable (same offset
/// awareness). Legacy naive and new offset-bearing timestamps are never
/// compared as if they were UTC.
pub fn cmp_ts(a: &Timestamp, b: &Timestamp) -> Option<Ordering> {
    match (a, b) {
        (Timestamp::Naive(x), Timestamp::Naive(y)) => Some(x.cmp(y)),
        (Timestamp::Offset(x), Timestamp::Offset(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn check_timestamp(
    findings: &mut Vec<Finding>,
    file: &str,
    field: &str,
    value: &str,
) -> Option<Timestamp> {
    match parse_ts(value) {
        Some(ts) => Some(ts),
        None => {
            findings.push(
                Finding::error(
                    file,
                    "value.invalid_timestamp",
                    format!("{field} = \"{value}\" is not a valid timestamp"),
                    "fix the value to RFC 3339 (with offset) or the legacy YYYY-MM-DDTHH:MM:SS form",
                )
                .with_field(field),
            );
            None
        }
    }
}

// ===== inspection =====

struct TaskFile {
    rel: String,
    task: Option<Task>,
}

/// Inspects the whole project. Never fails early: unreadable or malformed
/// files become findings and inspection continues.
pub fn inspect(root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    inspect_layout_and_config(root, &mut findings);
    let milestones = inspect_milestones(root, &mut findings);
    let task_files = inspect_tasks(root, &mut findings);
    cross_check_tasks(&task_files, &milestones, &mut findings);
    inspect_kb(root, &task_files, &mut findings);
    findings
}

fn inspect_layout_and_config(root: &Path, findings: &mut Vec<Finding>) {
    let nest = root.join(project::NEST_DIR);
    if !nest.join(project::CONFIG_FILE).is_file() {
        findings.push(Finding::error(
            &rel(root, &nest.join(project::CONFIG_FILE)),
            "project.missing_config",
            "missing .nest/config.toml".into(),
            "re-run jay init in a fresh folder or restore the config",
        ));
    }
    if !nest.join(project::TASKS_DIR).is_dir() {
        findings.push(Finding::error(
            &rel(root, &nest.join(project::TASKS_DIR)),
            "project.missing_tasks_dir",
            "missing .nest/tasks/ directory".into(),
            "create the directory (jay init does this automatically)",
        ));
    }
    let cfg_file = rel(root, &nest.join(project::CONFIG_FILE));
    let cfg = match project::load_config(root) {
        Ok(cfg) => cfg,
        Err(e) => {
            findings.push(Finding::error(
                &cfg_file,
                "config.parse_error",
                format!("config.toml invalid: {e}"),
                "fix the TOML syntax or restore the file",
            ));
            return;
        }
    };
    if let Ok(text) = std::fs::read_to_string(nest.join(project::CONFIG_FILE)) {
        if let Ok(value) = text.parse::<toml::Value>() {
            report_unknown_fields(
                findings,
                &cfg_file,
                &value,
                KNOWN_CONFIG_FIELDS,
                &[],
                "config",
            );
        }
    }
    if cfg.effective_git_integration() == GitIntegration::Auto && !root.join(".git").exists() {
        findings.push(Finding::warning(
            &cfg_file,
            "project.git_missing",
            "git_integration is 'auto' but the folder is not a git repository".into(),
            "run git init, or set git_integration = \"off\" to silence",
        ));
    }
}

fn inspect_milestones(root: &Path, findings: &mut Vec<Finding>) -> Vec<Milestone> {
    let path = root.join(project::NEST_DIR).join(project::MILESTONES_FILE);
    let f = rel(root, &path);
    if !path.is_file() {
        return Vec::new();
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            findings.push(Finding::error(
                &f,
                "milestones.read_error",
                format!("cannot read milestones.toml: {e}"),
                "restore the file",
            ));
            return Vec::new();
        }
    };
    if let Ok(value) = text.parse::<toml::Value>() {
        if let Some(arr) = value.get("milestones").and_then(|m| m.as_array()) {
            for (i, m) in arr.iter().enumerate() {
                report_unknown_fields(
                    findings,
                    &f,
                    m,
                    KNOWN_MILESTONE_FIELDS,
                    &[],
                    &format!("milestones[{i}]"),
                );
            }
        }
    }
    #[derive(serde::Deserialize)]
    struct MilestonesFile {
        #[serde(default)]
        milestones: Vec<Milestone>,
    }
    match toml::from_str::<MilestonesFile>(&text) {
        Ok(mf) => {
            let mut seen = BTreeSet::new();
            for m in &mf.milestones {
                if !seen.insert(m.id) {
                    findings.push(
                        Finding::error(
                            &f,
                            "milestone.duplicate_id",
                            format!("duplicate milestone id {}", m.id),
                            "renumber one of the milestones",
                        )
                        .with_field("id"),
                    );
                }
            }
            mf.milestones
        }
        Err(e) => {
            findings.push(Finding::error(
                &f,
                "milestones.parse_error",
                format!("milestones.toml invalid: {e}"),
                "fix the TOML syntax or restore the file",
            ));
            Vec::new()
        }
    }
}

fn inspect_tasks(root: &Path, findings: &mut Vec<Finding>) -> Vec<TaskFile> {
    let dir = root.join(project::NEST_DIR).join(project::TASKS_DIR);
    let mut files: Vec<TaskFile> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return files,
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let f = rel(root, &path);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file_id: Option<i64> = stem.parse().ok();
        if file_id.is_none() {
            findings.push(Finding::warning(
                &f,
                "task.nonnumeric_filename",
                format!("task file name '{stem}.toml' is not a numeric id"),
                "rename the file to <id>.toml or remove it",
            ));
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                findings.push(Finding::error(
                    &f,
                    "task.read_error",
                    format!("cannot read task file: {e}"),
                    "restore the file",
                ));
                files.push(TaskFile { rel: f, task: None });
                continue;
            }
        };
        let value: Option<toml::Value> = match text.parse::<toml::Value>() {
            Ok(v) => Some(v),
            Err(e) => {
                findings.push(Finding::error(
                    &f,
                    "task.parse_error",
                    format!("malformed TOML: {e}"),
                    "fix the TOML syntax by hand; doctor will not modify malformed files",
                ));
                files.push(TaskFile { rel: f, task: None });
                continue;
            }
        };
        let task: Option<Task> = match value.as_ref().map(|_| toml::from_str::<Task>(&text)) {
            Some(Ok(t)) => Some(t),
            Some(Err(e)) => {
                findings.push(Finding::error(
                    &f,
                    "task.invalid_value",
                    format!("task record invalid: {e}"),
                    "fix the offending field value (see message)",
                ));
                None
            }
            None => None,
        };
        // unknown / legacy fields (value-level, works even when Task parsing failed)
        if let Some(v) = &value {
            report_unknown_fields(
                findings,
                &f,
                v,
                KNOWN_TASK_FIELDS,
                LEGACY_TASK_FIELDS,
                "task",
            );
            inspect_legacy_closed_at(findings, &f, v.get("done_at"), v.get("closed_at"));
        }
        if let Some(t) = &task {
            inspect_task_record(findings, &f, file_id, t);
        }
        files.push(TaskFile { rel: f, task });
    }
    files
}

fn inspect_task_record(findings: &mut Vec<Finding>, f: &str, file_id: Option<i64>, t: &Task) {
    if let Some(fid) = file_id {
        if fid != t.id {
            findings.push(
                Finding::error(
                    f,
                    "task.filename_id_mismatch",
                    format!("file name says id {fid} but the record says id {}", t.id),
                    "rename the file to <id>.toml or fix the id field",
                )
                .with_field("id"),
            );
        }
    }
    // timestamps: syntax then ordering (only between comparable values)
    let created = check_timestamp(findings, f, "created_at", &t.created_at);
    let updated = check_timestamp(findings, f, "updated_at", &t.updated_at);
    let started = t
        .started_at
        .as_deref()
        .and_then(|s| check_timestamp(findings, f, "started_at", s));
    let done = t
        .done_at
        .as_deref()
        .and_then(|s| check_timestamp(findings, f, "done_at", s));
    if let (Some(a), Some(b)) = (&created, &started) {
        if matches!(cmp_ts(a, b), Some(Ordering::Greater)) {
            findings.push(timestamp_order(f, "started_at", "created_at"));
        }
    }
    if let (Some(a), Some(b)) = (&started, &done) {
        if matches!(cmp_ts(a, b), Some(Ordering::Greater)) {
            findings.push(timestamp_order(f, "done_at", "started_at"));
        }
    }
    if let (Some(a), Some(b)) = (&created, &done) {
        if matches!(cmp_ts(a, b), Some(Ordering::Greater)) {
            findings.push(timestamp_order(f, "done_at", "created_at"));
        }
    }
    if let (Some(a), Some(b)) = &(&created, &updated) {
        if matches!(cmp_ts(a, b), Some(Ordering::Greater)) {
            findings.push(timestamp_order(f, "updated_at", "created_at"));
        }
    }
    // lifecycle history (existing files default to empty; never invented)
    let mut prev: Option<crate::diag::Timestamp> = None;
    for (i, ev) in t.history.iter().enumerate() {
        match check_timestamp(findings, f, &format!("history[{i}].at"), &ev.at) {
            Some(ts) => {
                if let Some(p) = &prev {
                    if matches!(cmp_ts(p, &ts), Some(Ordering::Greater)) {
                        findings.push(
                            Finding::error(
                                f,
                                "value.timestamp_order",
                                format!("history[{i}].at is before history[{}].at", i - 1),
                                "fix the recorded event timestamps",
                            )
                            .with_field(&format!("history[{i}].at")),
                        );
                    }
                }
                prev = Some(ts);
            }
            None => prev = None,
        }
        if let Some(pd) = &ev.prior_done_at {
            check_timestamp(findings, f, &format!("history[{i}].prior_done_at"), pd);
        }
    }
    // closed records need completion evidence (empty problems/ideas/decisions are valid)
    if t.status == crate::model::TaskStatus::Closed {
        let blank = |o: &Option<String>| o.as_deref().unwrap_or("").trim().is_empty();
        if t.done_at.is_none() {
            findings.push(
                Finding::error(
                    f,
                    "task.closed_missing_done_at",
                    "closed task has no done_at timestamp".into(),
                    "set done_at (jay repair can migrate a legacy closed_at) or record the real completion time",
                )
                .with_field("done_at"),
            );
        }
        if blank(&t.report_result) {
            findings.push(
                Finding::error(
                    f,
                    "task.closed_missing_result",
                    "closed task has no nonblank report_result".into(),
                    "update the task with the real result — jay never fabricates completion evidence",
                )
                .with_field("report_result"),
            );
        }
        if blank(&t.report_validation) {
            findings.push(
                Finding::error(
                    f,
                    "task.closed_missing_validation",
                    "closed task has no nonblank report_validation".into(),
                    "update the task with the real validation — jay never fabricates completion evidence",
                )
                .with_field("report_validation"),
            );
        }
    }
}

fn inspect_legacy_closed_at(
    findings: &mut Vec<Finding>,
    f: &str,
    done_at: Option<&toml::Value>,
    closed_at: Option<&toml::Value>,
) {
    let Some(cv) = closed_at else { return };
    let closed_str = cv.as_str().unwrap_or_default().to_string();
    if parse_ts(&closed_str).is_none() {
        findings.push(
            Finding::error(
                f,
                "value.invalid_timestamp",
                format!("legacy closed_at = \"{closed_str}\" is not a valid timestamp"),
                "fix or remove the legacy closed_at field",
            )
            .with_field("closed_at"),
        );
        return;
    }
    match done_at.and_then(|d| d.as_str()) {
        None => findings.push(
            Finding::warning(
                f,
                "task.legacy_closed_at",
                format!("legacy field closed_at = \"{closed_str}\" used instead of done_at"),
                "run `jay repair` to migrate the value to done_at (dry run by default; --apply to write)",
            )
            .with_field("closed_at"),
        ),
        Some(d) => {
            let equivalent = d == closed_str
                || match (parse_ts(d), parse_ts(&closed_str)) {
                    (Some(a), Some(b)) => cmp_ts(&a, &b) == Some(Ordering::Equal),
                    _ => false,
                };
            if equivalent {
                findings.push(
                    Finding::warning(
                        f,
                        "task.legacy_closed_at",
                        "legacy field closed_at duplicates done_at".into(),
                        "run `jay repair --apply` to drop the redundant legacy field",
                    )
                    .with_field("closed_at"),
                );
            } else {
                findings.push(
                    Finding::error(
                        f,
                        "task.closed_at_conflict",
                        format!("legacy closed_at = \"{closed_str}\" conflicts with done_at = \"{d}\""),
                        "resolve by hand: decide which timestamp is authoritative, then remove closed_at",
                    )
                    .with_field("closed_at"),
                );
            }
        }
    }
}

fn timestamp_order(f: &str, later: &str, earlier: &str) -> Finding {
    Finding::error(
        f,
        "value.timestamp_order",
        format!("{later} is before {earlier}"),
        "fix the timestamps (legacy naive and new offset-bearing values are never compared)",
    )
    .with_field(later)
}

fn cross_check_tasks(files: &[TaskFile], milestones: &[Milestone], findings: &mut Vec<Finding>) {
    let ids: BTreeSet<i64> = files
        .iter()
        .filter_map(|tf| tf.task.as_ref().map(|t| t.id))
        .collect();
    let all_tasks: Vec<Task> = files.iter().filter_map(|tf| tf.task.clone()).collect();
    let milestone_ids: BTreeSet<i64> = milestones.iter().map(|m| m.id).collect();
    for tf in files {
        let Some(t) = &tf.task else { continue };
        for (field, target) in [
            ("parent_id", t.parent_id),
            ("epic_id", t.epic_id),
            ("follow_up_of", t.follow_up_of),
        ] {
            if let Some(id) = target {
                if !ids.contains(&id) {
                    findings.push(
                        Finding::error(
                            &tf.rel,
                            "task.dangling_ref",
                            format!("{field} = {id} does not reference an existing task"),
                            "fix or clear the reference",
                        )
                        .with_field(field),
                    );
                }
            }
        }
        if let Some(id) = t.milestone_id {
            if !milestone_ids.contains(&id) {
                findings.push(
                    Finding::error(
                        &tf.rel,
                        "task.dangling_ref",
                        format!("milestone_id = {id} does not reference an existing milestone"),
                        "fix or clear the reference, or add the milestone",
                    )
                    .with_field("milestone_id"),
                );
            }
        }
        // structured dependencies (hand-edited data)
        let mut seen = std::collections::BTreeSet::new();
        for d in &t.depends_on {
            if *d == t.id {
                findings.push(
                    Finding::error(
                        &tf.rel,
                        "task.depends_on_self",
                        format!("task {} depends on itself", t.id),
                        "remove the self-dependency",
                    )
                    .with_field("depends_on"),
                );
            } else if !seen.insert(*d) {
                findings.push(
                    Finding::error(
                        &tf.rel,
                        "task.depends_on_duplicate",
                        format!("duplicate dependency #{d} on task {}", t.id),
                        "remove the duplicate id",
                    )
                    .with_field("depends_on"),
                );
            } else if !ids.contains(d) {
                findings.push(
                    Finding::error(
                        &tf.rel,
                        "task.depends_on_missing",
                        format!("task {} depends on missing task #{d}", t.id),
                        "remove the edge or create/restore the prerequisite",
                    )
                    .with_field("depends_on"),
                );
            }
        }
        // reopened/cancelled prerequisite while this task is active or done
        if matches!(
            t.status,
            crate::model::TaskStatus::Started
                | crate::model::TaskStatus::Review
                | crate::model::TaskStatus::Closed
        ) {
            for d in &t.depends_on {
                if let Some(dep) = all_tasks.iter().find(|o| o.id == *d) {
                    if dep.status != crate::model::TaskStatus::Closed {
                        findings.push(
                            Finding::warning(
                                &tf.rel,
                                "task.dependency_inconsistency",
                                format!(
                                    "task {} is {} while prerequisite #{d} is {} (e.g. reopened or cancelled); jay never rewinds dependents automatically",
                                    t.id,
                                    t.status.as_str(),
                                    dep.status.as_str()
                                ),
                                "review the dependent task: re-verify it against the reopened prerequisite, or cancel/rewind it explicitly",
                            )
                            .with_field("depends_on"),
                        );
                    }
                }
            }
        }
    }
    // dependency cycles across the project (the first cycle found is reported
    // on each involved task; removing any edge breaks it and doctor re-runs)
    if let Some(cycle) = dependency_cycles(&all_tasks).first() {
        let desc = cycle
            .iter()
            .map(|i| format!("#{i}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        for id in cycle {
            if let Some(tf) = files
                .iter()
                .find(|tf| tf.task.as_ref().map(|t| t.id == *id).unwrap_or(false))
            {
                findings.push(
                    Finding::error(
                        &tf.rel,
                        "task.dependency_cycle",
                        format!("dependency cycle: {desc}"),
                        "remove one of the edges in the cycle",
                    )
                    .with_field("depends_on"),
                );
            }
        }
    }
}

/// Finds one dependency cycle (if any) and returns it as id -> ... -> id.
pub fn dependency_cycles(tasks: &[Task]) -> Vec<Vec<i64>> {
    let mut cycles = Vec::new();
    let mut visited = BTreeSet::new();
    for t in tasks {
        let mut path = Vec::new();
        if let Some(c) = find_cycle_dfs(t.id, tasks, &mut visited, &mut path) {
            cycles.push(c);
        }
    }
    cycles
}

fn find_cycle_dfs(
    id: i64,
    tasks: &[Task],
    visited: &mut BTreeSet<i64>,
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
    if let Some(t) = tasks.iter().find(|t| t.id == id) {
        for d in &t.depends_on {
            if let Some(c) = find_cycle_dfs(*d, tasks, visited, path) {
                return Some(c);
            }
        }
    }
    path.pop();
    None
}

fn inspect_kb(root: &Path, task_files: &[TaskFile], findings: &mut Vec<Finding>) {
    let kb = root.join(project::NEST_DIR).join(project::KB_DIR);
    let task_ids: BTreeSet<i64> = task_files
        .iter()
        .filter_map(|tf| tf.task.as_ref().map(|t| t.id))
        .collect();
    for kind in KnowledgeKind::ALL {
        let path = kb.join(kind.filename());
        let f = rel(root, &path);
        if !path.is_file() {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                findings.push(Finding::error(
                    &f,
                    "kb.read_error",
                    format!("cannot read {}: {e}", kind.filename()),
                    "restore the file",
                ));
                continue;
            }
        };
        if let Ok(value) = text.parse::<toml::Value>() {
            if let Some(arr) = value.get("entries").and_then(|m| m.as_array()) {
                for (i, e) in arr.iter().enumerate() {
                    report_unknown_fields(
                        findings,
                        &f,
                        e,
                        KNOWN_KB_ENTRY_FIELDS,
                        &[],
                        &format!("entries[{i}]"),
                    );
                }
            }
        }
        #[derive(serde::Deserialize)]
        struct EntriesFile {
            #[serde(default)]
            current_summary: Option<crate::knowledge::CurrentSummary>,
            #[serde(default)]
            entries: Vec<KnowledgeEntry>,
        }
        match toml::from_str::<EntriesFile>(&text) {
            Ok(ef) => {
                // current-summary pointer (status file)
                if let Some(cs) = &ef.current_summary {
                    if !ef.entries.iter().any(|e| e.id == cs.entry_id) {
                        findings.push(
                            Finding::error(
                                &f,
                                "kb.current_summary_missing",
                                format!(
                                    "current_summary points to missing status entry #{}",
                                    cs.entry_id
                                ),
                                "designate an existing entry (jay kb set-current <id>) or remove the pointer",
                            )
                            .with_field("current_summary"),
                        );
                    }
                }
                // supersession references
                for e in &ef.entries {
                    for s in &e.supersedes {
                        if *s == e.id {
                            findings.push(
                                Finding::error(
                                    &f,
                                    "kb.supersedes_self",
                                    format!("entry #{} supersedes itself", e.id),
                                    "remove the self-reference",
                                )
                                .with_field("supersedes"),
                            );
                        } else if !ef.entries.iter().any(|o| o.id == *s) {
                            findings.push(
                                Finding::error(
                                    &f,
                                    "kb.supersedes_missing",
                                    format!(
                                        "{} entry #{} supersedes missing entry #{s}",
                                        kind.as_str(),
                                        e.id
                                    ),
                                    "fix or remove the supersession reference",
                                )
                                .with_field("supersedes"),
                            );
                        }
                    }
                }
                if crate::knowledge::supersession_cycle(&ef.entries).is_some() {
                    findings.push(Finding::error(
                        &f,
                        "kb.supersedes_cycle",
                        "supersession references contain a cycle".into(),
                        "remove one of the supersedes edges in the cycle",
                    ));
                }
                // legacy 'current' tags contradicting the explicit designation
                if kind == KnowledgeKind::Status {
                    let tagged: Vec<&KnowledgeEntry> = ef
                        .entries
                        .iter()
                        .filter(|e| e.tags.iter().any(|t| t.eq_ignore_ascii_case("current")))
                        .collect();
                    for e in &tagged {
                        let contradicts = match &ef.current_summary {
                            Some(cs) => cs.entry_id != e.id,
                            None => ef
                                .entries
                                .iter()
                                .map(|x| x.id)
                                .max()
                                .map(|max| max != e.id)
                                .unwrap_or(false),
                        };
                        if contradicts {
                            findings.push(
                                Finding::warning(
                                    &f,
                                    "kb.legacy_current_tag",
                                    format!(
                                        "status entry #{} ({}) is tagged 'current' but is not the authoritative summary; tags never promote entries automatically",
                                        e.id, e.title
                                    ),
                                    "if this entry is outdated, record a new status entry and designate it with `jay kb set-current`; do not rely on tags",
                                )
                                .with_field("tags"),
                            );
                        }
                    }
                }
                let mut seen = BTreeSet::new();
                for e in &ef.entries {
                    if !seen.insert(e.id) {
                        findings.push(
                            Finding::error(
                                &f,
                                "kb.duplicate_id",
                                format!("duplicate {} entry id {}", kind.as_str(), e.id),
                                "renumber one of the entries",
                            )
                            .with_field("id"),
                        );
                    }
                    for ts_field in ["created_at", "updated_at"] {
                        let v = if ts_field == "created_at" {
                            &e.created_at
                        } else {
                            &e.updated_at
                        };
                        check_timestamp(findings, &f, ts_field, v);
                    }
                    if let Some(rt) = e.related_task {
                        if !task_ids.is_empty() && !task_ids.contains(&rt) {
                            findings.push(
                                Finding::warning(
                                    &f,
                                    "kb.related_task_missing",
                                    format!(
                                        "{} entry {} references missing task #{rt}",
                                        kind.as_str(),
                                        e.id
                                    ),
                                    "fix or clear related_task (the task may have been moved or deleted)",
                                )
                                .with_field("related_task"),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                findings.push(Finding::error(
                    &f,
                    "kb.parse_error",
                    format!("{} invalid: {e}", kind.filename()),
                    "fix the TOML syntax or restore the file",
                ));
            }
        }
    }
}

fn report_unknown_fields(
    findings: &mut Vec<Finding>,
    file: &str,
    value: &toml::Value,
    known: &[&str],
    legacy: &[&str],
    what: &str,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    for key in table.keys() {
        if known.contains(&key.as_str()) || legacy.contains(&key.as_str()) {
            continue;
        }
        findings.push(
            Finding::warning(
                file,
                "value.unknown_field",
                format!("unknown {what} field '{key}' (possibly from a newer jay version)"),
                "keep it (jay preserves unknown fields on repair) or remove it by hand; consider upgrading jay",
            )
            .with_field(key),
        );
    }
}

// ===== repair =====

/// One proposed repair operation.
#[derive(Debug, Clone, Serialize)]
pub struct RepairOp {
    pub file: String,
    pub code: &'static str,
    pub description: String,
}

/// Result of a repair run (dry run or apply).
#[derive(Debug, Clone, Serialize)]
pub struct RepairReport {
    pub applied: bool,
    pub proposed: Vec<RepairOp>,
    pub backups: Vec<String>,
    /// Error findings that repair cannot fix (they need explicit user/agent
    /// updates — jay never fabricates missing evidence).
    pub unresolved: Vec<Finding>,
}

/// Plans (and optionally applies) all safe, explicit repairs.
///
/// Currently supported: migrate a valid legacy `closed_at` to `done_at` when
/// `done_at` is absent or equivalent. Conflicting values remain an actionable
/// error and are never touched. Malformed files are never rewritten. Repair
/// preserves every other field, including unknown ones, and is idempotent.
pub fn repair(root: &Path, apply: bool) -> anyhow::Result<RepairReport> {
    let _lock = if apply {
        Some(crate::service::ProjectLock::acquire(root)?)
    } else {
        None
    };
    let findings = inspect(root);
    let mut proposed = Vec::new();
    let mut targets: Vec<(PathBuf, String)> = Vec::new();
    for f in &findings {
        if f.code == "task.legacy_closed_at" {
            proposed.push(RepairOp {
                file: f.file.clone(),
                code: f.code,
                description: "migrate legacy closed_at to done_at and drop the legacy field"
                    .to_string(),
            });
            targets.push((root.join(&f.file), f.file.clone()));
        }
    }
    targets.sort();
    targets.dedup_by(|a, b| a.0 == b.0);

    let mut backups = Vec::new();
    if apply {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        for (path, relpath) in &targets {
            let text = std::fs::read_to_string(path)?;
            let Some(mut value) = text.parse::<toml::Value>().ok() else {
                continue; // never rewrite malformed files
            };
            let Some(table) = value.as_table_mut() else {
                continue;
            };
            let Some(closed) = table.remove("closed_at") else {
                continue;
            };
            if table.get("done_at").is_none() {
                table.insert("done_at".to_string(), closed);
            }
            let new_text = toml::to_string_pretty(&value)?;
            let backup = root
                .join(project::NEST_DIR)
                .join("backups")
                .join(format!("repair-{stamp}"))
                .join(relpath);
            if let Some(dir) = backup.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&backup, &text)?;
            project::atomic_write(path, &new_text)?;
            backups.push(rel(root, &backup));
        }
    }

    let remaining = if apply { inspect(root) } else { findings };
    let unresolved = remaining
        .into_iter()
        .filter(|f| f.severity == Severity::Error && f.code != "task.legacy_closed_at")
        .collect();
    Ok(RepairReport {
        applied: apply,
        proposed,
        backups,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::now_ts;
    use crate::project::init_project;
    use crate::tasks;

    fn proj() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        init_project(base.path(), None).unwrap();
        base
    }

    fn write_task_file(root: &Path, name: &str, text: &str) {
        let dir = root.join(project::NEST_DIR).join(project::TASKS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn clean_project_has_no_findings() {
        let root = proj();
        let t = Task::new(1, "open idea".into(), "someday".into());
        tasks::save_task(root.path(), &t).unwrap();
        let f = inspect(root.path());
        assert!(f.is_empty(), "unexpected findings: {f:?}");
    }

    #[test]
    fn ordinary_future_task_is_valid() {
        // psotool #2 style: open, empty acceptance, no completion evidence
        let root = proj();
        write_task_file(
            root.path(),
            "2.toml",
            r#"id = 2
title = "Compare configurations across many seeds"
description = "Future work."
status = "open"
blocked = false
acceptance = []
context = ""
priority = "medium"
labels = ["future"]
links = []
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-11T01:28:29"
actor = "human"
updated_at = "2026-09-11T01:28:29"
"#,
        );
        let f = inspect(root.path());
        assert!(f.is_empty(), "unexpected findings: {f:?}");
    }

    #[test]
    fn siggen_style_malformed_closure_produces_all_findings() {
        // siggen #17 style: closed with closed_at instead of done_at and
        // missing result/validation
        let root = proj();
        write_task_file(
            root.path(),
            "17.toml",
            r#"id = 17
title = "Model waveform families"
description = "d"
status = "closed"
blocked = false
acceptance = ["a"]
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-12T15:08:43"
actor = "agent"
updated_at = "2026-09-13T18:45:00"
closed_at = "2026-09-13T18:45:00Z"
"#,
        );
        let f = inspect(root.path());
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.legacy_closed_at"), "{codes:?}");
        assert!(codes.contains(&"task.closed_missing_done_at"), "{codes:?}");
        assert!(codes.contains(&"task.closed_missing_result"), "{codes:?}");
        assert!(
            codes.contains(&"task.closed_missing_validation"),
            "{codes:?}"
        );
        // unknown-field noise must NOT include the recognized legacy field
        assert!(!f
            .iter()
            .any(|x| x.code == "value.unknown_field" && x.field.as_deref() == Some("closed_at")));
    }

    #[test]
    fn malformed_task_does_not_hide_later_files() {
        let root = proj();
        write_task_file(root.path(), "1.toml", "this is not toml {{{");
        let mut t = Task::new(2, "closed without evidence".into(), "".into());
        t.status = crate::model::TaskStatus::Closed;
        tasks::save_task(root.path(), &t).unwrap();
        let f = inspect(root.path());
        let files: BTreeSet<&str> = f.iter().map(|x| x.file.as_str()).collect();
        assert!(files.iter().any(|x| x.ends_with("1.toml")));
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.parse_error"));
        assert!(codes.contains(&"task.closed_missing_done_at"));
        assert!(codes.contains(&"task.closed_missing_result"));
    }

    #[test]
    fn repair_dry_run_changes_no_bytes_and_apply_migrates_with_backup() {
        let root = proj();
        let r = root.path();
        let text = r#"id = 17
title = "t"
description = "d"
status = "closed"
blocked = false
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = []
created_at = "2026-09-12T15:08:43"
actor = "agent"
updated_at = "2026-09-13T18:45:00"
closed_at = "2026-09-13T18:45:00Z"
future_field = "preserved"
"#;
        write_task_file(r, "17.toml", text);
        let before = std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap();

        // dry run: proposes, changes nothing
        let rep = repair(r, false).unwrap();
        assert_eq!(rep.proposed.len(), 1);
        assert!(!rep.applied);
        assert!(rep.backups.is_empty());
        let after = std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap();
        assert_eq!(before, after);

        // apply: migrates closed_at -> done_at, preserves unknown data, backs up
        let rep = repair(r, true).unwrap();
        assert_eq!(rep.backups.len(), 1);
        let after = std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap();
        assert!(after.contains("done_at = \"2026-09-13T18:45:00Z\""));
        assert!(!after.contains("closed_at"));
        assert!(after.contains("future_field = \"preserved\""));
        assert!(r.join(&rep.backups[0]).is_file());

        // idempotent: second run proposes nothing
        let rep2 = repair(r, true).unwrap();
        assert!(rep2.proposed.is_empty());

        // remaining unresolved findings still reported (missing result/validation)
        let codes: Vec<&str> = rep.unresolved.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.closed_missing_result"));
    }

    #[test]
    fn conflicting_closed_at_is_an_error_and_untouched() {
        let root = proj();
        let r = root.path();
        let text = r#"id = 1
title = "t"
description = "d"
status = "closed"
blocked = false
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = []
report_result = "r"
report_validation = "v"
created_at = "2026-09-12T15:08:43"
updated_at = "2026-09-13T18:45:00"
done_at = "2026-09-13T10:00:00Z"
closed_at = "2026-09-13T18:45:00Z"
"#;
        write_task_file(r, "1.toml", text);
        let f = inspect(r);
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.closed_at_conflict"), "{codes:?}");
        let rep = repair(r, true).unwrap();
        assert!(rep.proposed.is_empty());
        let after = std::fs::read_to_string(r.join(".nest/tasks/1.toml")).unwrap();
        assert_eq!(after, text);
    }

    #[test]
    fn equivalent_closed_at_is_dropped_on_repair() {
        let root = proj();
        let r = root.path();
        write_task_file(
            r,
            "1.toml",
            r#"id = 1
title = "t"
description = "d"
status = "closed"
blocked = false
acceptance = []
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = []
report_result = "r"
report_validation = "v"
created_at = "2026-09-12T15:08:43"
updated_at = "2026-09-13T18:45:00"
done_at = "2026-09-13T18:45:00"
closed_at = "2026-09-13T18:45:00"
"#,
        );
        let rep = repair(r, true).unwrap();
        assert_eq!(rep.proposed.len(), 1);
        let after = std::fs::read_to_string(r.join(".nest/tasks/1.toml")).unwrap();
        assert!(after.contains("done_at"));
        assert!(!after.contains("closed_at"));
        assert!(repair(r, true).unwrap().proposed.is_empty());
    }

    #[test]
    fn filename_id_mismatch_and_dangling_refs_detected() {
        let root = proj();
        let r = root.path();
        let mut t = Task::new(5, "wrong file".into(), "".into());
        t.parent_id = Some(99);
        t.milestone_id = Some(7);
        tasks::save_task(r, &t).unwrap();
        std::fs::rename(r.join(".nest/tasks/5.toml"), r.join(".nest/tasks/6.toml")).unwrap();
        let f = inspect(r);
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"task.filename_id_mismatch"), "{codes:?}");
        assert_eq!(
            codes.iter().filter(|c| **c == "task.dangling_ref").count(),
            2
        );
    }

    #[test]
    fn timestamp_validation_and_ordering() {
        assert!(parse_ts("2026-09-13T18:45:00Z").is_some());
        assert!(parse_ts("2026-09-13T18:45:00+02:00").is_some());
        assert!(parse_ts("2026-09-13T18:45:00").is_some());
        assert!(parse_ts("yesterday").is_none());
        // naive vs offset are never compared
        let a = parse_ts("2026-09-13T18:45:00").unwrap();
        let b = parse_ts("2026-09-13T18:45:00Z").unwrap();
        assert_eq!(cmp_ts(&a, &b), None);

        let root = proj();
        let mut t = Task::new(1, "t".into(), "".into());
        t.created_at = "2026-09-13T10:00:00".into();
        t.started_at = Some("2026-09-12T10:00:00".into());
        t.updated_at = "not-a-time".into();
        tasks::save_task(root.path(), &t).unwrap();
        let f = inspect(root.path());
        let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
        assert!(codes.contains(&"value.timestamp_order"), "{codes:?}");
        assert!(codes.contains(&"value.invalid_timestamp"), "{codes:?}");
    }

    #[test]
    fn unknown_fields_warned_not_errors() {
        let root = proj();
        let mut t = Task::new(1, "t".into(), "".into());
        t.updated_at = now_ts();
        tasks::save_task(root.path(), &t).unwrap();
        let p = root.path().join(".nest/tasks/1.toml");
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("schema_version = 99\n");
        std::fs::write(&p, text).unwrap();
        let f = inspect(root.path());
        let unk: Vec<&Finding> = f
            .iter()
            .filter(|x| x.code == "value.unknown_field")
            .collect();
        assert_eq!(unk.len(), 1);
        assert_eq!(unk[0].severity, Severity::Warning);
        // and the task still loads for diagnosis
        assert!(f.iter().all(|x| x.code != "task.parse_error"));
    }
}
