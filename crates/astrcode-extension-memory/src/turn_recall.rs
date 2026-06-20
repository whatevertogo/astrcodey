//! Project memory: recall at turn end, deliver on the next turn's first LLM request.

use std::{collections::HashMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::{
        ExchangeSummary, ExtensionError, HookResult, LifecycleContext, LifecycleHandler,
        ProviderContext, ProviderHandler, ProviderResult,
    },
    llm::LlmMessage,
};
use parking_lot::{Mutex, RwLock};

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

/// Project memories ranked at [`ExtensionEvent::TurnEnd`], consumed on next
/// `BeforeProviderRequest`.
#[derive(Default)]
pub(crate) struct ProjectRecallBuffer {
    pending: Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl ProjectRecallBuffer {
    pub(crate) fn store(&self, session_id: &str, lines: Vec<String>) {
        if lines.is_empty() {
            self.pending.lock().remove(session_id);
        } else {
            self.pending.lock().insert(session_id.to_string(), lines);
        }
    }

    pub(crate) fn take(&self, session_id: &str) -> Option<Vec<String>> {
        self.pending.lock().remove(session_id)
    }

    pub(crate) fn reset(&self) {
        self.pending.lock().clear();
    }
}

pub(crate) struct MemoryProjectRecallTurnEndHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub buffer: Arc<ProjectRecallBuffer>,
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemoryProjectRecallTurnEndHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let cfg = self.config.read().clone();
        if !cfg.inject_project_memories_per_turn {
            return Ok(HookResult::Allow);
        }
        let Some(exchange) = ctx.last_exchange else {
            return Ok(HookResult::Allow);
        };

        let query = recall_query_from_exchange(&exchange);
        if query.chars().count() < cfg.min_recall_query_chars {
            return Ok(HookResult::Allow);
        }

        let store_pool = self.store_pool.clone();
        let working_dir = ctx.working_dir.clone();
        let buffer = self.buffer.clone();
        let session_id = ctx.session_id.clone();

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

        buffer.store(&session_id, lines);
        Ok(HookResult::Allow)
    }
}

/// Delivers project memories prepared at the previous turn's end (does not rank here).
pub(crate) struct MemoryProjectRecallDeliveryProvider {
    pub buffer: Arc<ProjectRecallBuffer>,
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl ProviderHandler for MemoryProjectRecallDeliveryProvider {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        if !self.config.read().inject_project_memories_per_turn {
            return Ok(ProviderResult::Allow);
        }
        let Some(lines) = self.buffer.take(&ctx.session_id) else {
            return Ok(ProviderResult::Allow);
        };
        if lines.is_empty() {
            return Ok(ProviderResult::Allow);
        }
        Ok(ProviderResult::AppendMessages {
            messages: vec![LlmMessage::user(prompts::project_memory_injection(&lines))],
        })
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

pub(crate) fn recall_project_lines(
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
    use astrcode_extension_sdk::hostpaths::ensure_dir;
    use tempfile::TempDir;

    use super::*;
    use crate::index::{MemoryIndex, MemorySource};

    #[test]
    fn project_recall_buffer_take_is_one_shot() {
        let buf = ProjectRecallBuffer::default();
        buf.store("s1", vec!["line".to_string()]);
        assert_eq!(buf.take("s1").unwrap().len(), 1);
        assert!(buf.take("s1").is_none());
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
        ensure_dir(tmp.path()).unwrap();
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
