# jay CLI — Step-by-step Tutorial

jay is a git-like terminal project manager: each project is a folder with a
`.nest/` directory holding plain-text tasks. This tutorial walks through the CLI.

## 0. Build

    cargo build            # binary at target/debug/jay
    alias jay="$PWD/target/debug/jay"

Run any command with `--help` to see its options.

## 1. Init a project

    mkdir website && cd website
    jay init               # this folder is now a project
    # initialized project: website

`jay init` creates `.nest/` (config.toml + tasks/ + kb/ + milestones.toml).
The project name defaults to the folder name; pass a name to override:

    jay init "My Site"

New projects have `git_integration = "off"`: no branch/PR automation and no
missing-configuration noise. See §14 to opt in.

## 2. Create tasks

    jay task new "Write the landing page"
    jay task new "Fix the checkout crash" --priority high --label bug --estimate-points 5 --deadline 2026-09-01

Tasks are stored as `.nest/tasks/1.toml`, `.nest/tasks/2.toml`, ... (the id is
the file name). New tasks start in `open`. `--label` can repeat; `--deadline`
is an ISO date. Open ideas need no completion evidence — result/validation are
only required to *close* a task.

## 3. List tasks

    jay task list
    # 1   open        Write the landing page
    # 2   open        Fix the checkout crash

Ordered by priority (then id). Alias: `jay task ls`.

## 4. Status overview and ready work

    jay status             # counts + active + blocked + ready + current summary
    jay status --json      # machine-readable project context (same payload as MCP project_context)
    jay next               # ready work only: open, unblocked, dependencies closed
    jay next --json

`jay status` is read-only, like `git status`.

## 5. The state machine

Tasks move through a fixed order: `open -> started -> review -> closed`.

    jay task start 2      # open -> started (alias: do)
    jay task review 2     # started -> review

Transitions are validated — each action requires the expected state, and
**closing requires completion evidence** (nonblank report result and
validation):

    jay task done 2       # error: closing requires completion evidence ...

Every transition records a lifecycle event (actor, RFC 3339 timestamp with
offset, old/new state, reason) in the task's `history`.

## 6. Reports and atomic completion

Write the five report sections and close in one atomic step (`-` reads stdin):

    cat > report.json <<'JSON'
    {
      "result": "Landing page implemented with the new hero section",
      "validation": "make test passes; rendered states inspected",
      "problems": ["mobile nav needs follow-up"],
      "ideas": ["A/B test the hero copy"],
      "decisions": ["kept the existing CSS framework"]
    }
    JSON

    jay task complete 2 --report-file report.json
    # task 2 completed (report saved, status: closed)

`complete` requires the task to be in review — review is never silently
skipped. If validation fails, nothing changes (the file keeps its original
bytes). problems/ideas/decisions may be empty arrays; result and validation
must be nonblank.

To update the report without closing:

    jay task report 2 --report-file report.json            # replace provided sections
    jay task report 2 --report-file more.json --append     # append (text separated by a blank line, lists in order)

Omitted sections stay unchanged. Lists are real JSON arrays — never
JSON-encoded strings.

## 7. Partial edits

    echo '{"description": "updated scope", "assignee": null, "labels": []}' > patch.json
    jay task edit 2 --patch-file patch.json

PATCH semantics: omitted fields unchanged; empty lists clear lists; explicit
`null` clears nullable fields (assignee, deadline, estimates, parent/epic/
milestone); `null` for required fields (title, description) is rejected.
Status, timestamps, actor, report sections, history and follow-up links are
**protected** — they are controlled by lifecycle/report operations only, and
patches touching them are rejected, not ignored.

## 8. Block / unblock

    jay task block 2 --reason "waiting on hardware"
    jay task unblock 2

A blocked task can't transition until unblocked. `blocked` is a flag, not a
status — the task keeps its state, and blocked tasks never appear in
`jay next`.

## 9. Dependencies

    jay task new "Deploy to prod" --depends-on 2 --depends-on 3
    echo '{"depends_on": [2]}' > dep.json && jay task edit 4 --patch-file dep.json

Dependencies are project-local ids, validated on create/edit: no duplicates,
no missing ids, no self-dependency, no cycles. Effects:

- `jay next` only lists tasks whose dependencies are all **closed**
  (a cancelled prerequisite does NOT count as completed);
- `jay task start` is refused while dependencies are unmet, returning the ids;
- `jay task show <id>` explains each unmet dependency;
- moving a task across projects is refused while dependency edges exist —
  remove them explicitly first.

Prose dependencies ("Depends on #20" in `context`) are never parsed
automatically; review the text and add explicit edges.

## 10. Rewind, cancel, reopen, follow-ups

    jay task rewind 2     # review -> started -> open (one step back)
    jay task cancel 2     # any non-closed -> cancelled
    jay task reopen 2     # cancelled -> open (reason optional)

`closed` is no longer terminal — reopening a closed task requires a reason:

    jay task reopen 2 --reason "review found a regression"

Reopening preserves the prior completion timestamp and report (including that
cycle's branch/PR links) as a snapshot in `history`, then clears the active
cycle (`started_at`, `done_at`, result, validation) so a new closure requires
fresh evidence. Problems/ideas/decisions and manual links are kept. The old
cycle's PR is never reused as the new cycle's review result.

For "more work like this" prefer an explicit follow-up — the original stays
closed and the new task is a normal open task (no implied dependency):

    jay task follow-up 2 --title "Harden the checkout flow" --description "..."

## 11. Show a task

    jay task show 2       # detail + report + dependencies + git diagnostics + history

## 12. Find (fuzzy)

    jay task find checkout        # matches "Fix the checkout crash"
    jay task find fxchk           # fuzzy subsequence (case-insensitive)
    jay task find --priority high
    jay task find --label bug
    jay task find crash --status open

## 13. Move a task to another project

    jay task move 2 --to ../other-project

The task gets a new id in the destination. Moves are refused while
dependency or follow-up references would break (remove those edges first).

## 14. Config and git integration

    jay config                       # print .nest/config.toml
    jay config branch_template "feat/{task-id}-{slug}"
    jay config git_integration auto  # opt in to branch/PR automation
    jay config --show-origin         # where each value comes from (env/local/default)

`git_integration`:

- `off` (default for new projects): start/review perform no Git/Forgejo
  operations and emit no diagnostics. Manually attached links still work.
- `auto`: start creates the branch from `branch_template`; review opens a
  Forgejo PR (needs FORGEJO_TOKEN and FORGEJO_URL — see SETUP.md). Failures are recorded as
  structured `git_diagnostics` on the task (operation, message, time) —
  separate from report problems — and a branch/PR link is only recorded when
  the operation actually succeeded. Integration failure never undoes a
  successful transition.
- Legacy configs without the field keep automation only when `git_repo` or
  `branch_template` is configured.

## 15. Knowledge base and the current summary

    jay kb add --kind decision --title "Use TOML" --content "diffable" --tag format
    jay kb add --kind status --title "v1 delivered" --content "..." --set-current --commit $(git rev-parse --short HEAD)
    jay kb status            # the designated summary + provenance + freshness facts
    jay kb set-current 3     # designate an existing status entry as authoritative
    jay kb list / show / edit / search

Historical status entries are kept forever; `set-current` designates which
one is authoritative (records actor/time, optional commit). Entries are never
promoted automatically because they carry a `current` tag. When nothing is
designated, `kb status` falls back to the highest-id entry labeled "latest
historical update; no current summary designated". Freshness facts (tasks/KB
updated after the summary, recorded commit vs local HEAD) are presented as
evidence, never as a verdict on the prose. New status entries can declare
`--supersedes <id>` (validated; superseded text is preserved).

## 16. Doctor and repair

    jay doctor             # human-readable findings
    jay doctor --json      # machine-readable findings
    # exit codes: 0 = no integrity errors (warnings allowed), 1 = integrity
    # errors, 2 = invocation/runtime failure

Every config/task/milestone/KB file is inspected independently with stable
codes (e.g. `task.closed_missing_done_at`, `task.closed_at_conflict`,
`kb.legacy_current_tag`). A malformed file never hides findings in later
files; warnings alone do not fail.

    jay repair             # dry run: list proposed changes (default)
    jay repair --apply     # apply: backups under .nest/backups/, atomic writes

Repair is explicit, safe and idempotent: it currently migrates a valid legacy
`closed_at` to `done_at` (only when absent or equivalent — conflicts stay
actionable errors), preserving every other field including unknown ones. It
never fabricates result, validation, timestamps or actors — missing evidence
requires an explicit update from you or an agent. Recovery: copy the backup
file back over the repaired one.

## 17. Sync & workspace

    jay sync       # git pull + git push (commit local changes first)

    mkdir work && cd work
    mkdir proj-a && cd proj-a && jay init && cd ..
    mkdir proj-b && cd proj-b && jay init && cd ..
    jay project list   # lists proj-a and proj-b with task counts

A workspace is just a folder whose subfolders are projects — no marker or
init needed.

## Quick reference

| Command | Alias | Purpose |
|---------|-------|---------|
| jay init [name] | — | make this folder a project |
| jay status [--json] | st | overview + ready + current summary |
| jay next [--json] | — | ready work (deps closed, unblocked, open) |
| jay current | — | print the resolved context |
| jay config [field] [value] | — | read/edit config |
| jay doctor [--json] | — | validate (exit 0/1/2) |
| jay repair [--apply] | — | explicit safe repairs (backups on apply) |
| jay task new <title> [flags] | — | create a task (--depends-on repeatable) |
| jay task list | task ls | list tasks |
| jay task show <id> | — | full detail + history |
| jay task find <term> [flags] | — | fuzzy search + filters |
| jay task start <id> | task do | open -> started (deps must be closed) |
| jay task review <id> | — | started -> review |
| jay task done <id> | — | review -> closed (needs evidence) |
| jay task complete <id> --report-file <json> | — | report + close atomically |
| jay task report <id> --report-file <json> [--append] | — | typed 5-section update |
| jay task edit <id> --patch-file <json> | — | partial edit |
| jay task rewind <id> | — | one step back |
| jay task block <id> [--reason] / unblock <id> | — | blocked flag |
| jay task cancel <id> | — | -> cancelled |
| jay task reopen <id> [--reason] | — | cancelled/closed -> open |
| jay task follow-up <id> --title <t> | — | linked follow-up task |
| jay task move <id> --to <dir> | — | move a task (guarded) |
| jay kb status / set-current / add / list / show / edit / search | — | knowledge base |
| jay project list | project ls | list projects (workspace) |
| jay sync | — | git pull+push |

Statuses: open, started, review, closed, cancelled (+ blocked flag).
Priorities: highest, high, medium, low, lowest (or 0..4).
Every command accepts `--json` where applicable.

Reopened tasks use a new branch name in automatic Git mode: the rendered
branch template gains `-cycle-2`, then `-cycle-3`, and so on. Old links remain
in history. Applied repair reports are based on a fresh inspection after the
migration; dry runs report the existing errors without changing files.
