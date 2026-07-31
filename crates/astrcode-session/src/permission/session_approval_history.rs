use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::permission::ApprovalDecision;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use super::{PermissionContext, PermissionDecision, PermissionPolicy};

const APPROVAL_HISTORY_TEMPFILE_PREFIX: &str = ".approval-history.";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalHistoryFile {
    #[serde(default)]
    allowed_always: BTreeSet<String>,
    #[serde(default)]
    denied_always: BTreeSet<String>,
}

/// 会话级 AllowAlways / DenyAlways 记忆。
pub struct ApprovalHistoryStore {
    inner: Mutex<ApprovalHistoryFile>,
    persistence: AsyncMutex<ApprovalHistoryPersistence>,
}

enum ApprovalHistoryPersistence {
    Uninitialized,
    Ready(Option<PathBuf>),
    Failed(ApprovalHistoryError),
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ApprovalHistoryError {
    message: String,
}

impl ApprovalHistoryError {
    fn new(operation: &str, path: &Path, error: impl std::fmt::Display) -> Self {
        Self {
            message: format!(
                "failed to {operation} session approval history at {}: {error}",
                path.display()
            ),
        }
    }

    fn path_changed(previous: Option<&Path>, current: Option<&Path>) -> Self {
        Self {
            message: format!(
                "session approval history path changed from {} to {}",
                display_optional_path(previous),
                display_optional_path(current)
            ),
        }
    }

    fn uninitialized() -> Self {
        Self {
            message: "session approval history was used before initialization".into(),
        }
    }
}

impl Default for ApprovalHistoryStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ApprovalHistoryFile::default()),
            persistence: AsyncMutex::new(ApprovalHistoryPersistence::Uninitialized),
        }
    }
}

impl ApprovalHistoryStore {
    pub(crate) async fn ensure_loaded(
        &self,
        path: Option<&Path>,
    ) -> Result<(), ApprovalHistoryError> {
        let mut persistence = self.persistence.lock().await;
        match &*persistence {
            ApprovalHistoryPersistence::Ready(previous) => {
                if previous.as_deref() == path {
                    return Ok(());
                }
                return Err(ApprovalHistoryError::path_changed(
                    previous.as_deref(),
                    path,
                ));
            },
            ApprovalHistoryPersistence::Failed(error) => return Err(error.clone()),
            ApprovalHistoryPersistence::Uninitialized => {},
        }

        let loaded = match path {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(text) => serde_json::from_str(&text)
                    .map_err(|error| ApprovalHistoryError::new("parse", path, error)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(ApprovalHistoryFile::default())
                },
                Err(error) => Err(ApprovalHistoryError::new("read", path, error)),
            },
            None => Ok(ApprovalHistoryFile::default()),
        };

        match loaded {
            Ok(history) => {
                *self.inner.lock() = history;
                *persistence = ApprovalHistoryPersistence::Ready(path.map(Path::to_path_buf));
                Ok(())
            },
            Err(error) => {
                *persistence = ApprovalHistoryPersistence::Failed(error.clone());
                Err(error)
            },
        }
    }

    pub fn is_allowed_always(&self, rule_key: &str) -> bool {
        self.inner.lock().allowed_always.contains(rule_key)
    }

    pub fn is_denied_always(&self, rule_key: &str) -> bool {
        self.inner.lock().denied_always.contains(rule_key)
    }

    pub(crate) async fn record_decision(
        &self,
        rule_key: Option<&str>,
        decision: ApprovalDecision,
    ) -> Result<(), ApprovalHistoryError> {
        let Some(key) = rule_key.filter(|k| !k.is_empty()) else {
            return Ok(());
        };
        if !matches!(
            decision,
            ApprovalDecision::AllowAlways | ApprovalDecision::DenyAlways
        ) {
            return Ok(());
        }

        let mut persistence = self.persistence.lock().await;
        let path = match &*persistence {
            ApprovalHistoryPersistence::Ready(path) => path.clone(),
            ApprovalHistoryPersistence::Failed(error) => return Err(error.clone()),
            ApprovalHistoryPersistence::Uninitialized => {
                return Err(ApprovalHistoryError::uninitialized());
            },
        };

        let mut candidate = self.inner.lock().clone();
        candidate.record_decision(key, decision);
        if let Some(path) = path.as_deref() {
            if let Err(error) = persist_history(path, &candidate).await {
                *persistence = ApprovalHistoryPersistence::Failed(error.clone());
                return Err(error);
            }
        }
        *self.inner.lock() = candidate;
        Ok(())
    }
}

impl ApprovalHistoryFile {
    fn record_decision(&mut self, key: &str, decision: ApprovalDecision) {
        match decision {
            ApprovalDecision::AllowAlways => {
                self.denied_always.remove(key);
                self.allowed_always.insert(key.to_string());
            },
            ApprovalDecision::DenyAlways => {
                self.allowed_always.remove(key);
                self.denied_always.insert(key.to_string());
            },
            ApprovalDecision::AllowOnce | ApprovalDecision::DenyOnce => {},
        }
    }
}

async fn persist_history(
    path: &Path,
    history: &ApprovalHistoryFile,
) -> Result<(), ApprovalHistoryError> {
    let text = serde_json::to_string_pretty(history)
        .map_err(|error| ApprovalHistoryError::new("serialize", path, error))?;
    let path = path.to_path_buf();
    let task_path = path.clone();
    tokio::task::spawn_blocking(move || replace_history_file(&task_path, text.as_bytes()))
        .await
        .map_err(|error| ApprovalHistoryError::new("join persistence task for", &path, error))?
}

fn replace_history_file(path: &Path, content: &[u8]) -> Result<(), ApprovalHistoryError> {
    let parent = path.parent().ok_or_else(|| {
        ApprovalHistoryError::new(
            "resolve parent directory for",
            path,
            "path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ApprovalHistoryError::new("create directory for", path, error))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(APPROVAL_HISTORY_TEMPFILE_PREFIX);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }

    let mut temporary = builder
        .tempfile_in(parent)
        .map_err(|error| ApprovalHistoryError::new("create temporary file for", path, error))?;
    temporary
        .write_all(content)
        .map_err(|error| ApprovalHistoryError::new("write", path, error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ApprovalHistoryError::new("sync", path, error))?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| ApprovalHistoryError::new("replace", path, error.error))
}

fn display_optional_path(path: Option<&Path>) -> String {
    path.map_or_else(|| "<memory>".into(), |path| path.display().to_string())
}

pub(super) struct SessionApprovalHistoryPolicy {
    store: Arc<ApprovalHistoryStore>,
}

impl SessionApprovalHistoryPolicy {
    pub(super) fn new(store: Arc<ApprovalHistoryStore>) -> Self {
        Self { store }
    }
}

impl PermissionPolicy for SessionApprovalHistoryPolicy {
    fn priority(&self) -> u32 {
        55
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        let inner = self.store.inner.lock();
        for rule_key in history_lookup_keys(ctx) {
            if inner.allowed_always.contains(&rule_key) {
                return PermissionDecision::Allow;
            }
        }
        for rule_key in history_lookup_keys(ctx) {
            if inner.denied_always.contains(&rule_key) {
                return PermissionDecision::Deny {
                    reason: format!("Denied by session approval memory ({rule_key})"),
                };
            }
        }
        PermissionDecision::Pass
    }
}

/// 与链上 Ask 策略写入的 rule_key 对齐的候选键（按优先级顺序）。
fn history_lookup_keys(ctx: &PermissionContext<'_>) -> [String; 3] {
    [
        format!("shell:{}", ctx.tool_name),
        format!("configured:{}", ctx.tool_name),
        format!("tool:{}", ctx.tool_name),
    ]
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalMode;

    use super::*;

    #[tokio::test]
    async fn allow_always_short_circuits() {
        let store = Arc::new(ApprovalHistoryStore::default());
        store.ensure_loaded(None).await.unwrap();
        store
            .record_decision(Some("tool:shell"), ApprovalDecision::AllowAlways)
            .await
            .unwrap();
        let policy = SessionApprovalHistoryPolicy::new(store);
        let input = serde_json::json!({});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        assert_eq!(policy.evaluate(&ctx), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn shell_rule_key_short_circuits() {
        let store = Arc::new(ApprovalHistoryStore::default());
        store.ensure_loaded(None).await.unwrap();
        store
            .record_decision(Some("shell:shell"), ApprovalDecision::AllowAlways)
            .await
            .unwrap();
        let policy = SessionApprovalHistoryPolicy::new(store);
        let input = serde_json::json!({});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        assert_eq!(policy.evaluate(&ctx), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn load_and_persist_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let corrupt_path = temp.path().join("corrupt.json");
        tokio::fs::write(&corrupt_path, "{not json").await.unwrap();

        let corrupt_store = ApprovalHistoryStore::default();
        let error = corrupt_store
            .ensure_loaded(Some(&corrupt_path))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed to parse"));
        assert!(!corrupt_store.is_allowed_always("tool:shell"));

        let persisted_path = temp.path().join("persisted.json");
        let persisted_store = ApprovalHistoryStore::default();
        persisted_store
            .ensure_loaded(Some(&persisted_path))
            .await
            .unwrap();
        persisted_store
            .record_decision(Some("tool:shell"), ApprovalDecision::AllowAlways)
            .await
            .unwrap();
        let reloaded_store = ApprovalHistoryStore::default();
        reloaded_store
            .ensure_loaded(Some(&persisted_path))
            .await
            .unwrap();
        assert!(reloaded_store.is_allowed_always("tool:shell"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = tokio::fs::metadata(&persisted_path)
                .await
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        tokio::fs::write(
            &persisted_path,
            r#"{"allowedAlways":[],"deniedAlways":["tool:shell"]}"#,
        )
        .await
        .unwrap();
        persisted_store
            .ensure_loaded(Some(&persisted_path))
            .await
            .unwrap();
        assert!(persisted_store.is_allowed_always("tool:shell"));
        assert!(!persisted_store.is_denied_always("tool:shell"));

        let blocked_parent = temp.path().join("history");
        tokio::fs::create_dir(&blocked_parent).await.unwrap();
        let history_path = blocked_parent.join("approval-history.json");
        let persistence_store = ApprovalHistoryStore::default();
        persistence_store
            .ensure_loaded(Some(&history_path))
            .await
            .unwrap();
        persistence_store
            .record_decision(Some("tool:shell"), ApprovalDecision::AllowAlways)
            .await
            .unwrap();
        let saved_parent = temp.path().join("saved-history");
        tokio::fs::rename(&blocked_parent, &saved_parent)
            .await
            .unwrap();
        tokio::fs::write(&blocked_parent, "not a directory")
            .await
            .unwrap();

        let error = persistence_store
            .record_decision(Some("tool:shell"), ApprovalDecision::DenyAlways)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed to create directory"));
        assert!(
            persistence_store
                .ensure_loaded(Some(&history_path))
                .await
                .unwrap_err()
                .to_string()
                .contains("failed to create directory")
        );
        assert!(persistence_store.is_allowed_always("tool:shell"));
        assert!(!persistence_store.is_denied_always("tool:shell"));
        tokio::fs::remove_file(&blocked_parent).await.unwrap();
        tokio::fs::rename(&saved_parent, &blocked_parent)
            .await
            .unwrap();
        let preserved_store = ApprovalHistoryStore::default();
        preserved_store
            .ensure_loaded(Some(&history_path))
            .await
            .unwrap();
        assert!(preserved_store.is_allowed_always("tool:shell"));
        assert!(!preserved_store.is_denied_always("tool:shell"));

        let cleanup_parent = temp.path().join("rename-failure");
        tokio::fs::create_dir(&cleanup_parent).await.unwrap();
        let cleanup_path = cleanup_parent.join("approval-history.json");
        let cleanup_store = ApprovalHistoryStore::default();
        cleanup_store
            .ensure_loaded(Some(&cleanup_path))
            .await
            .unwrap();
        tokio::fs::create_dir(&cleanup_path).await.unwrap();
        let error = cleanup_store
            .record_decision(Some("tool:shell"), ApprovalDecision::AllowAlways)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed to replace"));
        assert!(!cleanup_store.is_allowed_always("tool:shell"));
        let mut entries = tokio::fs::read_dir(&cleanup_parent).await.unwrap();
        assert_eq!(
            entries.next_entry().await.unwrap().unwrap().path(),
            cleanup_path
        );
        assert!(entries.next_entry().await.unwrap().is_none());
    }
}
