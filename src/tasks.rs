//! Task and milestone persistence: one TOML file per task, one TOML list for
//! milestones — plain text, so `git diff`/`log`/`merge` work directly.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::model::{Milestone, Task};
use crate::project::{MILESTONES_FILE, NEST_DIR, TASKS_DIR};

fn tasks_dir(root: &Path) -> PathBuf {
    root.join(NEST_DIR).join(TASKS_DIR)
}

/// Path of a task's TOML file (`<root>/.nest/tasks/<id>.toml`).
pub fn task_file(root: &Path, id: i64) -> PathBuf {
    tasks_dir(root).join(format!("{id}.toml"))
}

/// Loads every task in the project, ordered by id.
pub fn load_tasks(root: &Path) -> Result<Vec<Task>> {
    let dir = tasks_dir(root);
    let mut tasks: Vec<Task> = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            tasks.push(toml::from_str(&text)?);
        }
    }
    tasks.sort_by_key(|t| t.id);
    Ok(tasks)
}

/// Loads a single task by id.
pub fn load_task(root: &Path, id: i64) -> Result<Option<Task>> {
    let path = task_file(root, id);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(toml::from_str(&text)?))
}

/// Saves a task to `.nest/tasks/<id>.toml` atomically (creating the dir as needed).
pub fn save_task(root: &Path, task: &Task) -> Result<()> {
    let dir = tasks_dir(root);
    std::fs::create_dir_all(&dir)?;
    crate::project::atomic_write(&task_file(root, task.id), &toml::to_string_pretty(task)?)
}

/// Deletes a task file (no-op if absent).
pub fn delete_task(root: &Path, id: i64) -> Result<()> {
    let path = task_file(root, id);
    if path.is_file() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Next available id = max(existing numeric file names) + 1 (1 if none).
pub fn next_id(root: &Path) -> Result<i64> {
    let dir = tasks_dir(root);
    let mut max = 0i64;
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".toml") {
                if let Ok(id) = stem.parse::<i64>() {
                    max = max.max(id);
                }
            }
        }
    }
    Ok(max + 1)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct MilestonesFile {
    #[serde(default)]
    milestones: Vec<Milestone>,
}

/// Loads milestones (empty list if the file is absent).
pub fn load_milestones(root: &Path) -> Result<Vec<Milestone>> {
    let path = root.join(NEST_DIR).join(MILESTONES_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(toml::from_str::<MilestonesFile>(&text)?.milestones)
}

/// Saves milestones as a `[[milestones]]` array of tables (atomically).
pub fn save_milestones(root: &Path, ms: &[Milestone]) -> Result<()> {
    let path = root.join(NEST_DIR).join(MILESTONES_FILE);
    crate::project::atomic_write(
        &path,
        &toml::to_string_pretty(&MilestonesFile {
            milestones: ms.to_vec(),
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{now_ts, TaskStatus};
    use crate::project::init_project;
    use std::fs;

    fn proj() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        let p = base.path().join("p");
        fs::create_dir_all(&p).unwrap();
        init_project(&p, None).unwrap();
        base
    }

    #[test]
    fn roundtrip_task_and_next_id() {
        let root = proj();
        let r = root.path();
        assert_eq!(next_id(r).unwrap(), 1);
        let t = Task::new(1, "first".into(), "do it".into());
        save_task(r, &t).unwrap();
        assert_eq!(next_id(r).unwrap(), 2);
        let loaded = load_task(r, 1).unwrap().unwrap();
        assert_eq!(loaded.title, "first");
        assert_eq!(loaded.status, TaskStatus::Open);
        save_task(r, &Task::new(5, "five".into(), "".into())).unwrap();
        assert_eq!(next_id(r).unwrap(), 6);
        delete_task(r, 1).unwrap();
        assert!(load_task(r, 1).unwrap().is_none());
    }

    #[test]
    fn milestones_roundtrip() {
        let root = proj();
        let r = root.path();
        assert!(load_milestones(r).unwrap().is_empty());
        let ms = vec![Milestone {
            id: 1,
            name: "v1.0".into(),
            target_date: None,
            done_at: None,
            created_at: now_ts(),
        }];
        save_milestones(r, &ms).unwrap();
        let back = load_milestones(r).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "v1.0");
    }
}
