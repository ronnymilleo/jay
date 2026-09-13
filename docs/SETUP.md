# Setup — running the jay MCP server for DSH agents

## Build

    cargo build             # debug binary at target/debug/jay
    # or: cargo build --release    # target/release/jay

## Run modes

- `jay <cmd>` — quick CLI: init/current/config/status/next/doctor/repair/
  sync + task/project/kb subcommands (aliases: `st` = status, `do` = start)
- `jay mcp` — MCP server over stdio (for agents)

Exit codes: 0 = success; `jay doctor` additionally uses 1 for integrity
errors (warnings alone exit 0); 2 = invocation/runtime failure.

## Register as an MCP server in DSH

The MCP server resolves the current project from its process working directory
(`cwd`), so point cwd at the project folder.

Add to ~/.dsh/profiles/web/cordis.patch.yml (replace paths for your machine):

    - insert:
        - id: mcp-jay
          name: '@deepseek-ai/dsh-mcp-client'
          config:
            serverName: jay
            transport: stdio
            command: /abs/path/to/jay
            args: ['mcp']
            cwd: /abs/path/to/the/project   # the project folder (has .nest/)
            toolCallTimeoutMs: 600000

The tools appear to agents as `mcp__jay__*`:

- Context: init_project, current_project, list_projects, get_config,
  update_config, project_context
- Tasks: list_tasks, create_task, get_task, patch_task, update_task_status,
  move_task, next_tasks, create_follow_up
- Report/promotion: update_report (typed, replace|append), complete_task
  (report + close, atomic), promote_idea
- Knowledge: get_project_status, set_current_summary, list_kb, get_kb,
  add_kb, update_kb, search_kb
- Legacy (documented replacement semantics — prefer the typed operations):
  update_task, update_report_section
- Extra: dedicate_agent

Restart the harness after changing it.

Sub-agents (`dedicate_agent` spawns `dsh --profile headless`) need the same
entry in ~/.dsh/profiles/headless/cordis.patch.yml so they can report back via MCP.

## MCP payloads for the standard agent workflow

Deliver a task (start -> review -> typed completion):

    update_task_status  {"id": 7, "action": "start"}
    ... do the work ...
    update_task_status  {"id": 7, "action": "review"}
    complete_task       {"id": 7,
                         "result": "what was delivered",
                         "validation": "how it was verified",
                         "problems": ["..."], "ideas": [], "decisions": ["..."]}

`complete_task` requires the task to be in review; result and validation must
be nonblank; on any validation failure nothing changes.

Partial edit (omitted fields unchanged; null clears nullable fields):

    patch_task          {"id": 7, "patch": {"description": "new scope",
                                            "labels": ["bug"],
                                            "assignee": null}}

Protected fields (status, timestamps, actor, report sections, history,
follow_up_of) are rejected in patches, never silently dropped.

Incremental report writing (one call, any subset of sections):

    update_report       {"id": 7, "mode": "append",
                         "result": "additional finding",
                         "problems": ["one more problem"]}

Structured dependencies and readiness:

    create_task         {"title": "Stage 2", "depends_on": [7]}
    next_tasks          {}        # open, unblocked, deps all closed

Authoritative summary:

    add_kb              {"kind": "status", "title": "v1 delivered",
                         "content": "...", "set_current": true,
                         "commit": "abc1234", "supersedes": [2]}
    set_current_summary {"id": 3, "commit": "abc1234"}
    get_project_status  {}        # designated summary + provenance + freshness
    project_context     {}        # summary + counts + active/blocked + ready

Reopen and follow-up:

    update_task_status  {"id": 7, "action": "reopen",
                         "reason": "review found a regression"}
    create_follow_up    {"task_id": 7, "title": "Harden edge cases"}

## Data

Project data is plain text in `.nest/` (config.toml + tasks/*.toml +
milestones.toml + kb/*.toml), committed with the project's git repo. Sync
between machines = git pull/push (`jay sync`). There is no central database.

Task and KB mutations and applied repairs use an OS-backed advisory lock on
`.nest/lock`. The empty file persists; ownership is released immediately on
close or process exit, including crashes. Do not delete it while Jay is running.
New projects ignore it automatically; existing projects should add `.nest/lock`
to their root `.gitignore`. This requires Rust 1.89 or newer
and a filesystem supporting OS file locks; no age-based lock breaking is used.

File writes use same-directory temporary files and atomic rename. Unknown task
and KB fields are preserved across mutations, including KB document, summary,
and entry extensions. Repair preserves unknown fields too; `jay doctor` reports
them as warnings.

## Integrity checking and repair

    jay doctor [--json]   # findings with stable codes, severities, suggestions
    jay repair            # dry run: what would change
    jay repair --apply    # apply: backups under .nest/backups/repair-<ts>/

Recovery after a repair: copy the backed-up file over the repaired one
(under `.nest/backups/repair-<timestamp>/<original relative path>`). Repair
never fabricates completion evidence, timestamps or actors — it only performs
documented safe migrations (e.g. legacy `closed_at` -> `done_at` when absent
or equivalent) and reports everything it cannot fix.

## Git/Forgejo automation (explicit)

Automation is controlled per project by `git_integration` in
`.nest/config.toml`:

- `off` (default for new projects): start/review perform no Git/Forgejo
  operations, emit no missing-configuration diagnostics, and never touch
  your links. Manually attached links (branch:, PR URLs, docs) remain usable.
- `auto`: starting a task creates the branch from `branch_template`; moving
  to review opens a real pull request via the Forgejo API. Failures become
  structured `git_diagnostics` entries on the task (operation, message,
  time), separate from `report_problems`; they never undo a successful
  transition. A branch link is only recorded when the branch was actually
  created, and only real PR URLs are recorded (the old `pr:<branch>`
  placeholder is no longer produced).

Reopening creates a new branch name by appending `-cycle-2`, `-cycle-3`, etc.
to the configured template's rendered name. Prior branch/PR links stay in the
reopen history snapshot, while the active cycle receives its own links.

Configure the Forgejo API with environment variables:

    export FORGEJO_TOKEN="your-api-token"
    export FORGEJO_URL="https://forgejo.example.com"

Both variables are required for automatic pull requests.

The project's `git_repo` defaults to the folder's remote origin URL (detected
on init); set `branch_template` for automatic branch names. Legacy configs
without `git_integration` keep automation only when `git_repo` or
`branch_template` is configured and nonblank; otherwise they resolve to `off`.
