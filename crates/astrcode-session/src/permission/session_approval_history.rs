use std::{collections::BTreeSet, path::Path, sync::Arc};

use astrcode_core::permission::ApprovalDecision;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::{PermissionContext, PermissionDecision, PermissionPolicy};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalHistoryFile {
    #[serde(default)]
    allowed_always: BTreeSet<String>,
    #[serde(default)]
    denied_always: BTreeSet<String>,
}

/// 会话级 AllowAlways / DenyAlways 记忆。
#[derive(Default)]
pub struct ApprovalHistoryStore {
    inner: Mutex<ApprovalHistoryFile>,
}

impl ApprovalHistoryStore {
    pub fn load_from(path: &Path) -> Self {
        let inner = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(inner),
        }
    }

    pub fn is_allowed_always(&self, rule_key: &str) -> bool {
        self.inner.lock().allowed_always.contains(rule_key)
    }

    pub fn is_denied_always(&self, rule_key: &str) -> bool {
        self.inner.lock().denied_always.contains(rule_key)
    }

    pub fn replace_from(&self, other: &Self) {
        *self.inner.lock() = other.inner.lock().clone();
    }

    pub fn persist_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let snapshot = self.inner.lock().clone();
        let text = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(path, text)
    }

    pub fn record_decision(&self, rule_key: Option<&str>, decision: ApprovalDecision) {
        let Some(key) = rule_key.filter(|k| !k.is_empty()) else {
            return;
        };
        let mut inner = self.inner.lock();
        match decision {
            ApprovalDecision::AllowAlways => {
                inner.denied_always.remove(key);
                inner.allowed_always.insert(key.to_string());
            },
            ApprovalDecision::DenyAlways => {
                inner.allowed_always.remove(key);
                inner.denied_always.insert(key.to_string());
            },
            ApprovalDecision::AllowOnce | ApprovalDecision::DenyOnce => {},
        }
    }
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

    #[test]
    fn allow_always_short_circuits() {
        let store = Arc::new(ApprovalHistoryStore::default());
        store.record_decision(Some("tool:shell"), ApprovalDecision::AllowAlways);
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

    #[test]
    fn shell_rule_key_short_circuits() {
        let store = Arc::new(ApprovalHistoryStore::default());
        store.record_decision(Some("shell:shell"), ApprovalDecision::AllowAlways);
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
}
