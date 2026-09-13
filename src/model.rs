use serde::{Deserialize, Serialize};

/// Status of a task — a fixed, ordered state machine.
///
/// `open -> started -> review -> closed` is the happy path; `cancelled` is a
/// side exit (reopenable); `blocked` is NOT a status but a flag on the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Backlog — not started.
    #[serde(rename = "open")]
    Open,
    /// Work in progress (a branch may be created).
    #[serde(rename = "started")]
    Started,
    /// Ready for / under review (a PR may be open).
    #[serde(rename = "review")]
    Review,
    /// Finished successfully (terminal — no rewind).
    #[serde(rename = "closed")]
    Closed,
    /// Abandoned (reopenable back to open).
    #[serde(rename = "cancelled")]
    Cancelled,
}

impl TaskStatus {
    /// Returns the string form of the status (also the TOML wire form).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::Started => "started",
            TaskStatus::Review => "review",
            TaskStatus::Closed => "closed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    /// True for the successful terminal state, which records `done_at`.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Closed)
    }

    /// All statuses in board/display order.
    pub const ALL: [TaskStatus; 5] = [
        TaskStatus::Open,
        TaskStatus::Started,
        TaskStatus::Review,
        TaskStatus::Closed,
        TaskStatus::Cancelled,
    ];
}

impl std::str::FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(TaskStatus::Open),
            "started" => Ok(TaskStatus::Started),
            "review" => Ok(TaskStatus::Review),
            "closed" => Ok(TaskStatus::Closed),
            "cancelled" => Ok(TaskStatus::Cancelled),
            other => anyhow::bail!("unknown task status: {other}"),
        }
    }
}

/// A state-machine action — what a `task <cmd>` or an MCP call performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// open -> started (records started_at, creates a branch).
    Start,
    /// started -> review (opens a PR).
    Review,
    /// review -> closed (records done_at).
    Done,
    /// review -> started -> open (one step back; never from closed).
    Rewind,
    /// any (except closed) -> cancelled.
    Cancel,
    /// cancelled -> open.
    Reopen,
    /// Set the `blocked` flag (with an optional reason).
    Block,
    /// Clear the `blocked` flag.
    Unblock,
}

impl TaskAction {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskAction::Start => "start",
            TaskAction::Review => "review",
            TaskAction::Done => "done",
            TaskAction::Rewind => "rewind",
            TaskAction::Cancel => "cancel",
            TaskAction::Reopen => "reopen",
            TaskAction::Block => "block",
            TaskAction::Unblock => "unblock",
        }
    }
}

impl std::str::FromStr for TaskAction {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(TaskAction::Start),
            "review" => Ok(TaskAction::Review),
            "done" => Ok(TaskAction::Done),
            "rewind" => Ok(TaskAction::Rewind),
            "cancel" => Ok(TaskAction::Cancel),
            "reopen" => Ok(TaskAction::Reopen),
            "block" => Ok(TaskAction::Block),
            "unblock" => Ok(TaskAction::Unblock),
            other => anyhow::bail!("unknown action: {other}"),
        }
    }
}

/// A snapshot of a prior completion cycle's report, preserved in history
/// when a closed task is reopened. Old reports are claims recorded at a
/// point in time — never silently reused as a new cycle's evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSnapshot {
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub validation: Option<String>,
    #[serde(default)]
    pub problems: Vec<String>,
    #[serde(default)]
    pub ideas: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Integration links (branch/PR) owned by that cycle.
    #[serde(default)]
    pub links: Vec<String>,
}

/// One recorded lifecycle transition (actor, offset-bearing timestamp,
/// old/new state, reason, and — for reopens — the prior cycle's evidence).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    /// RFC 3339 with offset.
    pub at: String,
    pub actor: String,
    #[serde(default)]
    pub from: Option<TaskStatus>,
    pub to: TaskStatus,
    #[serde(default)]
    pub reason: Option<String>,
    /// Prior completion timestamp preserved when reopening a closed task.
    #[serde(default)]
    pub prior_done_at: Option<String>,
    /// Prior report snapshot preserved when reopening a closed task.
    #[serde(default)]
    pub prior_report: Option<ReportSnapshot>,
}

/// Actor and reason supplied to a lifecycle transition.
#[derive(Debug, Clone)]
pub struct LifecycleCtx {
    pub actor: String,
    pub reason: Option<String>,
}

impl LifecycleCtx {
    pub fn new(actor: &str, reason: Option<&str>) -> Self {
        LifecycleCtx {
            actor: actor.to_string(),
            reason: reason.map(|s| s.to_string()),
        }
    }
}

/// Priority of a task (JIRA-like).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Priority {
    #[serde(rename = "highest")]
    Highest,
    #[serde(rename = "high")]
    High,
    #[default]
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "lowest")]
    Lowest,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Highest => "highest",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
            Priority::Lowest => "lowest",
        }
    }

    /// Numeric rank for ordering (0 = most urgent, first).
    pub fn rank(self) -> i32 {
        match self {
            Priority::Highest => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
            Priority::Lowest => 4,
        }
    }

    pub const ALL: [Priority; 5] = [
        Priority::Highest,
        Priority::High,
        Priority::Medium,
        Priority::Low,
        Priority::Lowest,
    ];
}

impl std::str::FromStr for Priority {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        match t {
            "highest" | "0" => Ok(Priority::Highest),
            "high" | "1" => Ok(Priority::High),
            "medium" | "2" => Ok(Priority::Medium),
            "low" | "3" => Ok(Priority::Low),
            "lowest" | "4" => Ok(Priority::Lowest),
            other => anyhow::bail!(
                "unknown priority: {other} (use highest|high|medium|low|lowest, or 0..4)"
            ),
        }
    }
}

/// A structured Git/Forgejo integration diagnostic — recorded when an
/// automatic branch/PR operation fails in `auto` mode. Kept separate from
/// `report_problems` (which are implementation findings by the worker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiagnostic {
    /// The attempted operation, e.g. "branch" or "pr".
    pub operation: String,
    /// What went wrong (or which prerequisite is missing).
    pub message: String,
    /// When it was recorded (RFC 3339 with offset for new records).
    pub at: String,
}

/// A task is a work contract: whoever works on it (human or agent) fills in
/// every field and reports through the standard sections when done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique id — also the task file name (`.nest/tasks/<id>.toml`).
    pub id: i64,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    /// Blocked flag — orthogonal to `status`; a blocked task can't transition.
    #[serde(default)]
    pub blocked: bool,
    /// Optional reason recorded when the task is blocked.
    #[serde(default)]
    pub block_reason: Option<String>,
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
    /// Structured local dependencies (task ids in this project). Independent
    /// of parent/epic/milestone relationships. A task is "ready" when open,
    /// unblocked and every dependency is closed (cancelled does NOT count).
    #[serde(default)]
    pub depends_on: Vec<i64>,
    pub report_result: Option<String>,
    pub report_problems: Vec<String>,
    pub report_ideas: Vec<String>,
    pub report_decisions: Vec<String>,
    pub report_validation: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub done_at: Option<String>,
    /// Set on tasks created as an explicit follow-up of another task.
    /// Does not imply a dependency unless one is requested separately.
    #[serde(default)]
    pub follow_up_of: Option<i64>,
    /// Last writer: "human" (CLI) or "agent:<name>" (MCP).
    pub actor: Option<String>,
    /// Last change timestamp.
    pub updated_at: String,
    /// Structured Git/Forgejo integration failures (auto mode only).
    /// Array of tables — must stay last for TOML serialization.
    #[serde(default)]
    pub git_diagnostics: Vec<GitDiagnostic>,
    /// Recorded lifecycle transitions. Existing files default to empty
    /// history — their past transitions are never invented.
    #[serde(default)]
    pub history: Vec<LifecycleEvent>,
}

/// Current timestamp in RFC 3339 with a UTC offset.
///
/// Legacy records may carry naive local timestamps (`YYYY-MM-DDTHH:MM:SS`);
/// those are preserved as-is — never rewritten with an invented offset.
pub fn now_ts() -> String {
    chrono::Local::now().to_rfc3339()
}

impl Task {
    /// Creates a fresh task with default fields; `created_at`/`updated_at`
    /// are set to now.
    pub fn new(id: i64, title: String, description: String) -> Self {
        let now = now_ts();
        Task {
            id,
            title,
            description,
            status: TaskStatus::Open,
            blocked: false,
            block_reason: None,
            acceptance: Vec::new(),
            context: String::new(),
            assignee: None,
            priority: Priority::default(),
            labels: Vec::new(),
            estimate_points: None,
            estimate_hours: None,
            deadline: None,
            links: Vec::new(),
            parent_id: None,
            epic_id: None,
            milestone_id: None,
            depends_on: Vec::new(),
            report_result: None,
            report_problems: Vec::new(),
            report_ideas: Vec::new(),
            report_decisions: Vec::new(),
            report_validation: None,
            created_at: now.clone(),
            started_at: None,
            done_at: None,
            follow_up_of: None,
            actor: None,
            updated_at: now,
            git_diagnostics: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Applies a state-machine action in place, validating the transition,
    /// recording a lifecycle event (actor, offset-bearing timestamp, old/new
    /// state, reason) and updating lifecycle timestamps.
    /// Does NOT persist — the caller saves.
    pub fn apply(&mut self, action: TaskAction, ctx: &LifecycleCtx) -> anyhow::Result<()> {
        let from = self.status;
        match action {
            TaskAction::Block => {
                if matches!(self.status, TaskStatus::Closed | TaskStatus::Cancelled) {
                    anyhow::bail!("cannot block a {} task", self.status.as_str());
                }
                self.blocked = true;
                self.block_reason = ctx.reason.clone();
            }
            TaskAction::Unblock => {
                if !self.blocked {
                    anyhow::bail!("task is not blocked");
                }
                self.blocked = false;
                self.block_reason = None;
            }
            TaskAction::Cancel => {
                if self.status == TaskStatus::Closed {
                    anyhow::bail!("cannot cancel a closed task");
                }
                self.status = TaskStatus::Cancelled;
                self.blocked = false;
                self.block_reason = None;
                self.history.push(LifecycleEvent {
                    at: now_ts(),
                    actor: ctx.actor.clone(),
                    from: Some(from),
                    to: TaskStatus::Cancelled,
                    reason: ctx.reason.clone(),
                    prior_done_at: None,
                    prior_report: None,
                });
            }
            TaskAction::Reopen => {
                match self.status {
                    // closed -> open: requires an explicit nonblank reason,
                    // preserves the prior cycle in history and clears the
                    // active cycle so a new closure needs fresh evidence
                    TaskStatus::Closed => {
                        if ctx.reason.as_deref().unwrap_or("").trim().is_empty() {
                            anyhow::bail!(
                                "reopening a closed task requires a nonblank reason (e.g. --reason \"review fix\")"
                            );
                        }
                    }
                    // cancelled -> open: kept compatible (reason optional)
                    TaskStatus::Cancelled => {}
                    other => anyhow::bail!(
                        "only a closed or cancelled task can be reopened (current: {})",
                        other.as_str()
                    ),
                }
                // preserve prior completion evidence in history ...
                let (integration, manual): (Vec<String>, Vec<String>) =
                    self.links.drain(..).partition(|l| is_integration_link(l));
                self.links = manual;
                let had_cycle = self.done_at.is_some()
                    || self.report_result.is_some()
                    || self.report_validation.is_some()
                    || !integration.is_empty();
                let prior_report = if had_cycle {
                    Some(ReportSnapshot {
                        result: self.report_result.take(),
                        validation: self.report_validation.take(),
                        problems: self.report_problems.clone(),
                        ideas: self.report_ideas.clone(),
                        decisions: self.report_decisions.clone(),
                        links: integration,
                    })
                } else {
                    None
                };
                let prior_done_at = self.done_at.take();
                // ... then clear active-cycle timestamps/evidence so a new
                // closure requires fresh result/validation. Other report
                // sections (problems/ideas/decisions) and metadata are kept.
                self.started_at = None;
                self.status = TaskStatus::Open;
                self.blocked = false;
                self.block_reason = None;
                self.history.push(LifecycleEvent {
                    at: now_ts(),
                    actor: ctx.actor.clone(),
                    from: Some(from),
                    to: TaskStatus::Open,
                    reason: ctx.reason.clone(),
                    prior_done_at,
                    prior_report,
                });
            }
            other => {
                if self.blocked {
                    anyhow::bail!("task is blocked — unblock it first");
                }
                match other {
                    TaskAction::Start => {
                        if self.status != TaskStatus::Open {
                            anyhow::bail!(
                                "cannot start a {} task (must be open)",
                                self.status.as_str()
                            );
                        }
                        self.status = TaskStatus::Started;
                        if self.started_at.is_none() {
                            self.started_at = Some(now_ts());
                        }
                    }
                    TaskAction::Review => {
                        if self.status != TaskStatus::Started {
                            anyhow::bail!(
                                "cannot review a {} task (must be started)",
                                self.status.as_str()
                            );
                        }
                        self.status = TaskStatus::Review;
                    }
                    TaskAction::Done => {
                        if self.status != TaskStatus::Review {
                            anyhow::bail!(
                                "cannot close a {} task (must be in review)",
                                self.status.as_str()
                            );
                        }
                        let blank =
                            |o: &Option<String>| o.as_deref().unwrap_or("").trim().is_empty();
                        if blank(&self.report_result) || blank(&self.report_validation) {
                            anyhow::bail!(
                                "closing requires completion evidence: set a nonblank report result and validation first (e.g. `jay task complete <id> --report-file <json>`)"
                            );
                        }
                        self.status = TaskStatus::Closed;
                        self.done_at = Some(now_ts());
                    }
                    TaskAction::Rewind => match self.status {
                        TaskStatus::Review => self.status = TaskStatus::Started,
                        TaskStatus::Started => self.status = TaskStatus::Open,
                        other => anyhow::bail!("cannot rewind a {} task", other.as_str()),
                    },
                    TaskAction::Block
                    | TaskAction::Unblock
                    | TaskAction::Cancel
                    | TaskAction::Reopen => unreachable!(),
                }
                if self.status != from {
                    self.history.push(LifecycleEvent {
                        at: now_ts(),
                        actor: ctx.actor.clone(),
                        from: Some(from),
                        to: self.status,
                        reason: ctx.reason.clone(),
                        prior_done_at: None,
                        prior_report: None,
                    });
                }
            }
        }
        self.actor = Some(ctx.actor.clone());
        self.updated_at = now_ts();
        Ok(())
    }
}

/// True for links owned by git integration (branch/PR markers and PR URLs) —
/// these belong to a completion cycle and are snapshotted into history on
/// reopen. Manual links stay on the task.
pub fn is_integration_link(link: &str) -> bool {
    link.starts_with("branch:") || link.starts_with("pr:") || link.contains("/pulls/")
}

/// Kind of knowledge base entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeKind {
    /// Architecture Decision Record — why we chose X over Y.
    #[serde(rename = "decision")]
    Decision,
    /// Current project state, blockers, next steps.
    #[serde(rename = "status")]
    Status,
    /// Implementation notes, architecture details, things-to-remember.
    #[serde(rename = "note")]
    Note,
}

impl KnowledgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KnowledgeKind::Decision => "decision",
            KnowledgeKind::Status => "status",
            KnowledgeKind::Note => "note",
        }
    }

    /// The TOML file name inside `.nest/kb/`.
    pub fn filename(self) -> &'static str {
        match self {
            KnowledgeKind::Decision => "decisions.toml",
            KnowledgeKind::Status => "status.toml",
            KnowledgeKind::Note => "notes.toml",
        }
    }

    pub const ALL: [KnowledgeKind; 3] = [
        KnowledgeKind::Decision,
        KnowledgeKind::Status,
        KnowledgeKind::Note,
    ];
}

impl std::str::FromStr for KnowledgeKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "decision" | "decisions" => Ok(KnowledgeKind::Decision),
            "status" => Ok(KnowledgeKind::Status),
            "note" | "notes" => Ok(KnowledgeKind::Note),
            other => anyhow::bail!("unknown knowledge kind: {other} (use decision|status|note)"),
        }
    }
}

/// A knowledge base entry — project-scoped context that agents and humans
/// query on demand instead of reading a monolithic handoff file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: i64,
    pub kind: KnowledgeKind,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub related_task: Option<i64>,
    /// Explicit supersession references (mainly status entries): the ids this
    /// entry supersedes. Superseded text is preserved, never rewritten.
    #[serde(default)]
    pub supersedes: Vec<i64>,
    /// Optional Git commit reference recorded with the entry/summary.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl KnowledgeEntry {
    pub fn new(id: i64, kind: KnowledgeKind, title: String, content: String) -> Self {
        let now = now_ts();
        KnowledgeEntry {
            id,
            kind,
            title,
            content,
            tags: Vec::new(),
            related_task: None,
            supersedes: Vec::new(),
            commit: None,
            actor: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

/// A milestone (release/version, e.g. v1.0) that groups tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Milestone {
    pub id: i64,
    pub name: String,
    pub target_date: Option<String>,
    pub done_at: Option<String>,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_parses_names_and_numeric_rank() {
        assert_eq!("highest".parse::<Priority>().unwrap(), Priority::Highest);
        assert_eq!("0".parse::<Priority>().unwrap(), Priority::Highest);
        assert_eq!("1".parse::<Priority>().unwrap(), Priority::High);
        assert_eq!("2".parse::<Priority>().unwrap(), Priority::Medium);
        assert_eq!("3".parse::<Priority>().unwrap(), Priority::Low);
        assert_eq!("4".parse::<Priority>().unwrap(), Priority::Lowest);
        assert_eq!(" high ".parse::<Priority>().unwrap(), Priority::High);
        assert!("urgent".parse::<Priority>().is_err());
    }

    #[test]
    fn task_serializes_status_and_priority_as_strings() {
        let t = Task::new(1, "t".into(), "d".into());
        let s = toml::to_string_pretty(&t).unwrap();
        assert!(s.contains("status = \"open\""));
        assert!(s.contains("priority = \"medium\""));
    }

    #[test]
    fn task_roundtrips_through_toml() {
        let mut t = Task::new(7, "title".into(), "desc".into());
        t.apply(TaskAction::Start, &LifecycleCtx::new("test", None))
            .unwrap();
        t.labels = vec!["a".into(), "b".into()];
        t.report_result = Some("done".into());
        t.actor = Some("human".into());
        let s = toml::to_string_pretty(&t).unwrap();
        let back: Task = toml::from_str(&s).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.status, TaskStatus::Started);
        assert_eq!(back.labels, vec!["a", "b"]);
        assert_eq!(back.actor.as_deref(), Some("human"));
    }

    #[test]
    fn state_machine_enforces_order() {
        let mut t = Task::new(1, "t".into(), "d".into());
        assert_eq!(t.status, TaskStatus::Open);

        // start requires open
        assert!(t
            .apply(TaskAction::Done, &LifecycleCtx::new("test", None))
            .is_err());
        t.apply(TaskAction::Start, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Started);
        assert!(t.started_at.is_some());

        // review requires started
        t.apply(TaskAction::Review, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Review);

        // done requires review AND completion evidence
        assert!(t
            .apply(TaskAction::Done, &LifecycleCtx::new("test", None))
            .is_err());
        t.report_result = Some("implemented".into());
        assert!(t
            .apply(TaskAction::Done, &LifecycleCtx::new("test", None))
            .is_err());
        t.report_validation = Some("tests pass".into());
        t.apply(TaskAction::Done, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Closed);
        assert!(t.done_at.is_some());

        // closed is terminal
        assert!(t
            .apply(TaskAction::Rewind, &LifecycleCtx::new("test", None))
            .is_err());
        assert!(t
            .apply(TaskAction::Cancel, &LifecycleCtx::new("test", None))
            .is_err());
    }

    #[test]
    fn rewind_goes_one_step_back() {
        let mut t = Task::new(1, "t".into(), "d".into());
        t.apply(TaskAction::Start, &LifecycleCtx::new("test", None))
            .unwrap();
        t.apply(TaskAction::Review, &LifecycleCtx::new("test", None))
            .unwrap();
        t.apply(TaskAction::Rewind, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Started);
        t.apply(TaskAction::Rewind, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Open);
        assert!(t
            .apply(TaskAction::Rewind, &LifecycleCtx::new("test", None))
            .is_err()); // open can't rewind
    }

    #[test]
    fn block_blocks_transitions_and_unblock_releases() {
        let mut t = Task::new(1, "t".into(), "d".into());
        t.apply(TaskAction::Start, &LifecycleCtx::new("test", None))
            .unwrap();
        t.apply(
            TaskAction::Block,
            &LifecycleCtx::new("test", Some("waiting on hardware")),
        )
        .unwrap();
        assert!(t.blocked);
        assert_eq!(t.block_reason.as_deref(), Some("waiting on hardware"));
        // blocked: can't transition
        assert!(t
            .apply(TaskAction::Review, &LifecycleCtx::new("test", None))
            .is_err());
        t.apply(TaskAction::Unblock, &LifecycleCtx::new("test", None))
            .unwrap();
        assert!(!t.blocked);
        t.apply(TaskAction::Review, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Review);
    }

    #[test]
    fn cancel_then_reopen() {
        let mut t = Task::new(1, "t".into(), "d".into());
        t.apply(TaskAction::Start, &LifecycleCtx::new("test", None))
            .unwrap();
        t.apply(TaskAction::Cancel, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled);
        assert!(t
            .apply(TaskAction::Done, &LifecycleCtx::new("test", None))
            .is_err()); // cancelled can't close directly
        t.apply(TaskAction::Reopen, &LifecycleCtx::new("test", None))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn knowledge_kind_parses_names_and_aliases() {
        assert_eq!(
            "decision".parse::<KnowledgeKind>().unwrap(),
            KnowledgeKind::Decision
        );
        assert_eq!(
            "decisions".parse::<KnowledgeKind>().unwrap(),
            KnowledgeKind::Decision
        );
        assert_eq!(
            "status".parse::<KnowledgeKind>().unwrap(),
            KnowledgeKind::Status
        );
        assert_eq!(
            "note".parse::<KnowledgeKind>().unwrap(),
            KnowledgeKind::Note
        );
        assert_eq!(
            "notes".parse::<KnowledgeKind>().unwrap(),
            KnowledgeKind::Note
        );
        assert!("unknown".parse::<KnowledgeKind>().is_err());
    }

    #[test]
    fn knowledge_kind_filenames() {
        assert_eq!(KnowledgeKind::Decision.filename(), "decisions.toml");
        assert_eq!(KnowledgeKind::Status.filename(), "status.toml");
        assert_eq!(KnowledgeKind::Note.filename(), "notes.toml");
    }

    #[test]
    fn knowledge_entry_roundtrips_through_toml() {
        let mut e = KnowledgeEntry::new(
            1,
            KnowledgeKind::Decision,
            "Use Odin".into(),
            "Because raylib".into(),
        );
        e.tags = vec!["stack".into(), "odin".into()];
        e.related_task = Some(42);
        e.actor = Some("human".into());
        let s = toml::to_string_pretty(&e).unwrap();
        let back: KnowledgeEntry = toml::from_str(&s).unwrap();
        assert_eq!(back.id, 1);
        assert_eq!(back.kind, KnowledgeKind::Decision);
        assert_eq!(back.title, "Use Odin");
        assert_eq!(back.tags, vec!["stack", "odin"]);
        assert_eq!(back.related_task, Some(42));
    }

    fn ctx(reason: Option<&str>) -> LifecycleCtx {
        LifecycleCtx::new("tester", reason)
    }

    fn closed_task_with_evidence() -> Task {
        let mut t = Task::new(1, "t".into(), "d".into());
        t.apply(TaskAction::Start, &ctx(None)).unwrap();
        t.apply(TaskAction::Review, &ctx(None)).unwrap();
        t.report_result = Some("cycle-1 result".into());
        t.report_validation = Some("cycle-1 validation".into());
        t.report_problems = vec!["p1".into()];
        t.report_ideas = vec!["i1".into()];
        t.report_decisions = vec!["d1".into()];
        t.links = vec![
            "branch:feat/1-t".into(),
            "http://forge/o/r/pulls/5".into(),
            "https://docs.example.com/manual".into(),
        ];
        t.apply(TaskAction::Done, &ctx(None)).unwrap();
        t
    }

    #[test]
    fn reopen_closed_requires_nonblank_reason() {
        let mut t = closed_task_with_evidence();
        assert!(t.apply(TaskAction::Reopen, &ctx(None)).is_err());
        assert!(t.apply(TaskAction::Reopen, &ctx(Some("  "))).is_err());
        assert_eq!(t.status, TaskStatus::Closed);
        t.apply(TaskAction::Reopen, &ctx(Some("review found a regression")))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn reopen_preserves_prior_cycle_and_requires_fresh_evidence() {
        let mut t = closed_task_with_evidence();
        let prior_done = t.done_at.clone().unwrap();
        t.apply(TaskAction::Reopen, &ctx(Some("fix review findings")))
            .unwrap();

        // active cycle cleared
        assert!(t.started_at.is_none());
        assert!(t.done_at.is_none());
        assert!(t.report_result.is_none());
        assert!(t.report_validation.is_none());
        // other report sections and metadata preserved
        assert_eq!(t.report_problems, vec!["p1"]);
        assert_eq!(t.report_ideas, vec!["i1"]);
        assert_eq!(t.report_decisions, vec!["d1"]);
        // manual links stay; integration links move to the snapshot
        assert_eq!(t.links, vec!["https://docs.example.com/manual"]);

        // history holds the prior completion timestamp and report snapshot
        let ev = t.history.last().unwrap();
        assert_eq!(ev.from, Some(TaskStatus::Closed));
        assert_eq!(ev.to, TaskStatus::Open);
        assert_eq!(ev.actor, "tester");
        assert_eq!(ev.reason.as_deref(), Some("fix review findings"));
        assert_eq!(ev.prior_done_at.as_deref(), Some(prior_done.as_str()));
        let snap = ev.prior_report.as_ref().unwrap();
        assert_eq!(snap.result.as_deref(), Some("cycle-1 result"));
        assert_eq!(snap.validation.as_deref(), Some("cycle-1 validation"));
        assert_eq!(
            snap.links,
            vec!["branch:feat/1-t", "http://forge/o/r/pulls/5"]
        );
        assert!(crate::diag::parse_ts(&ev.at).is_some());
        assert!(
            matches!(
                crate::diag::parse_ts(&ev.at).unwrap(),
                crate::diag::Timestamp::Offset(_)
            ),
            "history timestamps carry an offset"
        );

        // reclosing without fresh evidence fails
        t.apply(TaskAction::Start, &ctx(None)).unwrap();
        assert!(t.started_at.is_some());
        t.apply(TaskAction::Review, &ctx(None)).unwrap();
        assert!(t.apply(TaskAction::Done, &ctx(None)).is_err());
        t.report_result = Some("cycle-2 result".into());
        t.report_validation = Some("cycle-2 validation".into());
        t.apply(TaskAction::Done, &ctx(None)).unwrap();
        assert_eq!(t.report_result.as_deref(), Some("cycle-2 result"));
        assert_ne!(t.done_at.as_deref(), Some(prior_done.as_str()));
    }

    #[test]
    fn full_cycle_history_is_ordered_and_complete() {
        let mut t = closed_task_with_evidence();
        t.apply(TaskAction::Reopen, &ctx(Some("reopen"))).unwrap();
        t.apply(TaskAction::Start, &ctx(None)).unwrap();
        t.apply(TaskAction::Review, &ctx(None)).unwrap();
        t.report_result = Some("r2".into());
        t.report_validation = Some("v2".into());
        t.apply(TaskAction::Done, &ctx(None)).unwrap();

        let transitions: Vec<(TaskStatus, TaskStatus)> =
            t.history.iter().map(|e| (e.from.unwrap(), e.to)).collect();
        assert_eq!(
            transitions,
            vec![
                (TaskStatus::Open, TaskStatus::Started),
                (TaskStatus::Started, TaskStatus::Review),
                (TaskStatus::Review, TaskStatus::Closed),
                (TaskStatus::Closed, TaskStatus::Open),
                (TaskStatus::Open, TaskStatus::Started),
                (TaskStatus::Started, TaskStatus::Review),
                (TaskStatus::Review, TaskStatus::Closed),
            ]
        );
        // only the reopen event carries a prior-cycle snapshot
        assert_eq!(
            t.history
                .iter()
                .filter(|e| e.prior_report.is_some())
                .count(),
            1
        );
        // timestamps are ordered
        let times: Vec<_> = t
            .history
            .iter()
            .map(|e| crate::diag::parse_ts(&e.at).unwrap())
            .collect();
        for w in times.windows(2) {
            assert!(!matches!(
                crate::diag::cmp_ts(&w[0], &w[1]),
                Some(std::cmp::Ordering::Greater)
            ));
        }
    }

    #[test]
    fn cancelled_reopen_stays_compatible() {
        let mut t = Task::new(1, "t".into(), "d".into());
        t.apply(TaskAction::Start, &ctx(None)).unwrap();
        t.apply(TaskAction::Cancel, &ctx(Some("descoped"))).unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled);
        // reason optional for cancelled
        t.apply(TaskAction::Reopen, &ctx(None)).unwrap();
        assert_eq!(t.status, TaskStatus::Open);
        assert!(t.started_at.is_none(), "fresh cycle after reopen");
        assert_eq!(t.history.len(), 3); // start, cancel, reopen
    }

    #[test]
    fn legacy_tasks_default_to_empty_history() {
        let text = r#"id = 1
title = "legacy"
description = ""
status = "closed"
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
created_at = "2026-09-11T01:13:03"
done_at = "2026-09-11T01:29:23"
updated_at = "2026-09-11T01:29:23"
"#;
        let t: Task = toml::from_str(text).unwrap();
        assert!(t.history.is_empty(), "past transitions are never invented");
        assert_eq!(t.follow_up_of, None);
        assert!(t.depends_on.is_empty());
    }
}
