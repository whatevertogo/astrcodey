//! Project memory: recall at turn end, deliver on the next turn's first LLM request.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use astrcode_extension_sdk::{
    extension::{
        ExchangeSummary, ExtensionCall, ExtensionError, ExtensionPaths, HookResult,
        LifecycleContext, LifecycleHandler, PreparedProviderContribution, PreparedProviderEffect,
        ProviderContext, ProviderContributionHandler, ProviderContributionId,
        ProviderSettlementContext,
    },
    llm::LlmMessage,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::{config::MemoryConfig, prompts, store::MemoryStorePool};

/// 用户偏好的 per-session 只读快照。
///
/// `MemoryExtension` 是全局共享单例（runner 在 bootstrap 时创建一次，所有
/// session 复用同一实例），所以这里按 `session_id` 隔离缓存。一个 session
/// 首次加载后，整个 session 生命期内只返回同一份内容——`memory_save` 写入
/// 新偏好不影响它，system prompt 指纹保持稳定，KV cache 不被破坏。只有下一
/// 个 session 的 SessionStart 才重新加载最新值。
///
/// 缓存随活跃 session 增长，在 `stop()`（扩展卸载）时整体清空。进程重启后
/// 内存清零，resume 的 session 会重新加载。
#[derive(Default)]
pub(crate) struct SessionPrefsCache {
    state: Mutex<HashMap<String, Vec<String>>>,
}

impl SessionPrefsCache {
    /// 返回 session 的 user_prefs；首次调用加载并缓存，之后同 session 只读。
    pub(crate) fn lines_for_session(
        &self,
        session_id: &str,
        load: impl FnOnce() -> std::io::Result<Vec<String>>,
    ) -> std::io::Result<Vec<String>> {
        let mut guard = self.state.lock();
        if let Some(lines) = guard.get(session_id) {
            return Ok(lines.clone());
        }
        let lines = load()?;
        guard.insert(session_id.to_string(), lines.clone());
        Ok(lines)
    }

    /// SessionStart 时主动预加载，把注入时机锚定在 session 边界。
    /// 已缓存则跳过（幂等），避免覆盖 PromptBuild 的兜底加载值。
    pub(crate) fn preload_for_session(
        &self,
        session_id: &str,
        load: impl FnOnce() -> std::io::Result<Vec<String>>,
    ) -> std::io::Result<()> {
        let mut guard = self.state.lock();
        if guard.contains_key(session_id) {
            return Ok(());
        }
        let lines = load()?;
        guard.insert(session_id.to_string(), lines);
        Ok(())
    }

    pub(crate) fn reset(&self) {
        self.state.lock().clear();
    }
}

const PROJECT_RECALL_DIR: &str = "project-recall";
const PROJECT_RECALL_STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PendingProjectRecall {
    id: String,
    lines: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProjectRecallState {
    pending: Option<PendingProjectRecall>,
}

struct ProjectRecallStore {
    root: PathBuf,
}

impl ProjectRecallStore {
    fn from_paths(paths: &ExtensionPaths) -> Result<Self, ExtensionError> {
        let root = paths
            .session_data_dir()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?
            .join(PROJECT_RECALL_DIR);
        Ok(Self { root })
    }

    fn replace(&self, lines: Vec<String>) -> Result<(), ExtensionError> {
        let state = ProjectRecallState {
            pending: (!lines.is_empty()).then(|| PendingProjectRecall {
                id: uuid::Uuid::new_v4().to_string(),
                lines,
            }),
        };
        astrcode_extension_sdk::hostpaths::write_json_state(
            &self.root.join(PROJECT_RECALL_STATE_FILE),
            &state,
        )
        .map_err(|error| ExtensionError::Internal(format!("save project recall: {error}")))
    }

    fn pending(&self) -> Result<Option<PendingProjectRecall>, ExtensionError> {
        let state = astrcode_extension_sdk::hostpaths::read_json_state(
            &self.root.join(PROJECT_RECALL_STATE_FILE),
        )
        .map_err(|error| ExtensionError::Internal(format!("read project recall: {error}")))?;
        Ok(state.and_then(|state: ProjectRecallState| state.pending))
    }

    fn acknowledge(&self, contribution_id: &str) -> Result<(), ExtensionError> {
        let path = self.root.join(PROJECT_RECALL_STATE_FILE);
        astrcode_extension_sdk::hostpaths::update_json_state(
            &path,
            |state: Option<ProjectRecallState>| {
                let Some(mut state) = state else {
                    return Ok((None, ()));
                };
                if state
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.id != contribution_id)
                {
                    return Ok((None, ()));
                }
                state.pending = None;
                Ok((Some(state), ()))
            },
        )
        .map_err(|error| ExtensionError::Internal(format!("ack project recall: {error}")))
    }
}

pub(crate) struct MemoryProjectRecallTurnEndHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemoryProjectRecallTurnEndHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let cfg = self.config.read().clone();
        if !cfg.inject_project_memories_per_turn {
            return Ok(HookResult::Allow);
        }
        let Some(exchange) = ctx.last_exchange() else {
            return Ok(HookResult::Allow);
        };

        let query = recall_query_from_exchange(exchange);
        if query.chars().count() < cfg.min_recall_query_chars {
            return Ok(HookResult::Allow);
        }

        let store_pool = self.store_pool.clone();
        let working_dir = ctx.working_dir().to_string_lossy().into_owned();

        let lines = tokio::task::spawn_blocking(move || {
            recall_project_lines(
                &store_pool,
                &working_dir,
                &query,
                cfg.max_injected_project_memories,
                cfg.min_project_memory_score,
                cfg.max_injected_memory_chars,
            )
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        ProjectRecallStore::from_paths(ctx.paths())?.replace(lines)?;
        Ok(HookResult::Allow)
    }
}

/// Delivers project memories prepared at the previous turn's end (does not rank here).
pub(crate) struct MemoryProjectRecallDeliveryProvider {
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl ProviderContributionHandler for MemoryProjectRecallDeliveryProvider {
    async fn prepare(
        &self,
        ctx: ProviderContext,
    ) -> Result<Option<PreparedProviderContribution>, ExtensionError> {
        if !self.config.read().inject_project_memories_per_turn {
            return Ok(None);
        }
        let Some(pending) = ProjectRecallStore::from_paths(ctx.paths())?.pending()? else {
            return Ok(None);
        };
        Ok(Some(PreparedProviderContribution::new(
            ProviderContributionId::new(pending.id),
            PreparedProviderEffect::AppendMessages(vec![LlmMessage::user(
                prompts::project_memory_injection(&pending.lines),
            )]),
        )))
    }

    async fn acknowledge(&self, ctx: ProviderSettlementContext) -> Result<(), ExtensionError> {
        ProjectRecallStore::from_paths(ctx.paths())?.acknowledge(ctx.contribution_id().as_str())
    }
}

fn recall_query_from_exchange(exchange: &ExchangeSummary) -> String {
    let user = exchange.user_message.trim();
    let assistant = exchange.assistant_message.trim();
    if assistant.is_empty() {
        user.to_string()
    } else {
        format!("{user}\n\n{assistant}")
    }
}

fn recall_project_lines(
    pool: &MemoryStorePool,
    working_dir: &str,
    query: &str,
    limit: usize,
    min_score: f64,
    max_chars: usize,
) -> std::io::Result<Vec<String>> {
    let scoped = pool.get_scoped(working_dir)?;
    let ranked = scoped
        .project
        .memory_index()
        .rank_for_query(query, limit, min_score)?;
    let lines: Vec<String> = ranked.into_iter().map(|(_, line)| line).collect();
    Ok(trim_lines_to_char_budget(lines, max_chars))
}

fn trim_lines_to_char_budget(lines: Vec<String>, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let need = line.len() + if out.is_empty() { 0 } else { 1 };
        if used + need > max_chars {
            break;
        }
        used += need;
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::index::{MemoryIndex, MemorySource};

    #[test]
    fn project_recall_survives_reload_and_acknowledges_exact_revision() {
        let root = TempDir::new().unwrap();
        let store = ProjectRecallStore {
            root: root.path().to_path_buf(),
        };
        store.replace(vec!["first".into()]).unwrap();
        let first = store.pending().unwrap().unwrap();

        let reloaded = ProjectRecallStore {
            root: root.path().to_path_buf(),
        };
        assert_eq!(reloaded.pending().unwrap().unwrap(), first);
        reloaded.replace(vec!["newer".into()]).unwrap();
        let newer = reloaded.pending().unwrap().unwrap();
        reloaded.acknowledge(&first.id).unwrap();
        assert_eq!(reloaded.pending().unwrap().as_ref(), Some(&newer));
        reloaded.acknowledge(&newer.id).unwrap();
        assert!(reloaded.pending().unwrap().is_none());
    }

    #[test]
    fn trim_lines_respects_char_budget() {
        let lines = vec!["a".repeat(100), "b".repeat(100)];
        let out = trim_lines_to_char_budget(lines, 150);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn rank_for_query_filters_low_scores() {
        let tmp = TempDir::new().unwrap();
        let index = MemoryIndex::new(tmp.path());
        index
            .upsert_record(
                "Uses Rust for all backend services in this repo",
                "project_ctx",
                MemorySource::Manual,
                None,
                &["rust".to_string()],
                None,
            )
            .unwrap();
        index
            .upsert_record(
                "Team lunch is on Fridays",
                "general",
                MemorySource::Manual,
                None,
                &[],
                None,
            )
            .unwrap();

        let ranked = index
            .rank_for_query("refactor the Rust backend API", 5, 0.2)
            .unwrap();
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].1.contains("Rust"));
    }

    #[test]
    fn session_prefs_cache_hits_same_session() {
        let cache = SessionPrefsCache::default();
        let a = cache
            .lines_for_session("s1", || Ok(vec!["pref".to_string()]))
            .unwrap();
        let b = cache
            .lines_for_session("s1", || Ok(vec!["other".to_string()]))
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a, vec!["pref".to_string()]);
    }

    #[test]
    fn session_prefs_cache_isolates_sessions() {
        let cache = SessionPrefsCache::default();
        cache
            .lines_for_session("s1", || Ok(vec!["a".to_string()]))
            .unwrap();
        // s2 加载不应覆盖 s1（单槽缓存的旧 bug）
        cache
            .lines_for_session("s2", || Ok(vec!["b".to_string()]))
            .unwrap();
        let s1_again = cache
            .lines_for_session("s1", || Ok(vec!["SHOULD_NOT_LOAD".to_string()]))
            .unwrap();
        assert_eq!(s1_again, vec!["a".to_string()]);
    }

    #[test]
    fn preload_is_idempotent_and_does_not_clobber() {
        let cache = SessionPrefsCache::default();
        // PromptBuild 兜底先加载
        let prompt_load = cache
            .lines_for_session("s1", || Ok(vec!["from-prompt".to_string()]))
            .unwrap();
        // SessionStart 预加载幂等跳过，不覆盖 PromptBuild 的值
        cache
            .preload_for_session("s1", || Ok(vec!["from-session-start".to_string()]))
            .unwrap();
        let again = cache
            .lines_for_session("s1", || Ok(vec!["never".to_string()]))
            .unwrap();
        assert_eq!(prompt_load, again);
    }
}
