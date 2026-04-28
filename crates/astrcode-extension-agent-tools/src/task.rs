//! File-based task CRUD.
//!
//! Tasks are stored as JSON files in `~/.astrcode/tasks/<id>.json`.
//! Uses simple file locking via atomic rename for concurrency safety.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// File-based task store rooted at `~/.astrcode/tasks/`.
pub struct TaskStore {
    root: PathBuf,
}

impl TaskStore {
    pub fn new() -> Self {
        let root = dirs_fallback()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".astrcode")
            .join("tasks");
        let _ = std::fs::create_dir_all(&root);
        Self { root }
    }

    pub fn create(&self, subject: &str, description: &str, blocks: &[String]) -> Task {
        let id = format!("t{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let task = Task {
            id: id.clone(),
            subject: subject.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            blocks: blocks.to_vec(),
            blocked_by: Vec::new(),
        };
        self.save(&task);
        // Update blocked tasks with reverse deps
        for blocked_id in &task.blocks {
            if let Some(mut blocked) = self.load(blocked_id) {
                if !blocked.blocked_by.contains(&task.id) {
                    blocked.blocked_by.push(task.id.clone());
                }
                self.save(&blocked);
            }
        }
        task
    }

    pub fn list(&self) -> Vec<Task> {
        let mut tasks = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(task) = serde_json::from_str::<Task>(&content) {
                            tasks.push(task);
                        }
                    }
                }
            }
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tasks
    }

    pub fn update(
        &self,
        id: &str,
        status: Option<TaskStatus>,
        subject: Option<&str>,
        description: Option<&str>,
    ) -> Option<Task> {
        let mut task = self.load(id)?;
        if let Some(s) = status {
            task.status = s;
        }
        if let Some(s) = subject {
            task.subject = s.to_string();
        }
        if let Some(d) = description {
            task.description = d.to_string();
        }
        self.save(&task);
        Some(task)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn load(&self, id: &str) -> Option<Task> {
        let content = std::fs::read_to_string(self.path(id)).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save(&self, task: &Task) {
        let path = self.path(&task.id);
        if let Ok(json) = serde_json::to_string_pretty(task) {
            let tmp = self.root.join(format!("{}.tmp", task.id));
            let _ = std::fs::write(&tmp, &json);
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn dirs_fallback() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_create_and_list() {
        let store = TaskStore::new();
        let t = store.create("test subject", "test desc", &[]);
        assert_eq!(t.status, TaskStatus::Pending);

        let list = store.list();
        assert!(list.iter().any(|x| x.id == t.id));

        // Cleanup
        let _ = std::fs::remove_file(store.path(&t.id));
    }

    #[test]
    fn test_task_update_status() {
        let store = TaskStore::new();
        let t = store.create("update test", "desc", &[]);
        let updated = store
            .update(&t.id, Some(TaskStatus::InProgress), None, None)
            .unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);

        let _ = std::fs::remove_file(store.path(&t.id));
    }
}
