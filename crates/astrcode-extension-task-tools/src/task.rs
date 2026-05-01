//! 基于文件的 session-local 任务跟踪存储。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    path::PathBuf,
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

const HIGH_WATER_FILE: &str = ".highwatermark";
const LOCK_FILE: &str = ".lock";
const LOCK_RETRIES: usize = 100;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

/// 任务数据结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    /// 任务唯一标识。
    pub id: String,
    /// 任务主题。
    pub subject: String,
    /// 任务详细描述。
    pub description: String,
    /// 当前任务状态。
    pub status: TaskStatus,
    /// 进行中展示文案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// 可选 owner 字段，仅作为普通数据，不承载调度语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// 此任务阻止的其他任务 ID 列表。
    #[serde(default)]
    pub blocks: Vec<String>,
    /// 阻止此任务的其他任务 ID 列表。
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// 扩展元数据。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// 任务状态枚举。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 待处理。
    Pending,
    /// 进行中。
    InProgress,
    /// 已完成。
    Completed,
}

/// 依赖关系变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyChange {
    /// 用完整集合替换当前 blocks。
    Replace(Vec<String>),
    /// 在当前集合上增删 blocks。
    Patch {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

/// 任务更新输入。
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub status: Option<TaskStatus>,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<Option<String>>,
    pub blocks: Option<DependencyChange>,
}

/// 删除任务后的清理摘要。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DependencyCleanup {
    pub blocks_removed: usize,
    pub blocked_by_removed: usize,
}

/// 基于文件的任务存储。
pub struct TaskStore {
    root: PathBuf,
}

struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl TaskStore {
    /// 创建新的任务存储实例，root 为 JSON 文件存放目录。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 创建新任务。
    pub fn create(
        &self,
        subject: &str,
        description: &str,
        active_form: Option<String>,
        blocks: Vec<String>,
    ) -> Result<Task, String> {
        validate_text("subject", subject)?;
        validate_text("description", description)?;
        validate_optional_text("activeForm", active_form.as_deref())?;

        let _lock = self.lock()?;
        let mut tasks = self.load_all_map()?;
        validate_dependency_ids("", &blocks, &tasks)?;

        let id = self.next_id(&tasks)?;
        let task = Task {
            id: id.clone(),
            subject: subject.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            active_form,
            owner: None,
            blocks: unique_sorted(blocks),
            blocked_by: Vec::new(),
            metadata: BTreeMap::new(),
        };
        tasks.insert(id, task.clone());
        normalize_blocked_by(&mut tasks);
        self.save_all(tasks.values())?;
        Ok(task)
    }

    /// 读取单个任务。
    pub fn get(&self, id: &str) -> Result<Option<Task>, String> {
        self.load(id)
    }

    /// 列出所有任务，按数值 ID 排序。
    pub fn list(&self) -> Result<Vec<Task>, String> {
        let mut tasks = self.load_all_map()?.into_values().collect::<Vec<_>>();
        tasks.sort_by(compare_task_id);
        Ok(tasks)
    }

    /// 更新任务。
    pub fn update(&self, id: &str, update: TaskUpdate) -> Result<Task, String> {
        let _lock = self.lock()?;
        let mut tasks = self.load_all_map()?;
        {
            let task = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            apply_basic_update(task, &update)?;
        }

        if let Some(change) = update.blocks {
            let current = tasks
                .get(id)
                .ok_or_else(|| format!("task '{id}' not found"))?
                .blocks
                .clone();
            let next_blocks = apply_dependency_change(current, change);
            validate_dependency_ids(id, &next_blocks, &tasks)?;
            if let Some(task) = tasks.get_mut(id) {
                task.blocks = unique_sorted(next_blocks);
            }
        }

        normalize_blocked_by(&mut tasks);
        let updated = tasks
            .get(id)
            .cloned()
            .ok_or_else(|| format!("task '{id}' not found"))?;
        self.save_all(tasks.values())?;
        Ok(updated)
    }

    /// 删除任务并清理所有依赖边。
    pub fn delete(&self, id: &str) -> Result<DependencyCleanup, String> {
        let _lock = self.lock()?;
        let mut tasks = self.load_all_map()?;
        let removed = tasks
            .remove(id)
            .ok_or_else(|| format!("task '{id}' not found"))?;
        self.bump_high_water_from_id(id)?;

        let mut cleanup = DependencyCleanup {
            blocks_removed: removed.blocks.len(),
            blocked_by_removed: removed.blocked_by.len(),
        };
        for task in tasks.values_mut() {
            let before_blocks = task.blocks.len();
            task.blocks.retain(|value| value != id);
            cleanup.blocks_removed += before_blocks - task.blocks.len();

            let before_blocked_by = task.blocked_by.len();
            task.blocked_by.retain(|value| value != id);
            cleanup.blocked_by_removed += before_blocked_by - task.blocked_by.len();
        }

        normalize_blocked_by(&mut tasks);
        let path = self.path(id);
        std::fs::remove_file(&path).map_err(|error| format!("delete task {id}: {error}"))?;
        self.save_all(tasks.values())?;
        Ok(cleanup)
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create task directory {}: {e}", self.root.display()))
    }

    fn lock(&self) -> Result<StoreLock, String> {
        self.ensure_dir()?;
        let path = self.root.join(LOCK_FILE);
        for _ in 0..LOCK_RETRIES {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(StoreLock { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    thread::sleep(LOCK_RETRY_DELAY);
                },
                Err(error) => return Err(format!("acquire task lock: {error}")),
            }
        }
        Err("acquire task lock: timed out".to_string())
    }

    fn next_id(&self, tasks: &BTreeMap<String, Task>) -> Result<String, String> {
        let highest_existing = tasks
            .keys()
            .filter_map(|id| id.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        let high_water = self.read_high_water()?.max(highest_existing);
        let next = high_water + 1;
        self.write_high_water(next)?;
        Ok(next.to_string())
    }

    fn read_high_water(&self) -> Result<u64, String> {
        let path = self.root.join(HIGH_WATER_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .trim()
                .parse::<u64>()
                .map_err(|error| format!("read high water mark: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(format!("read high water mark: {error}")),
        }
    }

    fn write_high_water(&self, value: u64) -> Result<(), String> {
        let path = self.root.join(HIGH_WATER_FILE);
        std::fs::write(&path, value.to_string())
            .map_err(|error| format!("write high water mark: {error}"))
    }

    fn bump_high_water_from_id(&self, id: &str) -> Result<(), String> {
        let Ok(value) = id.parse::<u64>() else {
            return Ok(());
        };
        if value > self.read_high_water()? {
            self.write_high_water(value)?;
        }
        Ok(())
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn load(&self, id: &str) -> Result<Option<Task>, String> {
        let path = self.path(id);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map(Some)
                .map_err(|error| format!("parse task {id}: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read task {id}: {error}")),
        }
    }

    fn load_all_map(&self) -> Result<BTreeMap<String, Task>, String> {
        let mut tasks = BTreeMap::new();
        match std::fs::read_dir(&self.root) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|error| format!("read task entry: {error}"))?;
                    let path = entry.path();
                    if !is_task_path(&path) {
                        continue;
                    }
                    let content = std::fs::read_to_string(&path)
                        .map_err(|error| format!("read task {}: {error}", path.display()))?;
                    let task = serde_json::from_str::<Task>(&content)
                        .map_err(|error| format!("parse task {}: {error}", path.display()))?;
                    tasks.insert(task.id.clone(), task);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("read task directory: {error}")),
        }
        Ok(tasks)
    }

    fn save_all<'a>(&self, tasks: impl IntoIterator<Item = &'a Task>) -> Result<(), String> {
        self.ensure_dir()?;
        for task in tasks {
            self.save(task)?;
        }
        Ok(())
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

fn apply_basic_update(task: &mut Task, update: &TaskUpdate) -> Result<(), String> {
    if let Some(subject) = &update.subject {
        validate_text("subject", subject)?;
        task.subject = subject.clone();
    }
    if let Some(description) = &update.description {
        validate_text("description", description)?;
        task.description = description.clone();
    }
    if let Some(active_form) = &update.active_form {
        validate_optional_text("activeForm", active_form.as_deref())?;
        task.active_form = active_form.clone();
    }
    if let Some(status) = update.status {
        task.status = status;
    }
    validate_task_state(task)
}

fn validate_task_state(task: &Task) -> Result<(), String> {
    if task.status == TaskStatus::InProgress
        && task
            .active_form
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err("activeForm required when status is in_progress".to_string());
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

fn validate_optional_text(field: &str, value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(format!("{field} must not be blank when provided"));
    }
    Ok(())
}

fn validate_dependency_ids(
    task_id: &str,
    ids: &[String],
    tasks: &BTreeMap<String, Task>,
) -> Result<(), String> {
    for id in ids {
        if !task_id.is_empty() && id == task_id {
            return Err("task cannot block itself".to_string());
        }
        if !tasks.contains_key(id) {
            return Err(format!("dependency task '{id}' not found"));
        }
    }
    Ok(())
}

fn apply_dependency_change(current: Vec<String>, change: DependencyChange) -> Vec<String> {
    match change {
        DependencyChange::Replace(next) => next,
        DependencyChange::Patch { add, remove } => {
            let remove = remove.into_iter().collect::<BTreeSet<_>>();
            let mut next = current
                .into_iter()
                .filter(|id| !remove.contains(id))
                .collect::<Vec<_>>();
            next.extend(add);
            next
        },
    }
}

fn normalize_blocked_by(tasks: &mut BTreeMap<String, Task>) {
    for task in tasks.values_mut() {
        task.blocks = unique_sorted(std::mem::take(&mut task.blocks));
        task.blocked_by.clear();
    }

    let edges = tasks
        .values()
        .flat_map(|task| {
            task.blocks
                .iter()
                .map(|blocked_id| (task.id.clone(), blocked_id.clone()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    for (blocker_id, blocked_id) in edges {
        if let Some(blocked) = tasks.get_mut(&blocked_id) {
            blocked.blocked_by.push(blocker_id);
        }
    }

    for task in tasks.values_mut() {
        task.blocked_by = unique_sorted(std::mem::take(&mut task.blocked_by));
    }
}

fn unique_sorted(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn compare_task_id(a: &Task, b: &Task) -> std::cmp::Ordering {
    match (a.id.parse::<u64>(), b.id.parse::<u64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.id.cmp(&b.id),
    }
}

fn is_task_path(path: &std::path::Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("json")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn test_store(name: &str) -> TaskStore {
        let root = std::env::temp_dir().join("astrcode-task-tests").join(name);
        let _ = std::fs::remove_dir_all(&root);
        TaskStore::new(root)
    }

    #[test]
    fn creates_gets_and_lists_tasks() {
        let store = test_store("task-create-list");
        let task = store
            .create("test subject", "test desc", None, vec![])
            .expect("create task");

        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(store.get(&task.id).unwrap(), Some(task.clone()));
        assert!(store.list().unwrap().iter().any(|x| x.id == task.id));
    }

    #[test]
    fn requires_active_form_for_in_progress_tasks() {
        let store = test_store("task-active-form");
        let task = store
            .create("update test", "desc", None, vec![])
            .expect("create task");

        let error = store
            .update(
                &task.id,
                TaskUpdate {
                    status: Some(TaskStatus::InProgress),
                    ..TaskUpdate::default()
                },
            )
            .expect_err("in_progress without activeForm should fail");

        assert!(error.contains("activeForm required"));

        let updated = store
            .update(
                &task.id,
                TaskUpdate {
                    status: Some(TaskStatus::InProgress),
                    active_form: Some(Some("Working on task".to_string())),
                    ..TaskUpdate::default()
                },
            )
            .expect("activeForm should satisfy in_progress");
        assert_eq!(updated.status, TaskStatus::InProgress);
    }

    #[test]
    fn high_water_id_does_not_reuse_deleted_ids() {
        let store = test_store("task-high-water");
        let first = store.create("first", "desc", None, vec![]).unwrap();
        assert_eq!(first.id, "1");

        store.delete(&first.id).unwrap();
        let second = store.create("second", "desc", None, vec![]).unwrap();

        assert_eq!(second.id, "2");
    }

    #[test]
    fn dependency_updates_keep_reverse_edges_consistent() {
        let store = test_store("task-deps");
        let blocker = store.create("blocker", "desc", None, vec![]).unwrap();
        let blocked = store
            .create("blocked", "desc", None, vec![blocker.id.clone()])
            .unwrap();

        let blocker_after_create = store.get(&blocker.id).unwrap().unwrap();
        assert_eq!(blocked.blocks, vec![blocker.id.clone()]);
        assert_eq!(blocker_after_create.blocked_by, vec![blocked.id.clone()]);

        store
            .update(
                &blocked.id,
                TaskUpdate {
                    blocks: Some(DependencyChange::Patch {
                        add: vec![],
                        remove: vec![blocker.id.clone()],
                    }),
                    ..TaskUpdate::default()
                },
            )
            .unwrap();

        assert!(store.get(&blocked.id).unwrap().unwrap().blocks.is_empty());
        assert!(
            store
                .get(&blocker.id)
                .unwrap()
                .unwrap()
                .blocked_by
                .is_empty()
        );
    }

    #[test]
    fn dependency_validation_rejects_missing_and_self_references() {
        let store = test_store("task-dep-validation");
        let task = store.create("task", "desc", None, vec![]).unwrap();

        let missing = store
            .update(
                &task.id,
                TaskUpdate {
                    blocks: Some(DependencyChange::Replace(vec!["missing".to_string()])),
                    ..TaskUpdate::default()
                },
            )
            .expect_err("missing dependency should fail");
        assert!(missing.contains("dependency task 'missing' not found"));

        let self_ref = store
            .update(
                &task.id,
                TaskUpdate {
                    blocks: Some(DependencyChange::Replace(vec![task.id.clone()])),
                    ..TaskUpdate::default()
                },
            )
            .expect_err("self dependency should fail");
        assert!(self_ref.contains("task cannot block itself"));
    }

    #[test]
    fn delete_cleans_dependency_edges() {
        let store = test_store("task-delete-deps");
        let blocker = store.create("blocker", "desc", None, vec![]).unwrap();
        let blocked = store
            .create("blocked", "desc", None, vec![blocker.id.clone()])
            .unwrap();

        let cleanup = store.delete(&blocker.id).unwrap();

        assert!(cleanup.blocks_removed > 0);
        assert!(store.get(&blocker.id).unwrap().is_none());
        assert!(store.get(&blocked.id).unwrap().unwrap().blocks.is_empty());
    }

    #[test]
    fn concurrent_creates_get_unique_ids() {
        let store = Arc::new(test_store("task-concurrent-create"));
        let handles = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .create(&format!("task {index}"), "desc", None, vec![])
                        .expect("create task")
                        .id
                })
            })
            .collect::<Vec<_>>();

        let mut ids = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread should join"))
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();

        assert_eq!(ids.len(), 8);
        assert_eq!(store.list().unwrap().len(), 8);
    }
}
