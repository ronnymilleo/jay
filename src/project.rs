//! Project resolution, initialization and config (the `.nest/` folder).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const NEST_DIR: &str = ".nest";
pub const CONFIG_FILE: &str = "config.toml";
pub const TASKS_DIR: &str = "tasks";
pub const MILESTONES_FILE: &str = "milestones.toml";
pub const KB_DIR: &str = "kb";

/// Git/Forgejo automation policy for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitIntegration {
    /// No automatic branch/PR operations, no missing-configuration noise.
    #[serde(rename = "off")]
    Off,
    /// Best-effort branch/PR automation; failures become structured
    /// diagnostics on the task, never report problems.
    #[serde(rename = "auto")]
    Auto,
}

impl GitIntegration {
    pub fn as_str(self) -> &'static str {
        match self {
            GitIntegration::Off => "off",
            GitIntegration::Auto => "auto",
        }
    }
}

impl std::str::FromStr for GitIntegration {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "off" => Ok(GitIntegration::Off),
            "auto" => Ok(GitIntegration::Auto),
            other => anyhow::bail!("unknown git_integration mode: {other} (use off|auto)"),
        }
    }
}

/// Project metadata, stored in `.nest/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub git_repo: Option<String>,
    #[serde(default)]
    pub branch_template: Option<String>,
    /// Explicit automation policy. `None` only happens in legacy configs;
    /// see [ProjectConfig::effective_git_integration].
    #[serde(default)]
    pub git_integration: Option<GitIntegration>,
    #[serde(default)]
    pub links: Vec<String>,
}

impl ProjectConfig {
    /// Resolves the effective git integration policy.
    ///
    /// Compatibility rule for legacy configs without the field: automation
    /// stays enabled (`auto`) when `git_repo` or `branch_template` is
    /// configured and nonblank; otherwise it resolves to `off`.
    pub fn effective_git_integration(&self) -> GitIntegration {
        match self.git_integration {
            Some(mode) => mode,
            None => {
                let nonblank = |v: &Option<String>| {
                    v.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
                };
                if nonblank(&self.git_repo) || nonblank(&self.branch_template) {
                    GitIntegration::Auto
                } else {
                    GitIntegration::Off
                }
            }
        }
    }
}

/// The context a command resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    /// A project root (a folder with a `.nest/`).
    Project(PathBuf),
    /// A workspace root (a folder whose subfolders are projects).
    Workspace(PathBuf),
}

impl Context {
    pub fn root(&self) -> &Path {
        match self {
            Context::Project(p) | Context::Workspace(p) => p,
        }
    }

    pub fn is_project(&self) -> bool {
        matches!(self, Context::Project(_))
    }
}

/// Walks up from `start` to the filesystem root looking for `.nest/config.toml`.
pub fn find_nest(start: &Path) -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        if d.join(NEST_DIR).join(CONFIG_FILE).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Resolves the command context from the current working directory.
pub fn resolve_context() -> Result<Context> {
    let cwd = std::env::current_dir()?;
    resolve_context_at(&cwd)
}

pub fn resolve_context_at(cwd: &Path) -> Result<Context> {
    if let Some(root) = find_nest(cwd) {
        return Ok(Context::Project(root));
    }
    let subs = project_subfolders(cwd);
    if !subs.is_empty() {
        return Ok(Context::Workspace(cwd.to_path_buf()));
    }
    Err(anyhow!(
        "could not find a jay project in this folder or any subfolder - try jay init"
    ))
}

/// Direct subfolders of `dir` that are project roots (have a `.nest/`), sorted.
pub fn project_subfolders(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(NEST_DIR).join(CONFIG_FILE).is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// `jay init [name]` — creates `.nest/` (config + tasks + milestones).
pub fn init_project(dir: &Path, name: Option<&str>) -> Result<()> {
    let pro = dir.join(NEST_DIR);
    if pro.exists() {
        anyhow::bail!("already a jay project here ({NEST_DIR} exists)");
    }
    std::fs::create_dir_all(pro.join(TASKS_DIR))?;
    std::fs::create_dir_all(pro.join(KB_DIR))?;
    let config = ProjectConfig {
        name: name
            .map(|n| n.to_string())
            .unwrap_or_else(|| folder_name(dir)),
        description: String::new(),
        goal: None,
        git_repo: detect_git_repo(dir),
        branch_template: None,
        // New projects are quiet by default: automation must be opted in.
        git_integration: Some(GitIntegration::Off),
        links: Vec::new(),
    };
    std::fs::write(pro.join(".gitignore"), "/lock\n")?;
    save_config(dir, &config)?;
    std::fs::write(pro.join(MILESTONES_FILE), "# milestones\n")?;
    Ok(())
}

pub fn load_config(root: &Path) -> Result<ProjectConfig> {
    let path = root.join(NEST_DIR).join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&text)?)
}

/// Writes a file atomically: a temp file in the same directory, then rename.
/// Readers never observe a partially written file.
pub fn atomic_write(path: &Path, data: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, data)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

pub fn save_config(root: &Path, config: &ProjectConfig) -> Result<()> {
    let path = root.join(NEST_DIR).join(CONFIG_FILE);
    atomic_write(&path, &toml::to_string_pretty(config)?)
}

/// Default project name = the folder name.
pub fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_string())
}

/// Detects the remote origin URL via `git config --get remote.origin.url`.
pub fn detect_git_repo(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn init_creates_layout_and_resolves() {
        let base = tmpdir();
        let proj = base.path().join("myproj");
        fs::create_dir_all(&proj).unwrap();
        init_project(&proj, None).unwrap();
        assert!(proj.join(NEST_DIR).join(CONFIG_FILE).is_file());
        assert!(proj.join(NEST_DIR).join(TASKS_DIR).is_dir());
        assert!(proj.join(NEST_DIR).join(KB_DIR).is_dir());
        assert!(proj.join(NEST_DIR).join(MILESTONES_FILE).is_file());
        let cfg = load_config(&proj).unwrap();
        assert_eq!(cfg.name, "myproj");
        let ctx = resolve_context_at(&proj.join("a/b/c")).unwrap();
        assert_eq!(ctx, Context::Project(proj.clone()));
    }

    #[test]
    fn init_fails_when_pro_exists() {
        let base = tmpdir();
        let proj = base.path().join("p");
        fs::create_dir_all(&proj).unwrap();
        init_project(&proj, None).unwrap();
        assert!(init_project(&proj, None).is_err());
    }

    #[test]
    fn implicit_workspace_when_cwd_has_project_subfolders() {
        let base = tmpdir();
        let a = base.path().join("a");
        let b = base.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        init_project(&a, None).unwrap();
        init_project(&b, None).unwrap();
        let ctx = resolve_context_at(base.path()).unwrap();
        assert_eq!(ctx, Context::Workspace(base.path().to_path_buf()));
        let subs = project_subfolders(base.path());
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn outside_any_project_errors() {
        let base = tmpdir();
        assert!(resolve_context_at(base.path()).is_err());
    }

    #[test]
    fn folder_name_defaults_to_dir_name() {
        assert_eq!(folder_name(Path::new("/x/hello")), "hello");
        assert_eq!(folder_name(Path::new("/")), "project");
    }
}
