//! Reproductions of the workflow reliability review findings.
use jay::{
    knowledge,
    model::{KnowledgeEntry, KnowledgeKind, Task, TaskAction, TaskStatus},
    project, service, tasks,
};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    project::init_project(dir.path(), None).unwrap();
    dir
}

fn close(root: &Path, id: i64) {
    service::apply_action(root, id, TaskAction::Start, "test", None).unwrap();
    service::apply_action(root, id, TaskAction::Review, "test", None).unwrap();
    service::complete_task(
        root,
        id,
        service::ReportUpdate {
            result: Some("delivered".into()),
            validation: Some("checked".into()),
            ..Default::default()
        },
        "test",
    )
    .unwrap();
}

#[test]
fn concurrent_cli_kb_adds_keep_every_entry_and_unique_id() {
    let dir = project();
    let barrier = std::sync::Barrier::new(24);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..24)
            .map(|i| {
                let barrier = &barrier;
                let root = dir.path();
                scope.spawn(move || {
                    barrier.wait();
                    let output = Command::new(env!("CARGO_BIN_EXE_jay"))
                        .args([
                            "kb",
                            "add",
                            "--kind",
                            "status",
                            "--title",
                            &format!("entry {i}"),
                            "--content",
                            "delivered",
                            "--set-current",
                        ])
                        .current_dir(root)
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    let entries = knowledge::load_entries(dir.path(), KnowledgeKind::Status).unwrap();
    assert_eq!(entries.len(), 24);
    let ids: std::collections::BTreeSet<_> = entries.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 24);
    assert_eq!(
        knowledge::current_summary_ref(dir.path())
            .unwrap()
            .unwrap()
            .entry_id,
        24
    );
}

#[test]
fn kb_updates_preserve_extensions_at_every_document_level() {
    let dir = project();
    let root = dir.path();
    let entry = KnowledgeEntry::new(
        0,
        KnowledgeKind::Status,
        "summary".into(),
        "delivered".into(),
    );
    knowledge::add_status_entry(root, entry, true, "test").unwrap();
    let path = root.join(".nest/kb/status.toml");
    let mut raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    raw.as_table_mut().unwrap().insert(
        "future_root".into(),
        toml::Value::String("root extension".into()),
    );
    raw["entries"][0].as_table_mut().unwrap().insert(
        "future_entry".into(),
        toml::Value::String("entry extension".into()),
    );
    raw["current_summary"].as_table_mut().unwrap().insert(
        "future_summary".into(),
        toml::Value::String("summary extension".into()),
    );
    std::fs::write(&path, toml::to_string_pretty(&raw).unwrap()).unwrap();
    knowledge::update_entry(root, KnowledgeKind::Status, 1, |e| {
        e.content = "updated".into();
        e.commit = None;
    })
    .unwrap();
    knowledge::add_entry(
        root,
        KnowledgeEntry::new(0, KnowledgeKind::Status, "later".into(), "history".into()),
    )
    .unwrap();
    knowledge::set_current_summary(root, 1, "test", None).unwrap();
    let after: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(after["future_root"], raw["future_root"]);
    assert_eq!(
        after["entries"][0]["future_entry"],
        raw["entries"][0]["future_entry"]
    );
    assert_eq!(
        after["current_summary"]["future_summary"],
        raw["current_summary"]["future_summary"]
    );
    assert_eq!(after["entries"][0]["content"].as_str(), Some("updated"));
}

#[test]
fn old_lock_file_never_evicts_a_live_owner() {
    let dir = project();
    let lock = service::ProjectLock::acquire(dir.path()).unwrap();
    let path = dir.path().join(".nest/lock");
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(120)),
    )
    .unwrap();
    assert!(
        service::ProjectLock::acquire_with_timeout(dir.path(), Duration::from_millis(40)).is_err()
    );
    drop(lock);
    let _new_owner = service::ProjectLock::acquire(dir.path()).unwrap();
    assert!(path.exists());
}

// Child entry point: the parent kills it while it owns the lock.
#[test]
fn lock_holder_child() {
    let Some(root) = std::env::var_os("JAY_REVIEW_LOCK_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let _lock = service::ProjectLock::acquire(root).unwrap();
    std::fs::write(root.join("holder-ready"), "ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn crashed_process_releases_lock_without_stale_timeout() {
    let dir = project();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_child", "--nocapture"])
        .env("JAY_REVIEW_LOCK_ROOT", dir.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.path().join("holder-ready").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ready = dir.path().join("holder-ready").exists();
    let blocked =
        service::ProjectLock::acquire_with_timeout(dir.path(), Duration::from_millis(40)).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        ready && blocked,
        "child must own the lock before termination"
    );
    let _lock =
        service::ProjectLock::acquire_with_timeout(dir.path(), Duration::from_millis(100)).unwrap();
}

#[test]
fn dependency_is_rechecked_after_waiting_for_the_lock() {
    let dir = project();
    let root = dir.path();
    let prerequisite =
        service::create_task(root, |id| Task::new(id, "prerequisite".into(), "".into())).unwrap();
    close(root, prerequisite.id);
    let dependent = service::create_task(root, |id| {
        let mut t = Task::new(id, "dependent".into(), "".into());
        t.depends_on = vec![prerequisite.id];
        t
    })
    .unwrap();
    let lock = service::ProjectLock::acquire(root).unwrap();
    std::thread::scope(|scope| {
        let waiter = scope
            .spawn(|| service::apply_action(root, dependent.id, TaskAction::Start, "test", None));
        std::thread::sleep(Duration::from_millis(100));
        // Simulate a prerequisite mutation by the writer already holding the lock.
        let mut t = tasks::load_task(root, prerequisite.id).unwrap().unwrap();
        t.apply(
            TaskAction::Reopen,
            &jay::model::LifecycleCtx::new("test", Some("review fix")),
        )
        .unwrap();
        tasks::save_task(root, &t).unwrap();
        drop(lock);
        assert!(waiter
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("unmet dependencies"));
    });
    assert_eq!(
        tasks::load_task(root, dependent.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Open
    );
}

#[test]
fn reopening_uses_new_branches_and_keeps_prior_links_in_history() {
    let dir = project();
    let root = dir.path();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "baseline",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success());
    }
    let mut cfg = project::load_config(root).unwrap();
    cfg.git_integration = Some(project::GitIntegration::Auto);
    cfg.branch_template = Some("task/{task-id}-{slug}".into());
    cfg.git_repo = Some(".".into()); // local integration: no network or PR
    project::save_config(root, &cfg).unwrap();
    let t = service::create_task(root, |id| Task::new(id, "example".into(), "".into())).unwrap();
    close(root, t.id);
    for cycle in 2..=3 {
        let reopened =
            service::apply_action(root, t.id, TaskAction::Reopen, "test", Some("review fix"))
                .unwrap();
        assert!(!reopened
            .history
            .last()
            .unwrap()
            .prior_report
            .as_ref()
            .unwrap()
            .links
            .is_empty());
        close(root, t.id);
        let t = tasks::load_task(root, t.id).unwrap().unwrap();
        assert_eq!(
            t.links,
            vec![format!("branch:task/1-example-cycle-{cycle}")]
        );
        assert!(t.git_diagnostics.is_empty(), "{:?}", t.git_diagnostics);
    }
}

#[test]
fn applied_repair_reports_only_remaining_errors() {
    let dir = project();
    let root = dir.path();
    let t = service::create_task(root, |id| Task::new(id, "legacy".into(), "".into())).unwrap();
    close(root, t.id);
    let path = root.join(".nest/tasks/1.toml");
    let mut raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let table = raw.as_table_mut().unwrap();
    let done = table.remove("done_at").unwrap();
    table.insert("closed_at".into(), done);
    table.remove("report_validation");
    std::fs::write(&path, toml::to_string_pretty(&raw).unwrap()).unwrap();
    let dry = jay::diag::repair(root, false).unwrap();
    assert!(dry
        .unresolved
        .iter()
        .any(|f| f.code == "task.closed_missing_done_at"));
    let applied = jay::diag::repair(root, true).unwrap();
    assert!(!applied
        .unresolved
        .iter()
        .any(|f| f.code == "task.closed_missing_done_at"));
    assert!(applied
        .unresolved
        .iter()
        .any(|f| f.code == "task.closed_missing_validation"));
}

#[test]
fn bundled_examples_use_the_current_task_schema() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/workspace");
    for (name, expected) in [("alpha", TaskStatus::Open), ("beta", TaskStatus::Started)] {
        let root = workspace.join(name);
        let examples = tasks::load_tasks(&root).unwrap();
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].status, expected);
        service::validate_record(&examples[0]).unwrap();
        service::validate_dependencies(&root, &examples[0]).unwrap();
    }
}

#[test]
fn forgejo_requires_an_explicit_url_even_when_token_is_present() {
    let dir = project();
    let root = dir.path();
    let task = service::create_task(root, |id| Task::new(id, "example".into(), "".into())).unwrap();
    service::apply_action(root, task.id, TaskAction::Start, "test", None).unwrap();
    service::add_task_link(root, task.id, "branch:example").unwrap();
    let mut cfg = project::load_config(root).unwrap();
    cfg.git_integration = Some(project::GitIntegration::Auto);
    cfg.git_repo = Some("https://forgejo.example.com/owner/repo.git".into());
    project::save_config(root, &cfg).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jay"))
        .args(["task", "review", &task.id.to_string()])
        .env("FORGEJO_TOKEN", "test-placeholder")
        .env_remove("FORGEJO_URL")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let task = tasks::load_task(root, task.id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Review);
    assert_eq!(task.git_diagnostics.len(), 1);
    assert!(task.git_diagnostics[0]
        .message
        .contains("FORGEJO_URL missing or empty"));
}
