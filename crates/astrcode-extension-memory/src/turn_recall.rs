//! Project memory: recall at turn end, deliver on the next turn's first LLM request.

use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::{
        ExchangeSummary, ExtensionError, HookResult, LifecycleContext, LifecycleHandler,
        ProviderContext, ProviderHandler, ProviderResult,
    },
    llm::LlmMessage,
};
use parking_lot::{Mutex, RwLock};

use crate::{config::MemoryConfig, prompts, store::MemoryStorePool};

/// User preferences loaded once per session for PromptBuild.
#[derive(Default)]
pub(crate) struct SessionPrefsCache {
    state: Mutex<Option<(String, Vec<String>)>>,
}

impl SessionPrefsCache {
    pub(crate) fn lines_for_session(
        &self,
        session_id: &str,
        load: impl FnOnce() -> std::io::Result<Vec<String>>,
    ) -> std::io::Result<Vec<String>> {
        let mut guard = self.state.lock();
        if let Some((cached_id, lines)) = guard.as_ref() {
            if cached_id == session_id {
                return Ok(lines.clone());
            }
        }
        let lines = load()?;
        *guard = Some((session_id.to_string(), lines.clone()));
        Ok(lines)
    }

    pub(crate) fn reset(&self) {
        *self.state.lock() = None;
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
    use astrcode_support::hostpaths::ensure_dir;
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
}
