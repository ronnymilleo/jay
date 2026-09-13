//! Optional git-flow integration.
//!
//! Tasks carry branch and pull-request links. When git integration is `auto`
//! and a task starts, a branch is created from the project's template; when
//! it moves to review, a pull request is opened. When integration is `off`
//! (the default for new projects), no Git/Forgejo operation is attempted and
//! no missing-configuration noise is produced; manually attached links remain
//! usable. The actual git/Forgejo side effects are best-effort and never fail
//! the status transition — failures are recorded as structured
//! `git_diagnostics` on the task (never as report problems), and a link is
//! only recorded when the operation actually succeeded.
//!
//! Lifecycle entry points live in [`crate::service`]; this module only holds
//! the branch/PR side-effect helpers.

use anyhow::Result;
use std::path::Path;
use std::process::Command;

use crate::model::Task;
use crate::project::{self, ProjectConfig};
use crate::service;
use crate::tasks;

/// Converts a title into a branch-safe slug (lowercase, alphanumerics and dashes only).
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Renders a branch name from a template. Supported placeholders:
/// {project}, {task-id} / {task_id}, {slug}.
pub fn render_branch_name(template: &str, project_name: &str, task_id: i64, slug: &str) -> String {
    template
        .replace("{project}", project_name)
        .replace("{task-id}", &task_id.to_string())
        .replace("{task_id}", &task_id.to_string())
        .replace("{slug}", slug)
}

/// Returns the branch name for a task, if its project has a branch template.
pub fn branch_name_for(config: &ProjectConfig, task: &Task) -> Option<String> {
    let template = config.branch_template.as_deref()?;
    if template.trim().is_empty() {
        return None;
    }
    let slug = slugify(&task.title);
    let base = render_branch_name(template, &config.name, task.id, &slug);
    let reopen_count = task
        .history
        .iter()
        .filter(|event| {
            event.to == crate::model::TaskStatus::Open
                && matches!(
                    event.from,
                    Some(crate::model::TaskStatus::Closed | crate::model::TaskStatus::Cancelled)
                )
        })
        .count();
    Some(if reopen_count == 0 {
        base
    } else {
        format!("{base}-cycle-{}", reopen_count + 1)
    })
}

/// True when git_repo is a local path (rather than a remote URL).
fn local_git_dir(git_repo: Option<&str>) -> bool {
    matches!(git_repo, Some(r) if r.starts_with('/') || r.starts_with('.') || r.starts_with("~/") || r.starts_with("~\\"))
}

/// Runs a git command in the project folder, best-effort.
fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Creates the task's branch (from the project template) and records its
/// link — only when the branch was actually created.
pub(crate) fn auto_branch(root: &Path, task_id: i64) -> Result<()> {
    let task = tasks::load_task(root, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
    let config = project::load_config(root)?;

    let Some(branch) = branch_name_for(&config, &task) else {
        service::add_git_diagnostic(
            root,
            task_id,
            "branch",
            "no branch_template configured; branch not created",
        )?;
        return Ok(());
    };

    if !root.join(".git").exists() {
        service::add_git_diagnostic(
            root,
            task_id,
            "branch",
            "project folder is not a git repository",
        )?;
        return Ok(());
    }

    match run_git(root, &["checkout", "-b", &branch]) {
        Ok(()) => service::add_task_link(root, task_id, &format!("branch:{branch}"))?,
        Err(e) => service::add_git_diagnostic(
            root,
            task_id,
            "branch",
            &format!("branch creation failed: {e}"),
        )?,
    }
    Ok(())
}

/// Opens a pull request for the task's branch (via the Forgejo API when a
/// token is configured) and records the real PR URL — only on success.
pub(crate) fn auto_pr(root: &Path, task_id: i64) -> Result<()> {
    let task = tasks::load_task(root, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
    let config = project::load_config(root)?;

    if has_pr_link(&task.links) {
        return Ok(());
    }

    let Some(branch) = task
        .links
        .iter()
        .find_map(|l| l.strip_prefix("branch:"))
        .map(String::from)
    else {
        service::add_git_diagnostic(
            root,
            task_id,
            "pr",
            "no branch link recorded; cannot open PR",
        )?;
        return Ok(());
    };

    let Some(git_repo) = config.git_repo.as_deref() else {
        service::add_git_diagnostic(
            root,
            task_id,
            "pr",
            "no git_repo configured; cannot open PR",
        )?;
        return Ok(());
    };
    if local_git_dir(Some(git_repo)) {
        // Local checkout: no remote PR — this is expected, not a failure.
        return Ok(());
    }
    let Some((owner, repo)) = crate::forgejo::Forgejo::parse_repo(git_repo) else {
        service::add_git_diagnostic(
            root,
            task_id,
            "pr",
            &format!("cannot parse git_repo '{git_repo}' into owner/repo"),
        )?;
        return Ok(());
    };

    let Some(client) = crate::forgejo::Forgejo::from_env() else {
        service::add_git_diagnostic(
            root,
            task_id,
            "pr",
            "FORGEJO_TOKEN or FORGEJO_URL missing or empty; PR not created (attach a link manually if needed)",
        )?;
        return Ok(());
    };

    let base = client
        .default_branch(&owner, &repo)
        .unwrap_or_else(|_| "main".to_string());
    let body = pr_body(&task);
    match client.create_pr(&owner, &repo, &branch, &base, &task.title, &body) {
        Ok(url) => service::add_task_link(root, task_id, &url)?,
        Err(e) => {
            service::add_git_diagnostic(root, task_id, "pr", &format!("PR creation failed: {e}"))?;
        }
    }
    Ok(())
}

/// True if a task already carries a PR link: either a real PR URL or a
/// legacy `pr:` placeholder (placeholders are never created anymore).
fn has_pr_link(links: &[String]) -> bool {
    links
        .iter()
        .any(|l| l.starts_with("pr:") || l.contains("/pulls/"))
}

/// Builds the PR description body from a task.
fn pr_body(task: &Task) -> String {
    let mut b = if task.description.is_empty() {
        String::new()
    } else {
        format!("{}\n", task.description)
    };
    if !task.acceptance.is_empty() {
        b.push_str("\nAcceptance criteria:\n");
        for a in &task.acceptance {
            b.push_str(&format!("- {a}\n"));
        }
    }
    b.push_str(&format!("\njay task #{}\n", task.id));
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskAction;
    use crate::project::{init_project, GitIntegration};

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("Fix the Bug!!"), "fix-the-bug");
        assert_eq!(slugify("  Hello World  "), "hello-world");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn render_branch_name_substitutes_placeholders() {
        let out = render_branch_name("feature/{task-id}-{slug}", "demo", 42, "fix-bug");
        assert_eq!(out, "feature/42-fix-bug");
    }

    #[test]
    fn branch_name_requires_template() {
        let mut cfg = config();
        let task = crate::model::Task::new(1, "Fix bug".into(), "".into());
        assert!(branch_name_for(&cfg, &task).is_none());
        cfg.branch_template = Some("feat/{task-id}-{slug}".into());
        assert_eq!(branch_name_for(&cfg, &task).unwrap(), "feat/1-fix-bug");
    }

    fn config() -> ProjectConfig {
        ProjectConfig {
            name: "demo".into(),
            description: String::new(),
            goal: None,
            git_repo: None,
            branch_template: None,
            git_integration: None,
            links: Vec::new(),
        }
    }

    fn proj() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        init_project(base.path(), None).unwrap();
        base
    }

    fn set_config(root: &Path, f: impl FnOnce(&mut ProjectConfig)) {
        let mut cfg = project::load_config(root).unwrap();
        f(&mut cfg);
        project::save_config(root, &cfg).unwrap();
    }

    #[test]
    fn legacy_config_resolves_integration_from_automation_fields() {
        let mut cfg = config();
        cfg.git_integration = None;
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Off);
        cfg.branch_template = Some("".into());
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Off);
        cfg.branch_template = Some("feat/{task-id}".into());
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Auto);
        cfg.branch_template = None;
        cfg.git_repo = Some("git@host:o/r.git".into());
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Auto);
        // explicit off wins even with automation configured
        cfg.git_integration = Some(GitIntegration::Off);
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Off);
    }

    #[test]
    fn new_projects_default_to_off() {
        let root = proj();
        let cfg = project::load_config(root.path()).unwrap();
        assert_eq!(cfg.git_integration, Some(GitIntegration::Off));
        assert_eq!(cfg.effective_git_integration(), GitIntegration::Off);
    }

    #[test]
    fn off_mode_start_review_has_no_side_effects_or_noise() {
        let root = proj();
        let r = root.path();
        // stale links from an older cycle; template configured but mode is off
        set_config(r, |c| {
            c.branch_template = Some("feat/{task-id}-{slug}".into());
            c.git_repo = Some("git@host:o/rep.git".into());
        });
        let mut t = Task::new(1, "Fix bug".into(), "".into());
        t.links = vec!["branch:feat/old".into()];
        tasks::save_task(r, &t).unwrap();

        service::apply_action(r, 1, TaskAction::Start, "human", None).unwrap();
        service::apply_action(r, 1, TaskAction::Review, "human", None).unwrap();

        let t = tasks::load_task(r, 1).unwrap().unwrap();
        assert_eq!(t.status, crate::model::TaskStatus::Review);
        // no new links, no diagnostics, no report problems
        assert_eq!(t.links, vec!["branch:feat/old"]);
        assert!(t.git_diagnostics.is_empty());
        assert!(t.report_problems.is_empty());
    }

    #[test]
    fn auto_mode_failed_branch_creation_records_diagnostic_without_link() {
        let root = proj();
        let r = root.path();
        // auto mode, template set, but the folder is NOT a git repo
        set_config(r, |c| {
            c.git_integration = Some(GitIntegration::Auto);
            c.branch_template = Some("feat/{task-id}-{slug}".into());
        });
        tasks::save_task(r, &Task::new(1, "Fix bug".into(), "".into())).unwrap();

        let t = service::apply_action(r, 1, TaskAction::Start, "human", None).unwrap();
        assert_eq!(t.status, crate::model::TaskStatus::Started);
        assert!(t.links.is_empty(), "no branch link when creation failed");
        assert_eq!(t.git_diagnostics.len(), 1);
        assert_eq!(t.git_diagnostics[0].operation, "branch");
        assert!(
            t.report_problems.is_empty(),
            "diagnostics stay out of the report"
        );
    }

    #[test]
    fn auto_mode_pr_failure_is_visible_separately_from_report() {
        let root = proj();
        let r = root.path();
        // remote repo configured, but no FORGEJO_TOKEN in this test process
        std::env::remove_var("FORGEJO_TOKEN");
        set_config(r, |c| {
            c.git_integration = Some(GitIntegration::Auto);
            c.git_repo = Some("git@host:o/rep.git".into());
        });
        let mut t = Task::new(1, "Fix bug".into(), "".into());
        t.status = crate::model::TaskStatus::Started;
        t.links = vec!["branch:feat/1-fix-bug".into()];
        tasks::save_task(r, &t).unwrap();

        let t = service::apply_action(r, 1, TaskAction::Review, "human", None).unwrap();
        assert_eq!(t.status, crate::model::TaskStatus::Review);
        assert!(t.report_problems.is_empty());
        assert_eq!(t.git_diagnostics.len(), 1);
        assert_eq!(t.git_diagnostics[0].operation, "pr");
        // no placeholder pr: link is recorded on failure
        assert!(!t.links.iter().any(|l| l.starts_with("pr:")));
    }

    #[test]
    fn auto_mode_branch_created_in_disposable_repo_records_link() {
        let root = proj();
        let r = root.path();
        // disposable local git repo (no network)
        run_git_cmd(r, &["init", "-q", "-b", "main"]).unwrap();
        run_git_cmd(r, &["config", "user.email", "t@t"]).unwrap();
        run_git_cmd(r, &["config", "user.name", "t"]).unwrap();
        std::fs::write(r.join("f.txt"), "x").unwrap();
        run_git_cmd(r, &["add", "."]).unwrap();
        run_git_cmd(r, &["commit", "-qm", "init"]).unwrap();
        set_config(r, |c| {
            c.git_integration = Some(GitIntegration::Auto);
            c.branch_template = Some("feat/{task-id}-{slug}".into());
        });
        tasks::save_task(r, &Task::new(1, "Fix bug".into(), "".into())).unwrap();

        let t = service::apply_action(r, 1, TaskAction::Start, "human", None).unwrap();
        assert_eq!(t.links, vec!["branch:feat/1-fix-bug"]);
        assert!(t.git_diagnostics.is_empty());
    }

    fn run_git_cmd(root: &Path, args: &[&str]) -> Result<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }
}
