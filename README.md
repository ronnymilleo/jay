# Jay

Terminal project manager, git-like: one `.nest/` folder per project.

<figure align="center">
  <img src="https://github.com/user-attachments/assets/ad4e1b4e-2df7-4f86-8d0e-ca82b99ee85a" alt="Gralha-Azul — mascot of Jay">
  <figcaption>
    <em>AI-Generated image, if you are a real artist and want to contribute with your artwork, I'll be happy to replace it.</em>
  </figcaption>
</figure>

## What is jay?

**jay** is named after the azure jay (*Cyanocorax caeruleus*, the
gralha-azul) — a corvid from southern Brazil and the state bird of Paraná.
Jays are clever, loud, and obsessed with storing food for later. The azure
jay takes it further: it buries thousands of araucaria seeds every year and
forgets where it hid some of them. Those forgotten seeds grow into entire
forests.

That is the idea behind this tool. Tasks, decisions, and project knowledge
are seeds: jay helps you stash them in one place — the `.nest/` folder — and
even the ones you "forget" keep working for you, because everything is
plain-text TOML, versioned with git, and readable by both humans and AI
agents. Years later, `git log` shows exactly what grew, when, and why.

A jay also never wastes a trip: it carries food, tools, and gossip between
trees. Here, the CLI and the MCP server share the same data, so your
terminal sessions and your coding agents always see the same forest.

## Concept

- **Project = folder.** `jay init` in a folder turns it into a project: a
  `.nest/` directory holds the config and the tasks (one TOML file per task).
  No "registering projects" — you just cd into the folder.
- **Task = contract with a schema**: whoever works on it (a human via the CLI,
  or an agent via MCP) fills in every field and reports through the standard
  sections (result, validation, problems, ideas, decisions). Closing a task
  requires nonblank result and validation — jay never fabricates evidence.
- **Plain-text data**: tasks are TOML files committed with the project, so
  `git diff`/`log`/`merge` work on your data — and it syncs between
  machines with the repo. Writes are atomic; unknown/legacy fields in your
  files are preserved, never silently dropped.
- **MCP server** = the orchestrator agent's interface (same data the CLI sees).
  CLI and MCP share one mutation layer (`src/service.rs`), so they cannot
  disagree about validation, readiness or completion rules.
- **Workspace** = any folder whose subfolders are projects (no marker of its
  own); it lists those projects.
- `dedicate_agent` is an OPTIONAL extra: it spawns a sub-agent (default
  `dsh --profile headless`) with a standardized prompt.
- **Old reports and summaries are claims recorded at a point in time** — not
  independent verification of the underlying program.

## Status

Working: project = folder (`.nest/`), CLI (init/current/config/status/next/
sync/doctor/repair + task/project/kb subcommands), MCP server (context, task,
patch/report/complete, KB, readiness and project-context tools), explicit
git integration (`off` by default; `auto` records structured diagnostics),
structured dependencies, current-summary designation with freshness facts,
reopen-with-history and follow-ups. The interface is CLI and MCP; no TUI is included.

Version 0.1.0 is the first public-history baseline. Use it in daily work for
at least one week, record improvements and bugs, and assess compatibility
stability before deciding to release 1.0.0.

## Structure

- src/project.rs — resolution, init and config (the `.nest/` folder), atomic writes
- src/tasks.rs — plain-text persistence (one TOML file per task)
- src/model.rs — Task / Milestone / KnowledgeEntry / status / priority / lifecycle events
- src/service.rs — shared mutation service: project lock, patch, typed report, complete, follow-up, moves
- src/diag.rs — doctor findings (stable codes, severities) and explicit repair
- src/knowledge.rs — KB persistence, current summary, supersession, freshness, BM25 search
- src/cli.rs — quick commands (clap)
- src/mcp.rs — MCP server (rmcp, stdio)
- src/gitflow.rs — optional branch/PR automation (only when `git_integration = "auto"`)
- src/forgejo.rs — Forgejo API client
- docs/ — setup and CLI tutorial
- tests/integration.rs — end-to-end workflow scenarios

## How to run

    cargo build --locked       # binary at target/debug/jay
    cargo install --path . --locked  # install jay into ~/.cargo/bin

Validation:

    cargo test --locked
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

Common commands:

    jay init [name]            # make this folder a project
    jay current                # print the resolved context
    jay status [--json]        # overview + ready work + current summary/freshness
    jay next [--json]          # ready work: open, unblocked, dependencies closed
    jay config [field] [value] # read/edit .nest/config.toml
    jay sync                   # git pull+push
    jay doctor [--json]        # validate; exit 0 clean / 1 integrity errors / 2 runtime
    jay repair [--apply]       # explicit safe repairs (dry run by default, backups on apply)
    jay mcp                    # MCP server over stdio (for agents)

    jay task new <title> [--priority ...] [--label ...] [--estimate-points ...] [--depends-on <id>...]
    jay task list              # list tasks (alias: ls)
    jay task show <id>         # full detail + report + dependencies + history
    jay task find <term> [--priority ...] [--label ...] [--status ...]
    jay task start <id>        # open -> started (alias: do; refused while deps unmet)
    jay task review <id>       # started -> review
    jay task done <id>         # review -> closed (requires result + validation)
    jay task complete <id> --report-file <json>   # save report + close atomically
    jay task report <id> --report-file <json> [--append]  # typed 5-section update
    jay task edit <id> --patch-file <json>        # partial edit (null clears nullable fields)
    jay task rewind <id>       # one step back
    jay task block <id> [--reason ...] / jay task unblock <id>
    jay task cancel <id>
    jay task reopen <id> [--reason ...]   # cancelled -> open; closed -> open needs a reason
    jay task follow-up <id> --title <t> [--description <d>]  # linked follow-up task
    jay task move <id> --to <dir>

    jay project list           # list projects in a workspace (alias: ls)

    jay kb status              # current summary + provenance + freshness facts
    jay kb set-current <id> [--commit <ref>]      # designate the authoritative summary
    jay kb add --kind status --title ... --content ... [--set-current] [--supersedes <id>...] [--commit <ref>]
    jay kb list / show / edit / search           # see docs/CLI-TUTORIAL.md

## Data layout

    .nest/
      config.toml     # name, description, goal, git_repo, branch_template,
                      # git_integration ("off"|"auto"), links
      tasks/
        1.toml        # one task per file (id = file name)
        2.toml
      milestones.toml # a [[milestones]] list
      kb/             # decisions.toml, status.toml (incl. current_summary), notes.toml
      backups/        # recoverable backups created by `jay repair --apply`

Everything is committed with the project — it is how data travels between
machines. The persistent, OS-locked `.nest/lock` file serializes task and KB
writers; ownership releases automatically on process exit. Ignore this file in
Git and leave it in place. Building requires Rust 1.89 or newer. See
[locking and upgrade notes](docs/SETUP.md).

## MCP tools

- context: init_project / current_project / list_projects / get_config / update_config / project_context
- tasks: list_tasks / create_task / get_task / patch_task / update_task_status / move_task / next_tasks / create_follow_up
- report: update_report (typed, replace|append) / complete_task (report + close, atomic) / promote_idea
- knowledge: get_project_status / set_current_summary / list_kb / get_kb / add_kb / update_kb / search_kb
- legacy (documented replacement semantics): update_task / update_report_section
- extra: dedicate_agent

## Status lifecycle

open -> started -> review -> closed. cancelled is a side exit. blocked is a
flag (with a reason) that pauses transitions until unblock. start records
started_at; close records done_at and requires nonblank report result and
validation. Every transition records a lifecycle event (actor, RFC 3339
timestamp with offset, old/new state, reason) in the task's `history`.

`closed` tasks can be reopened: `reopen` (CLI `--reason`, MCP `reason`) works
from cancelled AND from closed — reopening a closed task requires a nonblank
reason, preserves the prior completion timestamp, report and integration
links as a history snapshot, and clears the active cycle so a new closure
needs fresh evidence. Explicit follow-ups (`create_follow_up` / `jay task
follow-up`) link a new open task via `follow_up_of` and leave the original
closed record untouched.

Git integration is explicit: `git_integration = "off"` (default for new
projects) performs no Git/Forgejo operations and emits no missing-config
noise; `"auto"` creates branches on start and opens PRs on review
(FORGEJO_TOKEN/FORGEJO_URL — see docs/SETUP.md), recording failures as
structured `git_diagnostics` on the task (never as report problems) and
never recording a branch/PR link for a failed operation. Legacy configs
without the field keep automation only when `git_repo` or `branch_template`
is configured.

## Dependencies and readiness

Tasks carry structured `depends_on` ids (project-local, validated: no
duplicates, missing ids, self-dependency or cycles). `jay next` / MCP
`next_tasks` list open, unblocked tasks whose dependencies are all closed
(cancelled does not count). Starting with unmet dependencies is refused;
cross-project moves are refused while dependency or follow-up edges would
break. Prose dependencies in `context` are never parsed automatically — an
agent reviews the text and adds explicit edges via `patch_task`.

## Example workspace

The `examples/workspace` directory contains two sample projects, alpha and beta.
Their `.nest` files are intentionally included as example data.

    cd examples/workspace
    jay project list
    jay current
    cd alpha
    jay task list

For full instructions, see [CLI tutorial](docs/CLI-TUTORIAL.md) and
[setup and MCP integration](docs/SETUP.md).

## License

Jay is licensed under the [MIT License](LICENCE).
