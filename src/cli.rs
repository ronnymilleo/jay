//! Quick terminal commands over the resolved project context.

use anyhow::{bail, Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use comfy_table::{presets, Cell, CellAlignment, Color, ContentArrangement, Table};

use crate::knowledge;
use crate::model::{KnowledgeEntry, KnowledgeKind, Priority, Task, TaskAction, TaskStatus};
use crate::project::{self, Context as JayContext, ProjectConfig};
use crate::tasks;

/// Global `--no-color` flag (set once at startup; a single-shot CLI).
static NO_COLOR: AtomicBool = AtomicBool::new(false);

/// Table border style for list/find commands.
#[derive(Clone, Copy, ValueEnum)]
enum BorderStyle {
    /// No lines (default).
    None,
    /// A single horizontal line under the header.
    Horizontal,
    /// Vertical lines between columns.
    Borders,
    /// Full grid (UTF-8 box).
    Full,
    /// Full grid in ASCII (`+---+`).
    Ascii,
}

impl BorderStyle {
    fn preset(self) -> &'static str {
        match self {
            BorderStyle::None => presets::NOTHING,
            BorderStyle::Horizontal => presets::UTF8_HORIZONTAL_ONLY,
            BorderStyle::Borders => presets::UTF8_BORDERS_ONLY,
            BorderStyle::Full => presets::UTF8_FULL,
            BorderStyle::Ascii => presets::ASCII_FULL,
        }
    }
}

/// Global `--borders` style (set once at startup).
static BORDER_STYLE: OnceLock<BorderStyle> = OnceLock::new();

fn border_preset() -> &'static str {
    BORDER_STYLE
        .get()
        .copied()
        .unwrap_or(BorderStyle::None)
        .preset()
}

#[derive(Parser)]
#[command(
    name = "jay",
    about = "Terminal project manager (git-like: one .nest per folder)"
)]
#[command(version)]
struct Cli {
    /// Disable ANSI colors.
    #[arg(long, global = true)]
    no_color: bool,

    /// Table border style for list/find output.
    #[arg(long, global = true, value_enum, default_value_t = BorderStyle::None)]
    borders: BorderStyle,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a project in the current folder.
    Init {
        /// Name (defaults to the folder name).
        name: Option<String>,
    },
    /// Print the resolved context (kind, root path, name).
    Current,
    /// Read or edit .nest/config.toml.
    Config {
        field: Option<String>,
        value: Option<String>,
        /// Show where each effective value comes from.
        #[arg(long)]
        show_origin: bool,
    },
    /// Project overview (tasks by status, active, blocked, ready work,
    /// current summary and freshness).
    #[command(alias = "st")]
    Status {
        /// Machine-readable combined project context (same payload as the
        /// MCP project_context tool).
        #[arg(long)]
        json: bool,
    },
    /// git pull + git push in the project folder.
    Sync,
    /// Validate the project (layout, config, task files, KB, repo).
    ///
    /// Exit codes: 0 = no integrity errors (warnings allowed), 1 = integrity
    /// errors found, 2 = invocation/runtime failure.
    Doctor {
        /// Emit findings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Explicit repair of known legacy data issues (dry run by default).
    ///
    /// Currently: migrate a valid legacy `closed_at` to `done_at`. Creates
    /// recoverable backups under .nest/backups/ when applying. Never
    /// fabricates missing completion evidence.
    Repair {
        /// Actually write the proposed changes (default: dry run).
        #[arg(long)]
        apply: bool,
    },
    /// Ready work: open, unblocked tasks whose dependencies are all closed
    /// (priority order, then id).
    Next {
        #[arg(long)]
        json: bool,
    },
    /// Task operations (new, start, review, done, ...).
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Project (workspace) operations.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Knowledge base operations (decisions, status, notes).
    Kb {
        #[command(subcommand)]
        cmd: KbCmd,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task (status: open).
    New {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long, short)]
        label: Vec<String>,
        #[arg(long)]
        estimate_points: Option<i64>,
        #[arg(long)]
        estimate_hours: Option<f64>,
        #[arg(long)]
        deadline: Option<String>,
        /// Task ids this task depends on (repeatable; project-local).
        #[arg(long)]
        depends_on: Vec<i64>,
    },
    /// List tasks.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a task's full detail.
    Show { id: i64 },
    /// Find tasks by a fuzzy title fragment.
    Find {
        term: String,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Start a task (open -> started; creates a branch).
    #[command(alias = "do")]
    Start { id: i64 },
    /// Send a task to review (started -> review; opens a PR).
    Review { id: i64 },
    /// Close a task (review -> closed).
    Done { id: i64 },
    /// Move a task one step back (review -> started -> open).
    Rewind { id: i64 },
    /// Cancel a task (any non-closed -> cancelled).
    Cancel { id: i64 },
    /// Reopen a cancelled or closed task (-> open). Reopening a closed task
    /// requires a nonblank reason; prior completion evidence is preserved in
    /// history and a new closure requires fresh result/validation.
    Reopen {
        id: i64,
        /// Why the task is reopened (mandatory for closed tasks).
        #[arg(long)]
        reason: Option<String>,
    },
    /// Create a follow-up task explicitly linked via follow_up_of. The
    /// original task is left unchanged (stays closed); no dependency is
    /// implied unless added separately.
    FollowUp {
        id: i64,
        /// Title of the new follow-up task (required).
        #[arg(long)]
        title: String,
        /// Description of the new follow-up task.
        #[arg(long)]
        description: Option<String>,
    },
    /// Block a task (sets the blocked flag).
    Block {
        id: i64,
        /// Why the task is blocked.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Unblock a task (clears the blocked flag).
    Unblock { id: i64 },
    /// Move a task to another project folder.
    Move {
        id: i64,
        #[arg(long)]
        to: String,
    },
    /// Partial edit via a JSON patch file (omitted fields unchanged; null
    /// clears nullable fields; empty lists clear lists; status/timestamps/
    /// report fields are protected). Use "-" to read the patch from stdin.
    Edit {
        id: i64,
        /// Path to a JSON patch object (or "-" for stdin).
        #[arg(long)]
        patch_file: String,
    },
    /// Typed report update: any subset of the five sections in one call.
    /// Use "-" to read the JSON report from stdin.
    Report {
        id: i64,
        /// Path to a JSON report object with result/validation/problems/
        /// ideas/decisions (or "-" for stdin).
        #[arg(long)]
        report_file: String,
        /// Append instead of replace (text separated by a blank line, lists
        /// appended in order).
        #[arg(long)]
        append: bool,
    },
    /// Save the report and close a task already in review, atomically.
    /// Requires nonblank result and validation; on failure nothing changes.
    Complete {
        id: i64,
        /// Path to a JSON report object (or "-" for stdin).
        #[arg(long)]
        report_file: String,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// List projects in a workspace.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum KbCmd {
    /// Show the current project summary (designated entry plus provenance
    /// and freshness facts, or the labeled highest-id fallback).
    Status,
    /// List knowledge base entries.
    #[command(alias = "ls")]
    List {
        /// Filter by kind: decision, status, or note.
        #[arg(long)]
        kind: Option<String>,
        /// Filter by tag (case-insensitive substring).
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show a single knowledge base entry.
    Show {
        id: i64,
        /// Entry kind: decision, status, or note.
        #[arg(long)]
        kind: String,
    },
    /// Add a knowledge base entry.
    Add {
        /// Entry kind: decision, status, or note.
        #[arg(long)]
        kind: String,
        /// Entry title.
        #[arg(long)]
        title: String,
        /// Entry content.
        #[arg(long)]
        content: String,
        /// Tags (repeatable).
        #[arg(long)]
        tag: Vec<String>,
        /// Related task id.
        #[arg(long)]
        related_task: Option<i64>,
        /// Status entries: ids this entry supersedes (repeatable; validated,
        /// superseded text is preserved).
        #[arg(long)]
        supersedes: Vec<i64>,
        /// Optional git commit reference recorded with the entry.
        #[arg(long)]
        commit: Option<String>,
        /// Status entries: also designate this entry as the current summary
        /// (one atomic file update).
        #[arg(long)]
        set_current: bool,
    },
    /// Designate an existing status entry as the authoritative current
    /// summary. Records actor and time; accepts an optional commit reference.
    /// Entries are never promoted automatically by a `current` tag.
    SetCurrent {
        id: i64,
        /// Optional git commit the summary describes.
        #[arg(long)]
        commit: Option<String>,
    },
    /// Update a knowledge base entry.
    Edit {
        id: i64,
        /// Entry kind: decision, status, or note.
        #[arg(long)]
        kind: String,
        /// New title.
        #[arg(long)]
        title: Option<String>,
        /// New content.
        #[arg(long)]
        content: Option<String>,
        /// Replace tags (repeatable).
        #[arg(long)]
        tag: Vec<String>,
        /// Related task id.
        #[arg(long)]
        related_task: Option<i64>,
        /// Replace supersedes references (repeatable; validated).
        #[arg(long)]
        supersedes: Vec<i64>,
    },
    /// Search entries by BM25 full-text ranking.
    #[command(alias = "s")]
    Search {
        /// Search query.
        query: String,
        /// Filter by kind: decision, status, or note.
        #[arg(long)]
        kind: Option<String>,
        /// Max results (default 10).
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
}

/// Runs the CLI. Returns the process exit code:
/// 0 = success (doctor: no integrity errors), 1 = doctor found integrity
/// errors. Invocation/runtime failures are returned as `Err` (the binary
/// exits 2 for those).
pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    NO_COLOR.store(cli.no_color, Ordering::Relaxed);
    let _ = BORDER_STYLE.set(cli.borders);
    match cli.command {
        Command::Init { name } => {
            let cwd = std::env::current_dir()?;
            project::init_project(&cwd, name.as_deref())?;
            println!("initialized project: {}", project::folder_name(&cwd));
        }
        Command::Current => cmd_current()?,
        Command::Config {
            field,
            value,
            show_origin,
        } => cmd_config(field, value, show_origin)?,
        Command::Status { json } => status_overview(json)?,
        Command::Next { json } => cmd_next(json)?,
        Command::Sync => cmd_sync()?,
        Command::Doctor { json } => return cmd_doctor(json),
        Command::Repair { apply } => cmd_repair(apply)?,
        Command::Task { cmd } => run_task(cmd)?,
        Command::Project { cmd } => run_project(cmd)?,
        Command::Kb { cmd } => run_kb(cmd)?,
    }
    Ok(0)
}

fn run_task(cmd: TaskCmd) -> Result<()> {
    match cmd {
        TaskCmd::New {
            title,
            description,
            priority,
            label,
            estimate_points,
            estimate_hours,
            deadline,
            depends_on,
        } => cmd_new(
            title,
            description,
            priority,
            label,
            estimate_points,
            estimate_hours,
            deadline,
            depends_on,
        ),
        TaskCmd::List { json } => {
            let root = require_project()?;
            ls_tasks(&root, json)
        }
        TaskCmd::Show { id } => cmd_show(id),
        TaskCmd::Find {
            term,
            priority,
            label,
            status,
        } => cmd_find(term, priority, label, status),
        TaskCmd::Start { id } => apply_action(id, TaskAction::Start, None),
        TaskCmd::Review { id } => apply_action(id, TaskAction::Review, None),
        TaskCmd::Done { id } => apply_action(id, TaskAction::Done, None),
        TaskCmd::Rewind { id } => apply_action(id, TaskAction::Rewind, None),
        TaskCmd::Cancel { id } => apply_action(id, TaskAction::Cancel, None),
        TaskCmd::Reopen { id, reason } => apply_action(id, TaskAction::Reopen, reason.as_deref()),
        TaskCmd::FollowUp {
            id,
            title,
            description,
        } => cmd_follow_up(id, title, description),
        TaskCmd::Block { id, reason } => apply_action(id, TaskAction::Block, reason.as_deref()),
        TaskCmd::Unblock { id } => apply_action(id, TaskAction::Unblock, None),
        TaskCmd::Move { id, to } => cmd_move(id, to),
        TaskCmd::Edit { id, patch_file } => cmd_task_edit(id, patch_file),
        TaskCmd::Report {
            id,
            report_file,
            append,
        } => cmd_task_report(id, report_file, append),
        TaskCmd::Complete { id, report_file } => cmd_task_complete(id, report_file),
    }
}

fn cmd_follow_up(id: i64, title: String, description: Option<String>) -> Result<()> {
    let root = require_project()?;
    let t = crate::service::create_follow_up(
        &root,
        id,
        title,
        description.unwrap_or_default(),
        "human",
    )?;
    println!("created follow-up task {} of #{}: {}", t.id, id, t.title);
    Ok(())
}

fn read_payload(path: &str) -> Result<String> {
    if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?)
    }
}

fn parse_report_payload(path: &str) -> Result<crate::service::ReportUpdate> {
    let text = read_payload(path)?;
    serde_json::from_str(&text).context(
        "report must be a JSON object with optional result/validation (strings) and problems/ideas/decisions (arrays of strings)",
    )
}

fn cmd_task_edit(id: i64, patch_file: String) -> Result<()> {
    let root = require_project()?;
    let text = read_payload(&patch_file)?;
    let patch = crate::service::parse_task_patch(&text)?;
    let t = crate::service::patch_task(&root, id, patch, "human")?;
    println!("patched task {}", t.id);
    Ok(())
}

fn cmd_task_report(id: i64, report_file: String, append: bool) -> Result<()> {
    let root = require_project()?;
    let upd = parse_report_payload(&report_file)?;
    let mode = if append {
        crate::service::ReportMode::Append
    } else {
        crate::service::ReportMode::Replace
    };
    let t = crate::service::update_report(&root, id, upd, mode, "human")?;
    println!(
        "report updated for task {} ({})",
        t.id,
        if append { "append" } else { "replace" }
    );
    Ok(())
}

fn cmd_task_complete(id: i64, report_file: String) -> Result<()> {
    let root = require_project()?;
    let upd = parse_report_payload(&report_file)?;
    let t = crate::service::complete_task(&root, id, upd, "human")?;
    println!(
        "task {} completed (report saved, status: {})",
        t.id,
        t.status.as_str()
    );
    Ok(())
}

fn run_project(cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::List { json } => cmd_project_list(json),
    }
}

fn run_kb(cmd: KbCmd) -> Result<()> {
    match cmd {
        KbCmd::Status => cmd_kb_status(),
        KbCmd::List { kind, tag, json } => cmd_kb_list(kind, tag, json),
        KbCmd::Show { id, kind } => cmd_kb_show(id, kind),
        KbCmd::Add {
            kind,
            title,
            content,
            tag,
            related_task,
            supersedes,
            commit,
            set_current,
        } => cmd_kb_add(
            kind,
            title,
            content,
            tag,
            related_task,
            supersedes,
            commit,
            set_current,
        ),
        KbCmd::SetCurrent { id, commit } => cmd_kb_set_current(id, commit),
        KbCmd::Edit {
            id,
            kind,
            title,
            content,
            tag,
            related_task,
            supersedes,
        } => cmd_kb_edit(id, kind, title, content, tag, related_task, supersedes),
        KbCmd::Search {
            query,
            kind,
            limit,
            json,
        } => cmd_kb_search(query, kind, limit, json),
    }
}

fn cmd_kb_search(
    query: String,
    kind: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    let root = require_project()?;
    let k = match kind.as_deref() {
        Some(s) => Some(s.parse::<KnowledgeKind>()?),
        None => None,
    };
    let limit = limit.unwrap_or(10);
    let results = knowledge::search(&root, &query, k)?;

    if json {
        let out: Vec<serde_json::Value> = results
            .iter()
            .take(limit)
            .map(|r| serde_json::json!({ "score": r.score, "entry": r.entry }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("(no matches)");
        return Ok(());
    }

    let mut table = Table::new();
    table
        .set_header(vec!["Score", "Kind", "Title", "Tags"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    for r in results.iter().take(limit) {
        table.add_row(vec![
            Cell::new(format!("{:.3}", r.score)),
            Cell::new(r.entry.kind.as_str()),
            Cell::new(r.entry.title.clone()),
            Cell::new(r.entry.tags.join(", ")),
        ]);
    }
    table
        .column_mut(0)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    println!("{}", table.trim_fmt());
    Ok(())
}

fn cmd_kb_status() -> Result<()> {
    let root = require_project()?;
    match knowledge::current_status(&root)? {
        Some(cs) => {
            if cs.designated {
                println!("== current summary (designated) ==");
            } else {
                println!("== {} ==", knowledge::FALLBACK_NOTE);
            }
            print_kb_entry(&cs.entry);
            if let Some(d) = &cs.designation {
                println!("designated by {} at {}", d.set_by, d.set_at);
                if let Some(c) = &d.commit {
                    println!("recorded commit: {c}");
                }
            }
            let fresh = knowledge::summary_freshness(&root, &cs)?;
            if fresh.possibly_stale {
                println!();
                println!("freshness (facts, not a verdict on the prose):");
                for fact in &fresh.facts {
                    println!("  - {fact}");
                }
            } else {
                println!(
                    "freshness: no task/KB updates postdate this summary{}",
                    if fresh.commit_matches_head == Some(true) {
                        "; commit matches local HEAD"
                    } else {
                        ""
                    }
                );
            }
        }
        None => println!("no status recorded"),
    }
    Ok(())
}

fn cmd_kb_set_current(id: i64, commit: Option<String>) -> Result<()> {
    let root = require_project()?;
    let cs = knowledge::set_current_summary(&root, id, "human", commit)?;
    println!(
        "current summary: status entry #{} (designated by {} at {})",
        cs.entry_id, cs.set_by, cs.set_at
    );
    Ok(())
}

fn cmd_kb_list(kind: Option<String>, tag: Option<String>, json: bool) -> Result<()> {
    let root = require_project()?;
    let k = match kind.as_deref() {
        Some(s) => Some(s.parse::<KnowledgeKind>()?),
        None => None,
    };
    let entries = knowledge::filter_entries(&root, k, tag.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no entries)");
        return Ok(());
    }
    let mut table = Table::new();
    table
        .set_header(vec!["ID", "Kind", "Title", "Tags"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    for e in &entries {
        table.add_row(vec![
            Cell::new(e.id.to_string()),
            Cell::new(e.kind.as_str()),
            Cell::new(e.title.clone()),
            Cell::new(e.tags.join(", ")),
        ]);
    }
    table
        .column_mut(0)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    println!("{}", table.trim_fmt());
    Ok(())
}

fn cmd_kb_show(id: i64, kind: String) -> Result<()> {
    let root = require_project()?;
    let k = kind.parse::<KnowledgeKind>()?;
    let entry = knowledge::get_entry(&root, k, id)?
        .ok_or_else(|| anyhow::anyhow!("knowledge entry {} ({}) not found", id, k.as_str()))?;
    print_kb_entry(&entry);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_kb_add(
    kind: String,
    title: String,
    content: String,
    tags: Vec<String>,
    related_task: Option<i64>,
    supersedes: Vec<i64>,
    commit: Option<String>,
    set_current: bool,
) -> Result<()> {
    let root = require_project()?;
    let k = kind.parse::<KnowledgeKind>()?;
    let mut entry = KnowledgeEntry::new(0, k, title, content);
    entry.tags = tags;
    entry.related_task = related_task;
    entry.supersedes = supersedes;
    entry.commit = commit;
    entry.actor = Some("human".to_string());
    if k == KnowledgeKind::Status {
        // entry creation + current designation in ONE atomic file update
        let (saved, cs) = knowledge::add_status_entry(&root, entry, set_current, "human")?;
        println!("added status #{}: {}", saved.id, saved.title);
        if let Some(cs) = cs {
            println!("designated as current summary (at {})", cs.set_at);
        }
        return Ok(());
    }
    if set_current {
        bail!("--set-current only applies to status entries");
    }
    let saved = knowledge::add_entry(&root, entry)?;
    println!(
        "added {} #{}: {}",
        saved.kind.as_str(),
        saved.id,
        saved.title
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_kb_edit(
    id: i64,
    kind: String,
    title: Option<String>,
    content: Option<String>,
    tags: Vec<String>,
    related_task: Option<i64>,
    supersedes: Vec<i64>,
) -> Result<()> {
    let root = require_project()?;
    let k = kind.parse::<KnowledgeKind>()?;
    let updated = knowledge::update_entry(&root, k, id, |entry| {
        if let Some(t) = title {
            entry.title = t;
        }
        if let Some(c) = content {
            entry.content = c;
        }
        if !tags.is_empty() {
            entry.tags = tags;
        }
        if let Some(rt) = related_task {
            entry.related_task = Some(rt);
        }
        if !supersedes.is_empty() {
            entry.supersedes = supersedes;
        }
        entry.actor = Some("human".to_string());
    })?;
    println!(
        "updated {} #{}: {}",
        updated.kind.as_str(),
        updated.id,
        updated.title
    );
    Ok(())
}

fn print_kb_entry(e: &KnowledgeEntry) {
    println!("[{}] #{} {}", e.kind.as_str(), e.id, e.title);
    if !e.tags.is_empty() {
        println!("tags: {}", e.tags.join(", "));
    }
    if let Some(rt) = e.related_task {
        println!("related task: #{rt}");
    }
    if let Some(a) = &e.actor {
        println!("actor: {a}");
    }
    println!("created: {}", e.created_at);
    println!("updated: {}", e.updated_at);
    println!();
    println!("{}", e.content);
}

fn cmd_project_list(json: bool) -> Result<()> {
    let ws = match project::resolve_context()? {
        JayContext::Workspace(root) => root,
        JayContext::Project(root) => root.parent().map(|p| p.to_path_buf()).unwrap_or(root),
    };
    ls_projects(&ws, json)
}

/// Applies a state-machine action to a task (the CLI side).
fn apply_action(id: i64, action: TaskAction, block_reason: Option<&str>) -> Result<()> {
    let root = require_project()?;
    let before = tasks::load_task(&root, id)?
        .ok_or_else(|| anyhow::anyhow!("task {id} not found"))?
        .git_diagnostics
        .len();
    let t = crate::service::apply_action(&root, id, action, "human", block_reason)?;
    if t.blocked {
        println!(
            "task {} is blocked: {}",
            id,
            t.block_reason.as_deref().unwrap_or("no reason")
        );
    } else {
        println!("task {} -> {}", id, t.status.as_str());
    }
    for d in t.git_diagnostics.iter().skip(before) {
        println!("git integration ({}): {}", d.operation, d.message);
    }
    Ok(())
}

fn require_project() -> Result<PathBuf> {
    match project::resolve_context()? {
        JayContext::Project(p) => Ok(p),
        JayContext::Workspace(_) => {
            bail!("this command needs a project — cd into a project folder (or a subfolder)")
        }
    }
}

fn cmd_current() -> Result<()> {
    let ctx = project::resolve_context()?;
    let kind = if ctx.is_project() {
        "project"
    } else {
        "workspace"
    };
    let name = match &ctx {
        JayContext::Project(_) => project::load_config(ctx.root())?.name,
        JayContext::Workspace(_) => project::folder_name(ctx.root()),
    };
    println!("{kind}\t{}\t{}", ctx.root().display(), name);
    Ok(())
}

fn cmd_config(field: Option<String>, value: Option<String>, show_origin: bool) -> Result<()> {
    let root = require_project()?;
    let mut cfg = project::load_config(&root)?;

    match (field, value) {
        (None, _) => {
            if show_origin {
                for (name, origin) in config_origins(&root, &cfg) {
                    println!("{name}\t{origin}");
                }
            } else {
                print!("{}", toml::to_string_pretty(&cfg)?);
            }
        }
        (Some(f), Some(v)) => {
            set_config_field(&mut cfg, &f, &v)?;
            project::save_config(&root, &cfg)?;
            println!("set {f} = {v}");
        }
        (Some(f), None) => {
            let val = get_config_field(&cfg, &f)?;
            if show_origin {
                let origin = config_origins(&root, &cfg)
                    .into_iter()
                    .find(|(n, _)| n == &f)
                    .map(|(_, o)| o)
                    .unwrap_or_else(|| "default".to_string());
                println!("{val}\t{origin}");
            } else {
                println!("{val}");
            }
        }
    }
    Ok(())
}

fn get_config_field(cfg: &ProjectConfig, field: &str) -> Result<String> {
    if let Ok(v) = std::env::var(format!("jay_{}", field.to_uppercase())) {
        return Ok(v);
    }
    let local = match field {
        "name" => cfg.name.clone(),
        "description" => cfg.description.clone(),
        "goal" => cfg.goal.clone().unwrap_or_default(),
        "git_repo" => cfg.git_repo.clone().unwrap_or_default(),
        "branch_template" => cfg.branch_template.clone().unwrap_or_default(),
        "git_integration" => cfg
            .git_integration
            .map(|g| g.as_str().to_string())
            .unwrap_or_else(|| {
                format!(
                    "{} (legacy default)",
                    cfg.effective_git_integration().as_str()
                )
            }),
        "links" => cfg.links.join(","),
        _ => bail!("unknown config field: {field}"),
    };
    Ok(local)
}

fn set_config_field(cfg: &mut ProjectConfig, field: &str, value: &str) -> Result<()> {
    match field {
        "name" => cfg.name = value.to_string(),
        "description" => cfg.description = value.to_string(),
        "goal" => cfg.goal = Some(value.to_string()),
        "git_repo" => cfg.git_repo = Some(value.to_string()),
        "branch_template" => cfg.branch_template = Some(value.to_string()),
        "git_integration" => cfg.git_integration = Some(value.parse()?),
        "links" => {
            cfg.links = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
        _ => bail!("unknown config field: {field}"),
    }
    Ok(())
}

fn config_origins(root: &std::path::Path, cfg: &ProjectConfig) -> Vec<(String, String)> {
    let fields = [
        "name",
        "description",
        "goal",
        "git_repo",
        "branch_template",
        "git_integration",
        "links",
    ];
    let _ = root;
    fields
        .iter()
        .map(|f| {
            let origin = if std::env::var(format!("jay_{}", f.to_uppercase())).is_ok() {
                "env"
            } else if *f == "git_integration" {
                if cfg.git_integration.is_some() {
                    "local"
                } else {
                    "default"
                }
            } else if get_config_field(cfg, f)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                "local"
            } else {
                "default"
            };
            (f.to_string(), origin.to_string())
        })
        .collect()
}

fn ls_tasks(root: &std::path::Path, json: bool) -> Result<()> {
    let mut tasks = tasks::load_tasks(root)?;
    tasks.sort_by(|a, b| {
        a.priority
            .rank()
            .cmp(&b.priority.rank())
            .then(a.id.cmp(&b.id))
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
        return Ok(());
    }
    if tasks.is_empty() {
        return Ok(());
    }
    let mut table = Table::new();
    table
        .set_header(vec!["ID", "Status", "Title"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    for t in tasks {
        table.add_row(vec![
            Cell::new(t.id.to_string()),
            status_cell(t.status),
            Cell::new(t.title.clone()),
        ]);
    }
    table
        .column_mut(0)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    println!("{}", table.trim_fmt());
    Ok(())
}

fn ls_projects(root: &std::path::Path, json: bool) -> Result<()> {
    let subs = project::project_subfolders(root);
    if json {
        let mut rows = Vec::new();
        for sub in &subs {
            let cfg = project::load_config(sub)?;
            let counts = status_counts(sub)?;
            let mut map = serde_json::Map::new();
            for (s, c) in &counts {
                map.insert(s.as_str().to_string(), serde_json::Value::from(*c as i64));
            }
            rows.push(serde_json::json!({
                "name": cfg.name,
                "path": sub.display().to_string(),
                "counts": map,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    // Text: render an aligned table (comfy-table handles width + ANSI).
    let mut table = Table::new();
    table
        .set_header(vec!["Project", "Tasks", "Active"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    for sub in &subs {
        let cfg = project::load_config(sub)?;
        let counts = status_counts(sub)?;
        let total: usize = counts.values().sum();
        let active = counts.get(&TaskStatus::Started).copied().unwrap_or(0);
        table.add_row(vec![cfg.name, total.to_string(), active.to_string()]);
    }
    table
        .column_mut(1)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    table
        .column_mut(2)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    println!("{}", table.trim_fmt());
    Ok(())
}

fn status_counts(root: &std::path::Path) -> Result<std::collections::BTreeMap<TaskStatus, usize>> {
    let mut counts = std::collections::BTreeMap::new();
    for t in tasks::load_tasks(root)? {
        *counts.entry(t.status).or_insert(0) += 1;
    }
    Ok(counts)
}

#[allow(clippy::too_many_arguments)]
fn cmd_new(
    title: String,
    description: Option<String>,
    priority: Option<String>,
    label: Vec<String>,
    estimate_points: Option<i64>,
    estimate_hours: Option<f64>,
    deadline: Option<String>,
    depends_on: Vec<i64>,
) -> Result<()> {
    let root = require_project()?;
    let prio: Option<Priority> = priority.map(|p| p.parse()).transpose()?;
    let t = crate::service::create_task(&root, |id| {
        let mut t = Task::new(id, title, description.unwrap_or_default());
        if let Some(p) = prio {
            t.priority = p;
        }
        t.labels = label;
        t.estimate_points = estimate_points;
        t.estimate_hours = estimate_hours;
        t.deadline = deadline;
        t.depends_on = depends_on;
        t.actor = Some("human".to_string());
        t
    })?;
    println!("created task {}: {}", t.id, t.title);
    Ok(())
}

fn cmd_next(json: bool) -> Result<()> {
    let root = require_project()?;
    let ready = crate::service::ready_tasks(&root)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ready)?);
        return Ok(());
    }
    if ready.is_empty() {
        println!("(no ready tasks: everything open is blocked or waiting on dependencies)");
        return Ok(());
    }
    let mut table = Table::new();
    table
        .set_header(vec!["ID", "Priority", "Title"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    for t in &ready {
        table.add_row(vec![
            Cell::new(t.id.to_string()),
            Cell::new(t.priority.as_str()),
            Cell::new(t.title.clone()),
        ]);
    }
    table
        .column_mut(0)
        .expect("column")
        .set_cell_alignment(CellAlignment::Right);
    println!("{}", table.trim_fmt());
    Ok(())
}

fn status_overview(json: bool) -> Result<()> {
    let root = require_project()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::service::project_context(&root)?)?
        );
        return Ok(());
    }
    let cfg = project::load_config(&root)?;
    let tasks = tasks::load_tasks(&root)?;
    let counts = status_counts(&root)?;
    println!("project: {} ({})", cfg.name, root.display());
    println!();
    for s in TaskStatus::ALL {
        let c = counts.get(&s).copied().unwrap_or(0);
        if c > 0 {
            println!("  {:<12} {}", s.as_str(), c);
        }
    }
    let active: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Started)
        .collect();
    let blocked: Vec<&Task> = tasks.iter().filter(|t| t.blocked).collect();
    if !active.is_empty() {
        println!("\nactive:");
        for t in active {
            println!("  {:>3} {}", t.id, t.title);
        }
    }
    if !blocked.is_empty() {
        println!("\nblocked:");
        for t in blocked {
            println!("  {:>3} {}", t.id, t.title);
        }
    }
    let ready = crate::service::ready_tasks(&root)?;
    if !ready.is_empty() {
        println!("\nready (open, unblocked, deps closed):");
        for t in ready.iter().take(10) {
            println!("  {:>3} [{}] {}", t.id, t.priority.as_str(), t.title);
        }
        if ready.len() > 10 {
            println!("  ... {} more (jay next)", ready.len() - 10);
        }
    }
    if let Some(cs) = knowledge::current_status(&root)? {
        let label = if cs.designated {
            "current summary".to_string()
        } else {
            knowledge::FALLBACK_NOTE.to_string()
        };
        println!("\n{label}:");
        println!("  status #{} {}", cs.entry.id, cs.entry.title);
        let fresh = knowledge::summary_freshness(&root, &cs)?;
        if fresh.possibly_stale {
            for fact in &fresh.facts {
                println!("  possibly stale: {fact}");
            }
        }
    }
    if let Some(branch) = current_branch(&root) {
        println!("\nbranch: {}", branch);
    }
    Ok(())
}

fn current_branch(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() {
        None
    } else {
        Some(b)
    }
}

fn cmd_show(id: i64) -> Result<()> {
    let root = require_project()?;
    let t = tasks::load_task(&root, id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))?;
    let all = tasks::load_tasks(&root)?;
    let out = format_task_detail(&t, &all);
    page(&out);
    Ok(())
}

fn format_task_detail(t: &Task, all: &[Task]) -> String {
    let mut s = String::new();
    s.push_str(&format!("#{} {}\n", t.id, t.title));
    s.push_str(&format!("status: {}\n", t.status.as_str()));
    s.push_str(&format!("priority: {}\n", t.priority.as_str()));
    if !t.labels.is_empty() {
        s.push_str(&format!("labels: {}\n", t.labels.join(", ")));
    }
    if let Some(v) = t.estimate_points {
        s.push_str(&format!("estimate: {} points", v));
    }
    if let Some(v) = t.estimate_hours {
        s.push_str(&format!(" / {}h", v));
    }
    if t.estimate_points.is_some() || t.estimate_hours.is_some() {
        s.push('\n');
    }
    if let Some(d) = &t.deadline {
        s.push_str(&format!("deadline: {}\n", d));
    }
    if !t.description.is_empty() {
        s.push_str(&format!("\ndescription:\n{}\n", t.description));
    }
    if !t.acceptance.is_empty() {
        s.push_str("\nacceptance:\n");
        for a in &t.acceptance {
            s.push_str(&format!("  - {}\n", a));
        }
    }
    if !t.context.is_empty() {
        s.push_str(&format!("\ncontext:\n{}\n", t.context));
    }
    if !t.depends_on.is_empty() {
        s.push_str("\ndepends on:\n");
        for d in &t.depends_on {
            let state = all.iter().find(|o| o.id == *d);
            match state {
                Some(o) if o.status == TaskStatus::Closed => {
                    s.push_str(&format!("  - #{d} (closed)\n"))
                }
                Some(o) => s.push_str(&format!(
                    "  - #{d} ({}) — UNMET: prerequisite must be closed (cancelled does not count)\n",
                    o.status.as_str()
                )),
                None => s.push_str(&format!("  - #{d} (missing) — UNMET\n")),
            }
        }
    }
    if !t.links.is_empty() {
        s.push_str("\nlinks:\n");
        for l in &t.links {
            s.push_str(&format!("  - {}\n", l));
        }
    }
    s.push_str(&format!("\ncreated: {}\n", t.created_at));
    if let Some(v) = &t.started_at {
        s.push_str(&format!("started: {}\n", v));
    }
    if let Some(v) = &t.done_at {
        s.push_str(&format!("done: {}\n", v));
    }
    if let Some(a) = &t.actor {
        s.push_str(&format!("actor: {}\n", a));
    }
    let has_report = t.report_result.is_some()
        || t.report_validation.is_some()
        || !t.report_problems.is_empty()
        || !t.report_ideas.is_empty()
        || !t.report_decisions.is_empty();
    if has_report {
        s.push_str("\nreport:\n");
        if let Some(v) = &t.report_result {
            s.push_str(&format!("  result: {}\n", v));
        }
        if let Some(v) = &t.report_validation {
            s.push_str(&format!("  validation: {}\n", v));
        }
        for p in &t.report_problems {
            s.push_str(&format!("  problem: {}\n", p));
        }
        for i in &t.report_ideas {
            s.push_str(&format!("  idea: {}\n", i));
        }
        for d in &t.report_decisions {
            s.push_str(&format!("  decision: {}\n", d));
        }
    }
    if let Some(orig) = t.follow_up_of {
        s.push_str(&format!("\nfollow-up of: #{orig}\n"));
    }
    if !t.git_diagnostics.is_empty() {
        s.push_str("\ngit integration diagnostics:\n");
        for d in &t.git_diagnostics {
            s.push_str(&format!("  [{}] {} ({})\n", d.operation, d.message, d.at));
        }
    }
    if !t.history.is_empty() {
        s.push_str("\nhistory:\n");
        for ev in &t.history {
            let from = ev.from.map(|f| f.as_str()).unwrap_or("?");
            s.push_str(&format!(
                "  {} {from} -> {} by {}\n",
                ev.at,
                ev.to.as_str(),
                ev.actor
            ));
            if let Some(r) = &ev.reason {
                s.push_str(&format!("    reason: {r}\n"));
            }
            if let Some(pd) = &ev.prior_done_at {
                s.push_str(&format!("    prior done_at: {pd}\n"));
            }
            if let Some(pr) = &ev.prior_report {
                s.push_str("    prior report:\n");
                if let Some(v) = &pr.result {
                    s.push_str(&format!("      result: {v}\n"));
                }
                if let Some(v) = &pr.validation {
                    s.push_str(&format!("      validation: {v}\n"));
                }
                for p in &pr.problems {
                    s.push_str(&format!("      problem: {p}\n"));
                }
                for i in &pr.ideas {
                    s.push_str(&format!("      idea: {i}\n"));
                }
                for d in &pr.decisions {
                    s.push_str(&format!("      decision: {d}\n"));
                }
                for l in &pr.links {
                    s.push_str(&format!("      link: {l}\n"));
                }
            }
        }
    }
    s
}

fn cmd_find(
    term: String,
    priority: Option<String>,
    label: Option<String>,
    status: Option<String>,
) -> Result<()> {
    let root = require_project()?;
    let tasks = tasks::load_tasks(&root)?;
    let prio: Option<Priority> = priority.map(|p| p.parse()).transpose()?;
    let st: Option<TaskStatus> = status.map(|s| s.parse()).transpose()?;
    let mut table = Table::new();
    table
        .set_header(vec!["ID", "Status", "Title"])
        .load_preset(border_preset())
        .set_content_arrangement(ContentArrangement::Dynamic);
    let mut found = 0;
    for t in tasks {
        if !fuzzy_match(&term, &t.title) {
            continue;
        }
        if let Some(p) = prio {
            if t.priority != p {
                continue;
            }
        }
        if let Some(l) = &label {
            if !t.labels.iter().any(|x| x == l) {
                continue;
            }
        }
        if let Some(s) = st {
            if t.status != s {
                continue;
            }
        }
        table.add_row(vec![
            Cell::new(t.id.to_string()),
            status_cell(t.status),
            Cell::new(t.title.clone()),
        ]);
        found += 1;
    }
    if found > 0 {
        table
            .column_mut(0)
            .expect("column")
            .set_cell_alignment(CellAlignment::Right);
        println!("{}", table.trim_fmt());
    }
    Ok(())
}

// True if needle is a subsequence of hay (case-insensitive).
fn fuzzy_match(needle: &str, hay: &str) -> bool {
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() {
        return true;
    }
    let lower = hay.to_lowercase();
    let mut it = lower.chars();
    for c in needle {
        if !it.any(|h| h == c) {
            return false;
        }
    }
    true
}

fn cmd_move(id: i64, to: String) -> Result<()> {
    let root = require_project()?;
    let dest = std::path::PathBuf::from(&to);
    let dest = if dest.is_absolute() {
        dest
    } else {
        std::env::current_dir()?.join(dest)
    };
    match project::resolve_context_at(&dest)? {
        JayContext::Project(d) => {
            let moved = crate::service::move_task(&root, id, &d, "human")?;
            println!("moved task {id} -> {} (new id {})", d.display(), moved.id);
        }
        JayContext::Workspace(_) => bail!("destination must be a project folder (with .nest)"),
    }
    Ok(())
}

fn cmd_sync() -> Result<()> {
    let root = require_project()?;
    run_git(&root, &["pull"])?;
    run_git(&root, &["push"])?;
    println!("synced");
    Ok(())
}

fn run_git(root: &std::path::Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn cmd_doctor(json: bool) -> Result<i32> {
    let root = require_project()?;
    let findings = crate::diag::inspect(&root);
    let errors = findings
        .iter()
        .filter(|f| f.severity == crate::diag::Severity::Error)
        .count();
    let warnings = findings.len() - errors;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": errors == 0,
                "errors": errors,
                "warnings": warnings,
                "findings": findings,
            }))?
        );
        return Ok(if errors > 0 { 1 } else { 0 });
    }
    if let Ok(cfg) = project::load_config(&root) {
        if root.join(".git").is_dir() {
            match current_branch(&root) {
                Some(b) => println!("repo ok, branch: {b}"),
                None => println!("repo present (no current branch / detached)"),
            }
        } else if cfg.effective_git_integration() == project::GitIntegration::Auto {
            println!("note: git_integration is 'auto' but this folder is not a git repo");
        }
    }
    if findings.is_empty() {
        println!("ok");
        return Ok(0);
    }
    for f in &findings {
        let field = f
            .field
            .as_ref()
            .map(|x| format!(" field={x}"))
            .unwrap_or_default();
        println!(
            "{}[{}] {}{}: {}",
            f.severity.as_str(),
            f.code,
            f.file,
            field,
            f.message
        );
        println!("    -> {}", f.suggestion);
    }
    println!("{} error(s), {} warning(s)", errors, warnings);
    if errors > 0 {
        println!("hint: run `jay repair` to preview safe explicit repairs (dry run by default)");
    }
    Ok(if errors > 0 { 1 } else { 0 })
}

fn cmd_repair(apply: bool) -> Result<()> {
    let root = require_project()?;
    let report = crate::diag::repair(&root, apply)?;
    let mode = if apply {
        "applied"
    } else {
        "proposed (dry run)"
    };
    if report.proposed.is_empty() {
        println!("no repairable issues found");
    } else {
        for op in &report.proposed {
            println!("{} [{}] {}: {}", mode, op.code, op.file, op.description);
        }
    }
    for b in &report.backups {
        println!("backup: {b}");
    }
    if !report.unresolved.is_empty() {
        println!();
        println!(
            "unresolved (repair cannot fix these — they need explicit user/agent updates; jay never fabricates evidence):"
        );
        for f in &report.unresolved {
            println!(
                "  {}[{}] {}: {}",
                f.severity.as_str(),
                f.code,
                f.file,
                f.message
            );
        }
    }
    if !apply && !report.proposed.is_empty() {
        println!();
        println!("re-run with --apply to write these changes (backups are created automatically)");
    }
    Ok(())
}

fn status_cell(s: TaskStatus) -> Cell {
    if NO_COLOR.load(Ordering::Relaxed) {
        return Cell::new(s.as_str());
    }
    let color = match s {
        TaskStatus::Open => Color::DarkGrey,
        TaskStatus::Started => Color::Yellow,
        TaskStatus::Review => Color::Magenta,
        TaskStatus::Closed => Color::Green,
        TaskStatus::Cancelled => Color::DarkGrey,
    };
    Cell::new(s.as_str()).fg(color)
}

fn page(text: &str) {
    if text.lines().count() <= 20 {
        print!("{text}");
        return;
    }
    let pager = std::env::var("PAGER").ok().filter(|p| !p.is_empty());
    let pager = pager.unwrap_or_else(|| "less".to_string());
    use std::io::Write;
    if let Ok(mut child) = std::process::Command::new(&pager)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    } else {
        print!("{text}");
    }
}
