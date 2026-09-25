//! End-to-end workflow tests under temporary directories.
//!
//! Workflow scenarios:
//! 1. Psotool-sized project: one fully reported local delivery, three future
//!    tasks, no Git automation noise, and a current summary.
//! 2. Siggen-sized project: dependency chain, deferred tasks, revised
//!    summaries, batch report completion, reopened review correction, and
//!    linked follow-up.
//! 3. Legacy records: unknown `closed_at`, absent evidence, contradictory
//!    current tags, no new fields; read-only diagnosis and explicit safe
//!    repair.
//! 4. CLI/MCP parity for edits, reporting, completion, readiness, current
//!    context and reopen; inspects MCP schemas as well as shared service
//!    behavior.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use jay::model::{KnowledgeEntry, KnowledgeKind, Task, TaskStatus};
use jay::project::{init_project, GitIntegration};
use jay::service::{self, ReportUpdate};
use jay::{diag, knowledge, project, tasks};

fn jay_bin() -> &'static str {
    env!("CARGO_BIN_EXE_jay")
}

struct Proj(tempfile::TempDir);

impl Proj {
    fn new(_name: &str) -> Self {
        let base = tempfile::tempdir().unwrap();
        init_project(base.path(), None).unwrap();
        // keep tests deterministic: no forgejo; integration defaults to off
        std::env::remove_var("FORGEJO_TOKEN");
        Proj(base)
    }

    fn root(&self) -> &Path {
        self.0.path()
    }
}

fn run_jay(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(jay_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn report(result: &str, validation: &str) -> ReportUpdate {
    serde_json::from_str(&format!(
        r#"{{"result":"{result}","validation":"{validation}","problems":[],"ideas":[],"decisions":[]}}"#
    ))
    .unwrap()
}

fn walk_to_review(root: &Path, id: i64) {
    service::apply_action(root, id, jay::model::TaskAction::Start, "human", None).unwrap();
    service::apply_action(root, id, jay::model::TaskAction::Review, "human", None).unwrap();
}

#[test]
fn cli_agent_actor_and_local_completion_are_recorded() {
    let proj = Proj::new("agent-workflow");
    let root = proj.root();
    let task = service::create_task(root, |id| {
        Task::new(id, "Deliver feature".into(), String::new())
    })
    .unwrap();
    let report_path = root.join("report.json");
    std::fs::write(
        &report_path,
        r#"{"result":"Delivered","validation":"Tests passed"}"#,
    )
    .unwrap();
    let path = report_path.to_str().unwrap();

    let (code, _, stderr) = run_jay(root, &["--actor", "agent:codex", "task", "start", "1"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = run_jay(
        root,
        &[
            "--actor",
            "agent:codex",
            "task",
            "complete",
            "1",
            "--report-file",
            path,
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let saved = tasks::load_task(root, task.id).unwrap().unwrap();
    assert_eq!(saved.status, TaskStatus::Closed);
    assert_eq!(saved.actor.as_deref(), Some("agent:codex"));
    assert_eq!(saved.history.len(), 3);
    assert_eq!(saved.history[1].to, TaskStatus::Review);
    assert_eq!(saved.history.first().unwrap().actor, "agent:codex");
    assert_eq!(saved.history.last().unwrap().actor, "agent:codex");

    let (code, _, stderr) = run_jay(root, &["--actor", "agent:", "task", "show", "1"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("actor must be"), "{stderr}");
}

#[test]
fn cli_actor_flag_overrides_session_environment() {
    let proj = Proj::new("actor-precedence");
    let root = proj.root();
    let output = Command::new(jay_bin())
        .args(["task", "new", "From environment"])
        .env("JAY_ACTOR", "agent:session")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        tasks::load_task(root, 1).unwrap().unwrap().actor.as_deref(),
        Some("agent:session")
    );
    let output = Command::new(jay_bin())
        .args(["--actor", "agent:override", "task", "start", "1"])
        .env("JAY_ACTOR", "agent:session")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        tasks::load_task(root, 1).unwrap().unwrap().history[0].actor,
        "agent:override"
    );
}

#[test]
fn completion_rejects_missing_evidence_without_changing_started_task() {
    let proj = Proj::new("incomplete");
    let root = proj.root();
    let task = service::create_task(root, |id| {
        Task::new(id, "Deliver feature".into(), String::new())
    })
    .unwrap();
    service::apply_action(
        root,
        task.id,
        jay::model::TaskAction::Start,
        "agent:codex",
        None,
    )
    .unwrap();
    let before = std::fs::read(tasks::task_file(root, task.id)).unwrap();
    assert!(service::complete_task(root, task.id, report("", "passed"), "agent:codex").is_err());
    assert_eq!(
        std::fs::read(tasks::task_file(root, task.id)).unwrap(),
        before
    );
}

#[test]
fn automatic_git_mode_still_requires_review_before_completion() {
    let proj = Proj::new("auto-review");
    let root = proj.root();
    let task = service::create_task(root, |id| {
        Task::new(id, "Deliver feature".into(), String::new())
    })
    .unwrap();
    service::apply_action(
        root,
        task.id,
        jay::model::TaskAction::Start,
        "agent:codex",
        None,
    )
    .unwrap();
    let mut config = project::load_config(root).unwrap();
    config.git_integration = Some(GitIntegration::Auto);
    project::save_config(root, &config).unwrap();
    let before = std::fs::read(tasks::task_file(root, task.id)).unwrap();
    assert!(
        service::complete_task(root, task.id, report("done", "passed"), "agent:codex").is_err()
    );
    assert_eq!(
        std::fs::read(tasks::task_file(root, task.id)).unwrap(),
        before
    );
}

#[test]
fn linked_commit_is_verified_and_visible_after_completion() {
    let proj = Proj::new("commit-link");
    let root = proj.root();
    let task = service::create_task(root, |id| {
        Task::new(id, "Deliver feature".into(), String::new())
    })
    .unwrap();
    service::apply_action(
        root,
        task.id,
        jay::model::TaskAction::Start,
        "agent:codex",
        None,
    )
    .unwrap();
    service::complete_task(root, task.id, report("done", "passed"), "agent:codex").unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "Test"
    ])
    .status
    .success());
    let (code, _, stderr) = run_jay(
        root,
        &["--actor", "agent:codex", "task", "link-commit", "1", "HEAD"],
    );
    assert_eq!(code, 0, "{stderr}");
    let saved = tasks::load_task(root, task.id).unwrap().unwrap();
    assert!(saved.links.iter().any(|link| link.starts_with("commit:")));
    let (code, _, _) = run_jay(root, &["task", "link-commit", "1", "missing-ref"]);
    assert_eq!(code, 2);
    service::apply_action(
        root,
        task.id,
        jay::model::TaskAction::Reopen,
        "agent:codex",
        Some("follow-up fix"),
    )
    .unwrap();
    let reopened = tasks::load_task(root, task.id).unwrap().unwrap();
    assert!(!reopened
        .links
        .iter()
        .any(|link| link.starts_with("commit:")));
    assert!(reopened
        .history
        .last()
        .unwrap()
        .prior_report
        .as_ref()
        .unwrap()
        .links
        .iter()
        .any(|link| link.starts_with("commit:")));
}

#[test]
fn stale_status_warns_and_draft_uses_updated_task_reports() {
    let proj = Proj::new("status-draft");
    let root = proj.root();
    let task = service::create_task(root, |id| {
        Task::new(id, "Deliver feature".into(), String::new())
    })
    .unwrap();
    let entry = KnowledgeEntry::new(
        0,
        KnowledgeKind::Status,
        "Before delivery".into(),
        "Task is open".into(),
    );
    knowledge::add_status_entry(root, entry, true, "agent:codex").unwrap();
    service::apply_action(
        root,
        task.id,
        jay::model::TaskAction::Start,
        "agent:codex",
        None,
    )
    .unwrap();
    service::complete_task(
        root,
        task.id,
        report("Feature delivered", "Tests passed"),
        "agent:codex",
    )
    .unwrap();

    let (code, status, _) = run_jay(root, &["status"]);
    assert_eq!(code, 0);
    assert!(status.contains("possibly stale"), "{status}");
    let (code, draft, _) = run_jay(root, &["kb", "draft-status"]);
    assert_eq!(code, 0);
    assert!(draft.contains("DRAFT ONLY"), "{draft}");
    assert!(draft.contains("Feature delivered"), "{draft}");
    assert!(draft.contains("Task #1 [closed]"), "{draft}");
}

// ===== scenario 1: psotool-sized project =====

#[test]
fn psotool_workflow_no_git_noise_with_current_summary() {
    let proj = Proj::new("psotool");
    let r = proj.root();

    // automation IS configured, but the project stays quiet because
    // git_integration defaults to off for new projects
    let mut cfg = project::load_config(r).unwrap();
    assert_eq!(cfg.effective_git_integration(), GitIntegration::Off);
    cfg.branch_template = Some("feat/{task-id}-{slug}".into());
    cfg.git_repo = Some("git@host:o/psotool.git".into());
    project::save_config(r, &cfg).unwrap();

    // one fully reported local delivery
    let t1 = service::create_task(r, |id| {
        let mut t = Task::new(id, "Build the PSO laboratory".into(), "core + viz".into());
        t.acceptance = vec!["tests pass".into(), "docs written".into()];
        t
    })
    .unwrap();
    walk_to_review(r, t1.id);
    let done = service::complete_task(
        r,
        t1.id,
        report(
            "Implemented the lab",
            "8 numerical tests pass; smoke run ok",
        ),
        "agent:worker",
    )
    .unwrap();
    assert_eq!(done.status, TaskStatus::Closed);
    assert!(done.done_at.is_some());

    // three lightweight future tasks with empty acceptance criteria
    for title in ["Compare seeds", "More landscapes", "CSV extras"] {
        service::create_task(r, |id| {
            let mut t = Task::new(id, title.into(), "future work".into());
            t.labels = vec!["future".into()];
            t
        })
        .unwrap();
    }

    // no git automation noise anywhere
    let all = tasks::load_tasks(r).unwrap();
    assert_eq!(all.len(), 4);
    for t in &all {
        assert!(
            t.git_diagnostics.is_empty(),
            "task {} has diagnostics",
            t.id
        );
        assert!(t.report_problems.is_empty(), "task {} has noise", t.id);
        assert!(
            t.links.iter().all(|l| !l.starts_with("branch:")),
            "stale branch link"
        );
    }

    // a current summary is designated atomically with entry creation
    let mut e = KnowledgeEntry::new(
        0,
        KnowledgeKind::Status,
        "v1 delivered".into(),
        "Lab shipped; future tasks open.".into(),
    );
    e.commit = Some("local-only".into());
    let (saved, cs) = knowledge::add_status_entry(r, e, true, "human").unwrap();
    assert!(cs.is_some());
    let cs_view = knowledge::current_status(r).unwrap().unwrap();
    assert!(cs_view.designated);
    assert_eq!(cs_view.entry.id, saved.id);

    // doctor is clean; ordinary future tasks need no completion evidence
    let findings = diag::inspect(r);
    let errors: Vec<_> = findings
        .iter()
        .filter(|f| f.severity == diag::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");

    // CLI: status shows summary + ready work; next lists exactly the future tasks
    let (code, out, _) = run_jay(r, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("current summary"), "{out}");
    assert!(out.contains("ready"), "{out}");
    let (code, out, _) = run_jay(r, &["next", "--json"]);
    assert_eq!(code, 0);
    let ready: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(ready.len(), 3);
    let (code, _, _) = run_jay(r, &["doctor", "--json"]);
    assert_eq!(code, 0);
}

// ===== scenario 2: siggen-sized project =====

#[test]
fn siggen_workflow_deps_summaries_reopen_and_followup() {
    let proj = Proj::new("siggen");
    let r = proj.root();

    // dependency chain: 1 <- 2 <- 3, plus deferred tasks 4,5 depending on 3
    let t1 = service::create_task(r, |id| {
        Task::new(id, "Plan expansion".into(), String::new())
    })
    .unwrap();
    let t2 = service::create_task(r, |id| {
        let mut t = Task::new(id, "WGN/AWGN".into(), String::new());
        t.depends_on = vec![t1.id];
        t
    })
    .unwrap();
    let t3 = service::create_task(r, |id| {
        let mut t = Task::new(id, "FSK families".into(), String::new());
        t.depends_on = vec![t2.id];
        t
    })
    .unwrap();
    for title in ["Deferred 2-FSK", "Deferred dataset export"] {
        service::create_task(r, |id| {
            let mut t = Task::new(id, title.into(), String::new());
            t.depends_on = vec![t3.id];
            t.labels = vec!["deferred".into()];
            t
        })
        .unwrap();
    }

    // readiness follows the chain exactly (CLI and service agree)
    let ready = service::ready_tasks(r).unwrap();
    assert_eq!(ready.iter().map(|t| t.id).collect::<Vec<_>>(), vec![t1.id]);

    // batch completion: 1 then 2 then 3, each with one typed report call
    for (id, res) in [
        (t1.id, "plan written"),
        (t2.id, "awgn delivered"),
        (t3.id, "fsk delivered"),
    ] {
        walk_to_review(r, id);
        service::complete_task(r, id, report(res, "ctest 5/5 passes"), "agent:w").unwrap();
        let (_, out, _) = run_jay(r, &["next", "--json"]);
        let ready: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        // deferred tasks become ready only after t3 closes
        if id != t3.id {
            assert_eq!(ready.len(), 1, "chain gate after #{id}");
        } else {
            assert_eq!(ready.len(), 2, "both deferred tasks ready after #3");
        }
    }

    // revised summaries with supersession + explicit designation
    let s1 = knowledge::add_entry(
        r,
        KnowledgeEntry::new(
            0,
            KnowledgeKind::Status,
            "After plan".into(),
            "planning done".into(),
        ),
    )
    .unwrap();
    let mut s2 = KnowledgeEntry::new(
        0,
        KnowledgeKind::Status,
        "After delivery".into(),
        "CLI-first scope delivered".into(),
    );
    s2.supersedes = vec![s1.id];
    let s2 = knowledge::add_entry(r, s2).unwrap();
    knowledge::set_current_summary(r, s2.id, "agent:rev", None).unwrap();
    // a later ordinary entry does NOT steal the designation
    knowledge::add_entry(
        r,
        KnowledgeEntry::new(
            0,
            KnowledgeKind::Status,
            "Review note".into(),
            "working-tree claim".into(),
        ),
    )
    .unwrap();
    let cs = knowledge::current_status(r).unwrap().unwrap();
    assert!(cs.designated);
    assert_eq!(cs.entry.id, s2.id);
    // superseded text preserved
    let s1_after = knowledge::get_entry(r, KnowledgeKind::Status, s1.id)
        .unwrap()
        .unwrap();
    assert_eq!(s1_after.content, "planning done");

    // reopened review correction on t2: fresh evidence required, old cycle in history
    service::apply_action(
        r,
        t2.id,
        jay::model::TaskAction::Reopen,
        "human",
        Some("SNR metric was wrong"),
    )
    .unwrap();
    let reopened = tasks::load_task(r, t2.id).unwrap().unwrap();
    assert_eq!(reopened.status, TaskStatus::Open);
    assert!(reopened.report_result.is_none());
    walk_to_review(r, t2.id);
    // stale evidence cannot close it
    assert!(service::apply_action(r, t2.id, jay::model::TaskAction::Done, "human", None).is_err());
    let fixed = service::complete_task(
        r,
        t2.id,
        report("awgn fixed", "statistical tests rerun"),
        "human",
    )
    .unwrap();
    assert_eq!(fixed.report_result.as_deref(), Some("awgn fixed"));
    assert_eq!(
        fixed
            .history
            .iter()
            .filter(|e| e.prior_report.is_some())
            .count(),
        1
    );

    // linked follow-up keeps the original closed
    let before = std::fs::read_to_string(tasks::task_file(r, t3.id)).unwrap();
    let fu = service::create_follow_up(r, t3.id, "GFSK variant".into(), "optional".into(), "human")
        .unwrap();
    assert_eq!(fu.follow_up_of, Some(t3.id));
    assert_eq!(
        std::fs::read_to_string(tasks::task_file(r, t3.id)).unwrap(),
        before
    );

    // doctor stays clean (reopened t2 was re-closed before inspection)
    let findings = diag::inspect(r);
    let errors: Vec<_> = findings
        .iter()
        .filter(|f| f.severity == diag::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

// ===== scenario 3: legacy records =====

const LEGACY_TASK_17: &str = r#"id = 17
title = "Model waveform families and capabilities"
description = "legacy closed record"
status = "closed"
blocked = false
acceptance = ["a"]
context = ""
priority = "medium"
labels = []
links = []
report_problems = []
report_ideas = []
report_decisions = ["d"]
created_at = "2026-09-12T15:08:43"
actor = "agent"
updated_at = "2026-09-13T18:45:00"
closed_at = "2026-09-13T18:45:00Z"
"#;

const LEGACY_STATUS: &str = r#"[[entries]]
id = 1
kind = "status"
title = "Current state (2026-08-29)"
content = "Functional generator; tests empty."
tags = ["current"]
actor = "human"
created_at = "2026-08-29T01:16:02"
updated_at = "2026-08-29T01:16:02"

[[entries]]
id = 2
kind = "status"
title = "Task #4 completed"
content = "Created sequential tasks. Local review; automatic branch/PR workflow unconfigured."
tags = []
related_task = 4
actor = "agent"
created_at = "2026-09-12T14:26:07"
updated_at = "2026-09-12T14:26:07"
"#;

#[test]
fn legacy_records_readonly_diagnosis_and_safe_repair() {
    let proj = Proj::new("legacy");
    let r = proj.root();
    std::fs::write(r.join(".nest/tasks/17.toml"), LEGACY_TASK_17).unwrap();
    // a conflicting record: closed_at differs from done_at
    std::fs::write(
        r.join(".nest/tasks/18.toml"),
        LEGACY_TASK_17
            .replace("id = 17", "id = 18")
            .replace("closed_at = \"2026-09-13T18:45:00Z\"", "done_at = \"2026-09-13T10:00:00Z\"\nclosed_at = \"2026-09-13T18:45:00Z\"\nreport_result = \"r\"\nreport_validation = \"v\""),
    )
    .unwrap();
    // a malformed file that must not hide findings in later files
    std::fs::write(r.join(".nest/tasks/19.toml"), "status = ").unwrap();
    std::fs::create_dir_all(r.join(".nest/kb")).unwrap();
    std::fs::write(r.join(".nest/kb/status.toml"), LEGACY_STATUS).unwrap();

    // CLI doctor: exit 1, JSON findings, all problems visible
    let (code, out, _) = run_jay(r, &["doctor", "--json"]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let codes: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"task.legacy_closed_at"), "{codes:?}");
    assert!(codes.contains(&"task.closed_missing_done_at"), "{codes:?}");
    assert!(codes.contains(&"task.closed_missing_result"), "{codes:?}");
    assert!(codes.contains(&"task.closed_at_conflict"), "{codes:?}");
    assert!(codes.contains(&"task.parse_error"), "{codes:?}");
    assert!(codes.contains(&"kb.legacy_current_tag"), "{codes:?}");
    // contradictory tag is a warning, never an error
    let tag_f = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "kb.legacy_current_tag")
        .unwrap();
    assert_eq!(tag_f["severity"], "warning");
    // warnings alone don't fail: a project with only warnings exits 0 (checked below)

    // diagnosis is read-only
    let before17 = std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap();
    let before_status = std::fs::read_to_string(r.join(".nest/kb/status.toml")).unwrap();

    // repair dry run changes no bytes
    let (code, out, _) = run_jay(r, &["repair"]);
    assert_eq!(code, 0);
    assert!(out.contains("dry run"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap(),
        before17
    );

    // apply: migrates 17, leaves the conflicting 18 and malformed 19 untouched, backs up
    let (code, out, _) = run_jay(r, &["repair", "--apply"]);
    assert_eq!(code, 0, "{out}");
    let after17 = std::fs::read_to_string(r.join(".nest/tasks/17.toml")).unwrap();
    assert!(after17.contains("done_at = \"2026-09-13T18:45:00Z\""));
    assert!(!after17.contains("closed_at"));
    // no new fields invented on the repaired record
    assert!(!after17.contains("history"));
    assert!(!after17.contains("depends_on"));
    assert!(
        !after17.contains("report_result"),
        "evidence never fabricated"
    );
    assert!(out.contains("backup:"), "{out}");
    let after18 = std::fs::read_to_string(r.join(".nest/tasks/18.toml")).unwrap();
    assert!(
        after18.contains("closed_at"),
        "conflicting record untouched"
    );
    assert_eq!(
        std::fs::read_to_string(r.join(".nest/tasks/19.toml")).unwrap(),
        "status = "
    );
    // KB with the contradictory tag is untouched
    assert_eq!(
        std::fs::read_to_string(r.join(".nest/kb/status.toml")).unwrap(),
        before_status
    );

    // idempotent: second run proposes nothing
    let (code, out, _) = run_jay(r, &["repair"]);
    assert_eq!(code, 0);
    assert!(out.contains("no repairable issues"), "{out}");

    // unresolved errors remain visible and doctor still exits 1
    let (code, _, _) = run_jay(r, &["doctor", "--json"]);
    assert_eq!(code, 1);

    // invocation failure has a distinct exit code (2)
    let empty = tempfile::tempdir().unwrap();
    let (code, _, err) = run_jay(empty.path(), &["doctor"]);
    assert_eq!(code, 2);
    assert!(!err.is_empty());
}

#[test]
fn warnings_alone_do_not_fail_doctor() {
    let proj = Proj::new("warnonly");
    let r = proj.root();
    // unknown field on a task = warning only
    let mut t = Task::new(1, "t".into(), String::new());
    t.actor = Some("human".into());
    tasks::save_task(r, &t).unwrap();
    let p = tasks::task_file(r, 1);
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("future_field = 1\n");
    std::fs::write(&p, text).unwrap();
    let (code, out, _) = run_jay(r, &["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("warning"), "{out}");
}

// ===== scenario 4: CLI/MCP parity =====

struct McpChild {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpChild {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(jay_bin())
            .arg("mcp")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut m = McpChild {
            child,
            stdin,
            reader,
            next_id: 1,
        };
        m.request(
            "initialize",
            &serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "0"}
            }),
        );
        m.notify("notifications/initialized", &serde_json::json!({}));
        m
    }

    fn send(&mut self, msg: &serde_json::Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: &serde_json::Value) {
        self.send(&serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "MCP server closed the connection");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
                return v;
            }
        }
    }

    fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let resp = self.request(
            "tools/call",
            &serde_json::json!({"name": tool, "arguments": args}),
        );
        assert!(resp.get("error").is_none(), "tool {tool} errored: {resp}");
        let result = &resp["result"];
        assert!(
            !result["isError"].as_bool().unwrap_or(false),
            "tool {tool} failed: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
    }

    fn tools_list(&mut self) -> Vec<serde_json::Value> {
        let resp = self.request("tools/list", &serde_json::json!({}));
        resp["result"]["tools"].as_array().unwrap().clone()
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_schemas_expose_new_operations() {
    let proj = Proj::new("schema");
    let r = proj.root();
    let mut mcp = McpChild::start(r);
    let tools = mcp.tools_list();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "patch_task",
        "update_report",
        "complete_task",
        "next_tasks",
        "project_context",
        "set_current_summary",
        "create_follow_up",
        "update_task",
        "update_report_section",
    ] {
        assert!(
            names.contains(&expected),
            "missing MCP tool {expected}: {names:?}"
        );
    }
    // legacy tools remain documented as legacy in their descriptions
    let legacy = tools.iter().find(|t| t["name"] == "update_task").unwrap();
    assert!(legacy["description"].as_str().unwrap().contains("LEGACY"));
    // patch_task schema exposes the patch object
    let patch = tools.iter().find(|t| t["name"] == "patch_task").unwrap();
    let props = &patch["inputSchema"]["properties"]["patch"];
    assert!(
        !props.is_null(),
        "patch_task schema must expose a patch argument: {patch}"
    );
}

#[test]
fn cli_mcp_parity_edit_report_complete_ready_context_reopen() {
    let proj = Proj::new("parity");
    let r = proj.root();

    // seed via CLI
    let (code, _, _) = run_jay(r, &["task", "new", "Shared task"]);
    assert_eq!(code, 0);
    let (code, _, _) = run_jay(r, &["task", "new", "Second task"]);
    assert_eq!(code, 0);

    let mut mcp = McpChild::start(r);

    // edit: MCP patch_task (partial) — CLI task edit uses the same service
    let patched = mcp.call(
        "patch_task",
        serde_json::json!({"id": 1, "patch": {"description": "via mcp", "labels": ["x"]}}),
    );
    assert_eq!(patched["description"], "via mcp");
    assert_eq!(patched["title"], "Shared task", "omitted fields unchanged");

    // CLI patch with null-clears semantics agrees with MCP behavior
    std::fs::write(r.join("patch.json"), r#"{"labels": [], "assignee": null}"#).unwrap();
    let (code, _, _) = run_jay(r, &["task", "edit", "1", "--patch-file", "patch.json"]);
    assert_eq!(code, 0);
    let via_mcp = mcp.call("get_task", serde_json::json!({"id": 1}));
    assert_eq!(via_mcp["labels"].as_array().unwrap().len(), 0);
    assert!(
        via_mcp["description"] == "via mcp",
        "CLI patch kept other fields"
    );

    // report + completion parity
    mcp.call(
        "update_task_status",
        serde_json::json!({"id": 1, "action": "start"}),
    );
    mcp.call(
        "update_task_status",
        serde_json::json!({"id": 1, "action": "review"}),
    );
    let rep = mcp.call(
        "update_report",
        serde_json::json!({"id": 1, "mode": "append", "result": "first", "problems": ["p1"]}),
    );
    assert_eq!(rep["report_result"], "first");
    let done = mcp.call(
        "complete_task",
        serde_json::json!({"id": 1, "result": "final", "validation": "v", "problems": [], "ideas": [], "decisions": []}),
    );
    assert_eq!(done["status"], "closed");

    // readiness parity: jay next --json vs next_tasks
    let (_, out, _) = run_jay(r, &["next", "--json"]);
    let cli_ready: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let mcp_ready = mcp.call("next_tasks", serde_json::json!({}));
    let cli_ids: Vec<i64> = cli_ready
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    let mcp_ids: Vec<i64> = mcp_ready
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(cli_ids, mcp_ids);
    assert_eq!(cli_ids, vec![2]);

    // current context parity: jay status --json vs project_context
    let (_, cli_ctx, _) = run_jay(r, &["status", "--json"]);
    let cli_ctx: serde_json::Value = serde_json::from_str(&cli_ctx).unwrap();
    let mcp_ctx = mcp.call("project_context", serde_json::json!({}));
    for key in [
        "counts",
        "active",
        "blocked",
        "ready",
        "summary",
        "git_integration",
    ] {
        assert_eq!(cli_ctx[key], mcp_ctx[key], "context mismatch on {key}");
    }

    // current summary via set_current_summary + get_project_status
    mcp.call(
        "add_kb",
        serde_json::json!({"kind": "status", "title": "S1", "content": "state one", "set_current": true}),
    );
    let st = mcp.call("get_project_status", serde_json::json!({}));
    assert_eq!(st["designated"], true);
    assert_eq!(st["entry"]["title"], "S1");

    // reopen parity: reason required, fresh evidence required
    let err_resp = mcp.request(
        "tools/call",
        &serde_json::json!({"name": "update_task_status", "arguments": {"id": 1, "action": "reopen"}}),
    );
    let failed =
        err_resp.get("error").is_some() || err_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(failed, "reopen without reason must fail: {err_resp}");
    let reopened = mcp.call(
        "update_task_status",
        serde_json::json!({"id": 1, "action": "reopen", "reason": "review fix"}),
    );
    assert_eq!(reopened["status"], "open");
    let t = tasks::load_task(r, 1).unwrap().unwrap();
    assert!(
        t.report_result.is_none(),
        "fresh cycle requires fresh evidence"
    );
    assert!(t.history.iter().any(|e| e.prior_report.is_some()));

    // follow-up parity: MCP create_follow_up vs CLI task follow-up
    let fu = mcp.call(
        "create_follow_up",
        serde_json::json!({"task_id": 2, "title": "MCP follow-up"}),
    );
    assert_eq!(fu["follow_up_of"], 2);
    let (code, out, _) = run_jay(r, &["task", "follow-up", "2", "--title", "CLI follow-up"]);
    assert_eq!(code, 0, "{out}");
    let all = tasks::load_tasks(r).unwrap();
    let fus: Vec<_> = all.iter().filter(|t| t.follow_up_of == Some(2)).collect();
    assert_eq!(fus.len(), 2);
}
