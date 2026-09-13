//! MCP server (Model Context Protocol) over stdio.
//!
//! Exposes project/context and task tools so agents (orchestrator and
//! sub-agents) can interact with jay in a standardized way. The current
//! project is resolved from the process working directory.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_handler, tool_router, transport::stdio, ErrorData as McpError, ServiceExt};
use std::path::PathBuf;

use crate::model::{KnowledgeEntry, KnowledgeKind, Priority, Task, TaskAction};
use crate::project::{self, Context, ProjectConfig};
use crate::{knowledge, service, tasks};

/// Converts an anyhow error into an MCP internal error.
fn mcp_err(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// Parses an optional priority string, defaulting to medium.
fn parse_priority(s: Option<&str>) -> Result<Priority, McpError> {
    match s {
        Some(p) => p
            .parse::<Priority>()
            .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None)),
        None => Ok(Priority::Medium),
    }
}

/// MCP service. The current project is resolved from the process cwd.
#[derive(Clone)]
pub struct Jay {
    root: PathBuf,
}

// ===== argument structs =====

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct InitProjectArgs {
    dir: String,
    name: String,
    #[serde(default)]
    description: String,
    goal: Option<String>,
    git_repo: Option<String>,
    branch_template: Option<String>,
    #[serde(default)]
    links: Vec<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListProjectsArgs {
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateConfigArgs {
    field: String,
    value: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CreateTaskArgs {
    title: String,
    #[serde(default)]
    description: String,
    priority: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    estimate_points: Option<i64>,
    estimate_hours: Option<f64>,
    deadline: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    #[serde(default)]
    acceptance: Vec<String>,
    #[serde(default)]
    context: String,
    assignee: Option<String>,
    parent_id: Option<i64>,
    epic_id: Option<i64>,
    milestone_id: Option<i64>,
    /// Task ids this task depends on (project-local; validated: no
    /// duplicates, missing ids, self-dependency or cycles).
    #[serde(default)]
    depends_on: Vec<i64>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct GetTaskArgs {
    id: i64,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateTaskArgs {
    id: i64,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance: Vec<String>,
    #[serde(default)]
    context: String,
    assignee: Option<String>,
    priority: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    estimate_points: Option<i64>,
    estimate_hours: Option<f64>,
    deadline: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    parent_id: Option<i64>,
    epic_id: Option<i64>,
    milestone_id: Option<i64>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateTaskStatusArgs {
    id: i64,
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateReportSectionArgs {
    id: i64,
    section: String,
    content: String,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct PromoteIdeaArgs {
    task_id: i64,
    idx: i64,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct MoveTaskArgs {
    id: i64,
    to_dir: String,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CreateFollowUpArgs {
    /// Id of the original task (left unchanged; stays closed when closed).
    task_id: i64,
    /// Title of the new follow-up task (required, nonblank).
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct DedicateAgentArgs {
    task_id: i64,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct PatchTaskArgs {
    id: i64,
    /// Partial task edit as a JSON object; see the tool description for semantics.
    patch: serde_json::Value,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateReportArgs {
    id: i64,
    /// "replace" (default) or "append".
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    validation: Option<String>,
    #[serde(default)]
    problems: Option<Vec<String>>,
    #[serde(default)]
    ideas: Option<Vec<String>>,
    #[serde(default)]
    decisions: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CompleteTaskArgs {
    id: i64,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    validation: Option<String>,
    #[serde(default)]
    problems: Option<Vec<String>>,
    #[serde(default)]
    ideas: Option<Vec<String>>,
    #[serde(default)]
    decisions: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

// ===== knowledge base argument structs =====

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct GetProjectStatusArgs {
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListKbArgs {
    /// Filter by kind: decision, status, or note. Omit for all.
    #[serde(default)]
    kind: Option<String>,
    /// Filter by tag (case-insensitive substring match).
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct GetKbArgs {
    id: i64,
    kind: String,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct AddKbArgs {
    kind: String,
    title: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    related_task: Option<i64>,
    /// Status entries: ids this entry supersedes (validated; text preserved).
    #[serde(default)]
    supersedes: Vec<i64>,
    /// Optional git commit reference recorded with the entry.
    #[serde(default)]
    commit: Option<String>,
    /// Status entries: also designate as the current summary atomically.
    #[serde(default)]
    set_current: bool,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SetCurrentSummaryArgs {
    /// Id of an existing status entry with meaningful content.
    id: i64,
    /// Optional git commit the summary describes.
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateKbArgs {
    id: i64,
    kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    related_task: Option<i64>,
    /// Replace supersession references (validated: existing targets, no cycles).
    #[serde(default)]
    supersedes: Option<Vec<i64>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SearchKbArgs {
    /// Search query (BM25-ranked full-text search).
    query: String,
    /// Optional kind filter: decision, status, or note.
    #[serde(default)]
    kind: Option<String>,
    /// Max results to return (default 10).
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    dir: Option<String>,
}

/// Builds the standardized sub-agent prompt for a task.
fn build_prompt(task: &Task, config: &ProjectConfig) -> String {
    let acceptance = if task.acceptance.is_empty() {
        "(none)".to_string()
    } else {
        task.acceptance
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let context = if task.context.is_empty() {
        "(none)".to_string()
    } else {
        task.context.clone()
    };
    format!(
        "You are a sub-agent working on a task in the jay project management tool.\n\
         Project: {} — {}\n\n\
         TASK (id {}): {}\n\n\
         {}\n\n\
         Status: {}\n\
         Acceptance criteria:\n{}\n\
         Context:\n{}\n\n\
         Instructions (in this order):\n\
         1. Start the task: update_task_status(id={}, action='start').\n\
         2. Do the work to satisfy the acceptance criteria.\n\
         3. Send it to review: update_task_status(id={}, action='review').\n\
         4. Finish with ONE typed completion call carrying all five report sections:\n\
            complete_task(id={}, result=..., validation=..., problems=[...], ideas=[...], decisions=[...])\n\
            - result and validation must be nonblank; problems/ideas/decisions may be empty arrays.\n\
            - complete_task saves the report and closes the task in one atomic mutation;\n\
              it requires the task to be in review and never skips it.\n\
         5. Partial edits: patch_task (fields) and update_report (report sections,\n\
            replace|append). update_task and update_report_section are legacy\n\
            replacement-style APIs — prefer the typed operations.\n\
         6. Keep artifacts in English.",
        config.name,
        config.description,
        task.id,
        task.title,
        task.description,
        task.status.as_str(),
        acceptance,
        context,
        task.id,
        task.id,
        task.id
    )
}

impl Default for Jay {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl Jay {
    /// Creates the service, resolving the current project from the cwd.
    pub fn new() -> Self {
        let root = match project::resolve_context() {
            Ok(Context::Project(p)) => p,
            Ok(Context::Workspace(w)) => w,
            Err(_) => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        Self { root }
    }

    fn result_json<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// The project root for task operations (errors on a workspace).
    fn project_root(&self) -> Result<PathBuf, McpError> {
        match project::resolve_context_at(&self.root) {
            Ok(Context::Project(p)) => Ok(p),
            _ => Err(McpError::internal_error(
                "the MCP server's cwd is not a jay project folder",
                None,
            )),
        }
    }

    fn target_root(&self, dir: Option<&str>) -> Result<PathBuf, McpError> {
        match dir {
            Some(d) => {
                let p = PathBuf::from(d);
                let p = if p.is_absolute() {
                    p
                } else {
                    std::env::current_dir().map(|c| c.join(d)).unwrap_or(p)
                };
                match project::resolve_context_at(&p) {
                    Ok(Context::Project(r)) => Ok(r),
                    _ => Err(McpError::internal_error(
                        format!("{d} is not a jay project folder"),
                        None,
                    )),
                }
            }
            None => self.project_root(),
        }
    }

    // ===== context tools =====

    #[tool(
        description = "Initialize a project in a folder. Args: dir, name, description, goal, git_repo, branch_template, links"
    )]
    async fn init_project(
        &self,
        Parameters(args): Parameters<InitProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let dir = PathBuf::from(&args.dir);
        let dir = if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()
                .map(|c| c.join(&args.dir))
                .unwrap_or(dir)
        };
        project::init_project(&dir, Some(&args.name))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut cfg = project::load_config(&dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        cfg.description = args.description;
        cfg.goal = args.goal;
        cfg.git_repo = args.git_repo;
        cfg.branch_template = args.branch_template;
        cfg.links = args.links;
        project::save_config(&dir, &cfg)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::result_json(&cfg)
    }

    #[tool(description = "Resolve and describe the current project/workspace from the process cwd")]
    async fn current_project(&self) -> Result<CallToolResult, McpError> {
        let ctx = project::resolve_context()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let kind = if ctx.is_project() {
            "project"
        } else {
            "workspace"
        };
        let cfg = match &ctx {
            Context::Project(_) => Some(
                project::load_config(ctx.root())
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            ),
            Context::Workspace(_) => None,
        };
        let name = cfg
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| project::folder_name(ctx.root()));
        Self::result_json(&serde_json::json!({
            "kind": kind,
            "root": ctx.root().display().to_string(),
            "name": name,
            "config": cfg,
        }))
    }

    #[tool(
        description = "List projects in a workspace folder. Args: dir (optional, defaults to cwd)"
    )]
    async fn list_projects(
        &self,
        Parameters(args): Parameters<ListProjectsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let dir = match args.dir {
            Some(d) => {
                let p = PathBuf::from(&d);
                if p.is_absolute() {
                    p
                } else {
                    std::env::current_dir().map(|c| c.join(d)).unwrap_or(p)
                }
            }
            None => self.root.clone(),
        };
        let subs = project::project_subfolders(&dir);
        let mut rows = Vec::new();
        for sub in subs {
            let cfg = project::load_config(&sub)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let mut counts = serde_json::Map::new();
            for t in tasks::load_tasks(&sub)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                let key = t.status.as_str().to_string();
                *counts.entry(key).or_insert(serde_json::Value::from(0i64)) = {
                    let c = counts
                        .get(t.status.as_str())
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    serde_json::Value::from(c + 1)
                };
            }
            rows.push(serde_json::json!({ "name": cfg.name, "path": sub.display().to_string(), "counts": counts }));
        }
        Self::result_json(&rows)
    }

    #[tool(description = "Read the current project's config (.nest/config.toml)")]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
        let cfg = project::load_config(&self.root)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::result_json(&cfg)
    }

    #[tool(description = "Update a single config field. Args: field, value")]
    async fn update_config(
        &self,
        Parameters(args): Parameters<UpdateConfigArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut cfg = project::load_config(&self.root)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match args.field.as_str() {
            "name" => cfg.name = args.value,
            "description" => cfg.description = args.value,
            "goal" => cfg.goal = Some(args.value),
            "git_repo" => cfg.git_repo = Some(args.value),
            "branch_template" => cfg.branch_template = Some(args.value),
            "git_integration" => {
                cfg.git_integration =
                    Some(args.value.parse().map_err(|e: anyhow::Error| {
                        McpError::internal_error(e.to_string(), None)
                    })?)
            }
            "links" => {
                cfg.links = args
                    .value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            other => {
                return Err(McpError::internal_error(
                    format!("unknown config field: {other}"),
                    None,
                ))
            }
        }
        project::save_config(&self.root, &cfg)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::result_json(&cfg)
    }

    // ===== task tools =====

    #[tool(
        description = "List tasks in the current project (priority order). Args: dir (optional)"
    )]
    async fn list_tasks(
        &self,
        Parameters(args): Parameters<ListProjectsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let mut tasks =
            tasks::load_tasks(&root).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        tasks.sort_by(|a, b| {
            a.priority
                .rank()
                .cmp(&b.priority.rank())
                .then(a.id.cmp(&b.id))
        });
        Self::result_json(&tasks)
    }

    #[tool(
        description = "Ready work: open, unblocked tasks whose dependencies are all closed (cancelled prerequisites do NOT count), sorted by priority then id. CLI/MCP readiness decisions are identical. Args: dir (optional)"
    )]
    async fn next_tasks(
        &self,
        Parameters(args): Parameters<ListProjectsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let ready = service::ready_tasks(&root).map_err(mcp_err)?;
        Self::result_json(&ready)
    }

    #[tool(
        description = "Create a task (race-free id allocation). Args: title, description, priority, labels, estimates, deadline, links, acceptance, context, assignee, parent/epic/milestone, depends_on (validated project-local edges), dir (optional)"
    )]
    async fn create_task(
        &self,
        Parameters(args): Parameters<CreateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let priority = parse_priority(args.priority.as_deref())?;
        let t = service::create_task(&root, |id| {
            let mut t = Task::new(id, args.title, args.description);
            t.priority = priority;
            t.labels = args.labels;
            t.estimate_points = args.estimate_points;
            t.estimate_hours = args.estimate_hours;
            t.deadline = args.deadline;
            t.links = args.links;
            t.acceptance = args.acceptance;
            t.context = args.context;
            t.assignee = args.assignee;
            t.parent_id = args.parent_id;
            t.epic_id = args.epic_id;
            t.milestone_id = args.milestone_id;
            t.depends_on = args.depends_on;
            t.actor = Some("agent".to_string());
            t
        })
        .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(description = "Get a task by id. Args: id, dir (optional)")]
    async fn get_task(
        &self,
        Parameters(args): Parameters<GetTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let t = tasks::load_task(&root, args.id)
            .map_err(mcp_err)?
            .ok_or_else(|| McpError::internal_error(format!("task {} not found", args.id), None))?;
        Self::result_json(&t)
    }

    #[tool(
        description = "LEGACY replacement-style update: every editable field is replaced (omitted fields become defaults). Prefer patch_task for partial edits. Args: id + fields, dir (optional)"
    )]
    async fn update_task(
        &self,
        Parameters(args): Parameters<UpdateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let priority = parse_priority(args.priority.as_deref())?;
        let t = service::replace_task(
            &root,
            args.id,
            service::TaskReplace {
                title: args.title,
                description: args.description,
                acceptance: args.acceptance,
                context: args.context,
                assignee: args.assignee,
                priority,
                labels: args.labels,
                estimate_points: args.estimate_points,
                estimate_hours: args.estimate_hours,
                deadline: args.deadline,
                links: args.links,
                parent_id: args.parent_id,
                epic_id: args.epic_id,
                milestone_id: args.milestone_id,
            },
            "agent",
        )
        .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Partial task edit (PATCH semantics): omitted fields unchanged; empty lists clear lists; explicit null clears nullable fields; null for required fields rejected; status/timestamps/report fields are protected (use lifecycle and report operations). depends_on edges are validated (no duplicates/missing/self/cycles). Args: id, patch (object), actor (optional), dir (optional)"
    )]
    async fn patch_task(
        &self,
        Parameters(args): Parameters<PatchTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let patch = service::parse_task_patch_value(&args.patch).map_err(mcp_err)?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::patch_task(&root, args.id, patch, &format!("agent:{actor}"))
            .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Apply a state-machine action to a task. Args: id, action (start|review|done|rewind|cancel|reopen|block|unblock), reason (for block; mandatory nonblank for reopening a closed task), actor (optional), dir (optional). 'start' is refused while dependencies are unmet; 'reopen' works from cancelled AND closed (prior evidence is preserved in history and a new closure requires fresh result/validation); 'done' requires nonblank report result+validation; prefer complete_task to save the report and close atomically. Git/Forgejo side effects only run when git_integration is 'auto'; failures appear in git_diagnostics."
    )]
    async fn update_task_status(
        &self,
        Parameters(args): Parameters<UpdateTaskStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let action: TaskAction = args.action.parse().map_err(mcp_err)?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::apply_action(
            &root,
            args.id,
            action,
            &format!("agent:{actor}"),
            args.reason.as_deref(),
        )
        .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Typed report update: any subset of the five report sections in ONE call, with explicit mode. Omitted sections unchanged. replace (default): provided sections overwrite; append: text sections appended after a blank line, list sections appended in order. Args: id, mode (replace|append), result, validation, problems[], ideas[], decisions[], actor (optional), dir (optional)"
    )]
    async fn update_report(
        &self,
        Parameters(args): Parameters<UpdateReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let mode = match args.mode.as_deref() {
            Some(m) => m.parse::<service::ReportMode>().map_err(mcp_err)?,
            None => service::ReportMode::Replace,
        };
        let upd = service::ReportUpdate {
            result: args.result,
            validation: args.validation,
            problems: args.problems,
            ideas: args.ideas,
            decisions: args.decisions,
        };
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::update_report(&root, args.id, upd, mode, &format!("agent:{actor}"))
            .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "LEGACY single-section replacement (sections overwrite; list sections accept a JSON-array-encoded string). Prefer update_report (typed, all sections, replace|append) or complete_task. Args: id, section (result|validation|problems|ideas|decisions), content, actor (optional), dir (optional)"
    )]
    async fn update_report_section(
        &self,
        Parameters(args): Parameters<UpdateReportSectionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::update_report_section_legacy(
            &root,
            args.id,
            &args.section,
            &args.content,
            &format!("agent:{actor}"),
        )
        .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Save the five-section report and close a task already in review in ONE atomic local mutation. Review is never silently skipped; result and validation must be nonblank; on any validation failure nothing changes. Args: id, result, validation, problems[], ideas[], decisions[], actor (optional), dir (optional)"
    )]
    async fn complete_task(
        &self,
        Parameters(args): Parameters<CompleteTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let upd = service::ReportUpdate {
            result: args.result,
            validation: args.validation,
            problems: args.problems,
            ideas: args.ideas,
            decisions: args.decisions,
        };
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::complete_task(&root, args.id, upd, &format!("agent:{actor}"))
            .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Promote a reported idea into a new task. Args: task_id, idx (index into ideas), dir (optional)"
    )]
    async fn promote_idea(
        &self,
        Parameters(args): Parameters<PromoteIdeaArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let t = tasks::load_task(&root, args.task_id)
            .map_err(mcp_err)?
            .ok_or_else(|| {
                McpError::internal_error(format!("task {} not found", args.task_id), None)
            })?;
        let idea = t
            .report_ideas
            .get(args.idx as usize)
            .cloned()
            .ok_or_else(|| {
                McpError::internal_error(format!("idea index {} out of range", args.idx), None)
            })?;
        let nt = service::create_task(&root, |id| {
            let mut nt = Task::new(id, idea, String::new());
            nt.actor = Some("agent".to_string());
            nt
        })
        .map_err(mcp_err)?;
        Self::result_json(&nt)
    }

    #[tool(
        description = "Move a task to another project folder (new id; unknown fields preserved; both projects locked). Args: id, to_dir, dir (optional, source project)"
    )]
    async fn move_task(
        &self,
        Parameters(args): Parameters<MoveTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let dest = PathBuf::from(&args.to_dir);
        let dest = if dest.is_absolute() {
            dest
        } else {
            std::env::current_dir()
                .map(|c| c.join(&args.to_dir))
                .unwrap_or(dest)
        };
        let d = match project::resolve_context_at(&dest) {
            Ok(Context::Project(d)) => d,
            _ => {
                return Err(McpError::internal_error(
                    format!("{} is not a jay project folder", args.to_dir),
                    None,
                ))
            }
        };
        let moved = service::move_task(&root, args.id, &d, "agent").map_err(mcp_err)?;
        Self::result_json(&moved)
    }

    #[tool(
        description = "Create an explicit follow-up task linked via follow_up_of. The original task is left unchanged (stays closed); a normal open task is created with the supplied title; no dependency is implied unless requested separately. Args: task_id, title, description (optional), actor (optional), dir (optional)"
    )]
    async fn create_follow_up(
        &self,
        Parameters(args): Parameters<CreateFollowUpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let t = service::create_follow_up(
            &root,
            args.task_id,
            args.title,
            args.description,
            &format!("agent:{actor}"),
        )
        .map_err(mcp_err)?;
        Self::result_json(&t)
    }

    #[tool(
        description = "Spawn a sub-agent for a task. Args: task_id, command (optional; default dsh --profile headless), dir (optional)"
    )]
    async fn dedicate_agent(
        &self,
        Parameters(args): Parameters<DedicateAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let task = tasks::load_task(&root, args.task_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("task {} not found", args.task_id), None)
            })?;
        let config = project::load_config(&root)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let prompt = build_prompt(&task, &config);

        let mut cmd = match &args.command {
            Some(command_line) => {
                let parts: Vec<&str> = command_line.split_whitespace().collect();
                if parts.is_empty() {
                    return Err(McpError::internal_error("empty command", None));
                }
                let mut c = tokio::process::Command::new(parts[0]);
                c.args(&parts[1..]);
                c
            }
            None => {
                let mut c = tokio::process::Command::new("dsh");
                c.args(["--profile", "headless"]);
                c
            }
        };
        cmd.arg(&prompt);

        let out = cmd
            .output()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(-1);
        let text = format!("exit={code}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    // ===== knowledge base tools =====

    #[tool(
        description = "Get the authoritative current project summary: the explicitly designated status entry plus provenance and freshness facts, or the highest-id historical entry labeled 'latest historical update; no current summary designated'. Entries are never promoted automatically by a 'current' tag. Args: dir (optional)"
    )]
    async fn get_project_status(
        &self,
        Parameters(args): Parameters<GetProjectStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        match knowledge::current_status(&root).map_err(mcp_err)? {
            Some(cs) => {
                let freshness = knowledge::summary_freshness(&root, &cs).map_err(mcp_err)?;
                Self::result_json(&serde_json::json!({
                    "entry": cs.entry,
                    "designated": cs.designated,
                    "designation": cs.designation,
                    "note": cs.note,
                    "freshness": freshness,
                }))
            }
            None => Ok(CallToolResult::success(vec![ContentBlock::text(
                "no status recorded",
            )])),
        }
    }

    #[tool(
        description = "Designate an existing status entry as the authoritative current summary. Requires meaningful content; records actor and time; accepts an optional git commit reference. Historical entries are preserved. Args: id, commit (optional), actor (optional), dir (optional)"
    )]
    async fn set_current_summary(
        &self,
        Parameters(args): Parameters<SetCurrentSummaryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let cs =
            knowledge::set_current_summary(&root, args.id, &format!("agent:{actor}"), args.commit)
                .map_err(mcp_err)?;
        Self::result_json(&cs)
    }

    #[tool(
        description = "Combined project context: current summary (provenance + freshness), task counts, active tasks, blocked tasks and ready work. Identical payload to 'jay status --json'. Args: dir (optional)"
    )]
    async fn project_context(
        &self,
        Parameters(args): Parameters<GetProjectStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let ctx = service::project_context(&root).map_err(mcp_err)?;
        Self::result_json(&ctx)
    }

    #[tool(
        description = "List knowledge base entries. Args: kind (decision|status|note, optional), tag (optional filter), dir (optional)"
    )]
    async fn list_kb(
        &self,
        Parameters(args): Parameters<ListKbArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let kind = match args.kind.as_deref() {
            Some(k) => Some(
                k.parse::<KnowledgeKind>()
                    .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None))?,
            ),
            None => None,
        };
        let entries = knowledge::filter_entries(&root, kind, args.tag.as_deref())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::result_json(&entries)
    }

    #[tool(
        description = "Get a single knowledge base entry by id and kind. Args: id, kind (decision|status|note), dir (optional)"
    )]
    async fn get_kb(
        &self,
        Parameters(args): Parameters<GetKbArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let kind = args
            .kind
            .parse::<KnowledgeKind>()
            .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None))?;
        let entry = knowledge::get_entry(&root, kind, args.id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| {
                McpError::internal_error(
                    format!("knowledge entry {} ({}) not found", args.id, kind.as_str()),
                    None,
                )
            })?;
        Self::result_json(&entry)
    }

    #[tool(
        description = "Add a knowledge base entry. Args: kind (decision|status|note), title, content, tags (optional), related_task (optional), supersedes (optional, validated), commit (optional), set_current (status only: designate atomically), actor (optional), dir (optional)"
    )]
    async fn add_kb(
        &self,
        Parameters(args): Parameters<AddKbArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let kind = args
            .kind
            .parse::<KnowledgeKind>()
            .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None))?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let mut entry = KnowledgeEntry::new(0, kind, args.title, args.content);
        entry.tags = args.tags;
        entry.related_task = args.related_task;
        entry.supersedes = args.supersedes;
        entry.commit = args.commit;
        entry.actor = Some(format!("agent:{actor}"));
        if kind == KnowledgeKind::Status {
            let (saved, cs) = knowledge::add_status_entry(
                &root,
                entry,
                args.set_current,
                &format!("agent:{actor}"),
            )
            .map_err(mcp_err)?;
            return Self::result_json(&serde_json::json!({
                "entry": saved,
                "current_summary": cs,
            }));
        }
        if args.set_current {
            return Err(McpError::internal_error(
                "set_current only applies to status entries",
                None,
            ));
        }
        let saved = knowledge::add_entry(&root, entry).map_err(mcp_err)?;
        Self::result_json(&saved)
    }

    #[tool(
        description = "Update a knowledge base entry. Args: id, kind (decision|status|note), title (optional), content (optional), tags (optional), related_task (optional), supersedes (optional, validated), actor (optional), dir (optional)"
    )]
    async fn update_kb(
        &self,
        Parameters(args): Parameters<UpdateKbArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let kind = args
            .kind
            .parse::<KnowledgeKind>()
            .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None))?;
        let actor = args.actor.unwrap_or_else(|| "agent".to_string());
        let updated = knowledge::update_entry(&root, kind, args.id, |entry| {
            if let Some(t) = args.title {
                entry.title = t;
            }
            if let Some(c) = args.content {
                entry.content = c;
            }
            if let Some(tags) = args.tags {
                entry.tags = tags;
            }
            if let Some(rt) = args.related_task {
                entry.related_task = Some(rt);
            }
            if let Some(s) = args.supersedes {
                entry.supersedes = s;
            }
            entry.actor = Some(format!("agent:{actor}"));
        })
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::result_json(&updated)
    }

    #[tool(
        description = "Search the knowledge base with BM25 full-text ranking. Args: query, kind (optional filter), limit (optional, default 10), dir (optional)"
    )]
    async fn search_kb(
        &self,
        Parameters(args): Parameters<SearchKbArgs>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.target_root(args.dir.as_deref())?;
        let kind = match args.kind.as_deref() {
            Some(k) => Some(
                k.parse::<KnowledgeKind>()
                    .map_err(|e: anyhow::Error| McpError::internal_error(e.to_string(), None))?,
            ),
            None => None,
        };
        let limit = args.limit.unwrap_or(10);
        let results = knowledge::search(&root, &args.query, kind)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let results: Vec<_> = results.into_iter().take(limit).collect();
        let current_id = knowledge::current_summary_ref(&root)
            .map_err(mcp_err)?
            .map(|cs| cs.entry_id);
        Self::result_json(&serde_json::json!({
            "query": args.query,
            "current_summary_entry_id": current_id,
            "results": results.iter().map(|r| serde_json::json!({
                "score": r.score,
                "entry": r.entry,
                "is_current_summary": current_id == Some(r.entry.id) && r.entry.kind == KnowledgeKind::Status,
            })).collect::<Vec<_>>(),
        }))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for Jay {}

/// Runs the MCP stdio server until the client disconnects.
pub async fn run() -> anyhow::Result<()> {
    let service = Jay::new()
        .serve(stdio())
        .await
        .inspect_err(|e| eprintln!("error starting MCP server: {e}"))?;
    service.waiting().await?;
    Ok(())
}
