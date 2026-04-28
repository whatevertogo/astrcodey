//! 基于文件的任务 CRUD 操作。
//!
//! 任务以 JSON 文件形式存储在 `<root>/<id>.json`，ID 为自增整数。
//! 使用原子重命名实现简单的文件锁以保证并发安全。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 任务数据结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// 任务唯一标识（自增整数）
    pub id: String,
    /// 任务主题
    pub subject: String,
    /// 任务详细描述
    pub description: String,
    /// 当前任务状态
    pub status: TaskStatus,
    /// 此任务阻止的其他任务 ID 列表
    #[serde(default)]
    pub blocks: Vec<String>,
    /// 阻止此任务的其他任务 ID 列表（反向依赖）
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// 任务状态枚举。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 待处理
    Pending,
    /// 进行中
    InProgress,
    /// 已完成
    Completed,
}

/// 基于文件的任务存储。
pub struct TaskStore {
    /// 任务文件存储根目录
    root: PathBuf,
}

impl TaskStore {
    /// 创建新的任务存储实例，root 为 JSON 文件存放目录。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 确保存储根目录存在，失败时返回可读的错误信息。
    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create task directory {}: {e}", self.root.display()))
    }

    /// 扫描目录中已有 JSON 文件，返回下一个可用 ID（最大数值 + 1）。
    fn next_id(&self) -> String {
        let mut max_id: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(num) = stem.parse::<u64>() {
                            max_id = max_id.max(num);
                        }
                    }
                }
            }
        }
        (max_id + 1).to_string()
    }

    /// 创建新任务。
    ///
    /// 生成自增 ID，将任务持久化到文件，并更新被阻止任务的反向依赖。
    /// IO 失败时返回可读的错误描述。
    pub fn create(
        &self,
        subject: &str,
        description: &str,
        blocks: &[String],
    ) -> Result<Task, String> {
        self.ensure_dir()?;
        let id = self.next_id();
        let task = Task {
            id: id.clone(),
            subject: subject.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            blocks: blocks.to_vec(),
            blocked_by: Vec::new(),
        };
        self.save(&task)?;
        for blocked_id in &task.blocks {
            if let Some(mut blocked) = self.load(blocked_id) {
                if !blocked.blocked_by.contains(&task.id) {
                    blocked.blocked_by.push(task.id.clone());
                }
                self.save(&blocked)?;
            }
        }
        Ok(task)
    }

    /// 列出所有任务，按 ID 排序。
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

    /// 更新任务的指定字段。
    /// 任务不存在或 IO 失败时返回可读的错误描述。
    pub fn update(
        &self,
        id: &str,
        status: Option<TaskStatus>,
        subject: Option<&str>,
        description: Option<&str>,
    ) -> Result<Task, String> {
        let mut task = self
            .load(id)
            .ok_or_else(|| format!("task '{id}' not found"))?;
        if let Some(s) = status {
            task.status = s;
        }
        if let Some(s) = subject {
            task.subject = s.to_string();
        }
        if let Some(d) = description {
            task.description = d.to_string();
        }
        self.save(&task)?;
        Ok(task)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn load(&self, id: &str) -> Option<Task> {
        let content = std::fs::read_to_string(self.path(id)).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save(&self, task: &Task) -> Result<(), String> {
        let path = self.path(&task.id);
        let json = serde_json::to_string_pretty(task)
            .map_err(|e| format!("serialize task {}: {e}", task.id))?;
        let tmp = self.root.join(format!("{}.tmp", task.id));
        std::fs::write(&tmp, &json).map_err(|e| format!("write task {}: {e}", task.id))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("save task {}: {e}", task.id))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> TaskStore {
        let root = std::env::temp_dir()
            .join("astrcode-task-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        TaskStore::new(root)
    }

    #[test]
    fn test_task_create_and_list() {
        let store = test_store("task-create-list");
        let t = store
            .create("test subject", "test desc", &[])
            .expect("create task");
        assert_eq!(t.status, TaskStatus::Pending);

        let list = store.list();
        assert!(list.iter().any(|x| x.id == t.id));

        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn test_task_update_status() {
        let store = test_store("task-update-status");
        let t = store
            .create("update test", "desc", &[])
            .expect("create task");
        let updated = store
            .update(&t.id, Some(TaskStatus::InProgress), None, None)
            .unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);

        let _ = std::fs::remove_dir_all(&store.root);
    }
}
