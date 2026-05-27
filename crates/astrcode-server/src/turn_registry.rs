//! TurnRegistry — 统一的活跃 turn 进程控制索引。
//!
//! 合并了之前的 `CommandHandler.active_turns` 和 `SessionManager.ActiveExecutionIndex`。
//! 只存进程控制句柄（turn_id + abort_handle + session 引用），不存业务状态。
//!
//! 注意：`has_active()` 是进程控制层的优化索引，权威状态来自事件日志的 `phase` 字段。
//! 进程重启后 registry 为空，需通过 `TurnScheduler::repair_stale()` 从事件重建一致性。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::Session;
use parking_lot::Mutex;
use tokio::task::AbortHandle;

struct TurnEntry {
    turn_id: TurnId,
    abort_handle: AbortHandle,
    session: Arc<Session>,
}

pub struct TurnRegistry {
    entries: Mutex<HashMap<SessionId, TurnEntry>>,
}

impl TurnRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 注册活跃 turn。若 session_id 已有活跃 turn 则返回 false。
    pub fn register(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        abort_handle: AbortHandle,
        session: Arc<Session>,
    ) -> bool {
        let mut entries = self.entries.lock();
        if entries.contains_key(&session_id) {
            return false;
        }
        entries.insert(
            session_id,
            TurnEntry {
                turn_id,
                abort_handle,
                session,
            },
        );
        true
    }

    /// 仅在 turn_id 匹配时移除，返回被移除的 session。
    pub fn remove_if_matches(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Option<Arc<Session>> {
        let mut entries = self.entries.lock();
        if entries
            .get(session_id)
            .is_some_and(|entry| &entry.turn_id == turn_id)
        {
            entries.remove(session_id).map(|e| e.session)
        } else {
            None
        }
    }

    /// Abort 并移除活跃 turn，返回 turn_id 和 session 用于写终态事件。
    pub fn abort_and_remove(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        let entry = self.entries.lock().remove(session_id)?;
        entry.abort_handle.abort();
        Some((entry.turn_id, entry.session))
    }

    /// 仅移除（不 abort）。用于已完成的 turn 清理。
    pub fn remove(&self, session_id: &SessionId) {
        self.entries.lock().remove(session_id);
    }

    pub fn has_active(&self, session_id: &SessionId) -> bool {
        self.entries.lock().contains_key(session_id)
    }

    /// 获取指定 session 的活跃 turn_id。
    pub fn active_turn_id(&self, session_id: &SessionId) -> Option<TurnId> {
        self.entries
            .lock()
            .get(session_id)
            .map(|e| e.turn_id.clone())
    }

    /// 获取指定 session 的活跃 session Arc。
    pub fn get_session(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        self.entries
            .lock()
            .get(session_id)
            .map(|e| Arc::clone(&e.session))
    }
}

impl Default for TurnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        storage::EventStore,
        tool::ToolDefinition,
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;

    struct NeverLlm;

    #[async_trait::async_trait]
    impl LlmProvider for NeverLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            std::future::pending().await
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_caps() -> Arc<astrcode_session::SessionRuntimeServices> {
        let llm: Arc<dyn LlmProvider> = Arc::new(NeverLlm);
        let extension_runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
        let context_assembler = Arc::new(
            astrcode_context::context_assembler::LlmContextAssembler::new(Default::default()),
        );
        Arc::new(astrcode_session::SessionRuntimeServices::new(
            Arc::clone(&llm),
            llm,
            extension_runner,
            context_assembler,
            EffectiveConfig {
                llm: LlmSettings {
                    provider_kind: "mock".into(),
                    base_url: String::new(),
                    api_key: String::new(),
                    api_mode: OpenAiApiMode::ChatCompletions,
                    model_id: "mock".into(),
                    max_tokens: 1024,
                    context_limit: 1024,
                    connect_timeout_secs: 1,
                    read_timeout_secs: 1,
                    max_retries: 0,
                    retry_base_delay_ms: 0,
                    supports_prompt_cache_key: false,
                    prompt_cache_retention: None,
                    reasoning: false,
                    thinking_level: None,
                },
                small_llm: LlmSettings {
                    provider_kind: "mock".into(),
                    base_url: String::new(),
                    api_key: String::new(),
                    api_mode: OpenAiApiMode::ChatCompletions,
                    model_id: "mock".into(),
                    max_tokens: 1024,
                    context_limit: 1024,
                    connect_timeout_secs: 1,
                    read_timeout_secs: 1,
                    max_retries: 0,
                    retry_base_delay_ms: 0,
                    supports_prompt_cache_key: false,
                    prompt_cache_retention: None,
                    reasoning: false,
                    thinking_level: None,
                },
                context: Default::default(),
                agent: Default::default(),
                extensions: ExtensionSettings::default(),
            },
        ))
    }

    async fn make_session(sid: &str) -> Arc<Session> {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let runtime = Arc::new(astrcode_session::SessionRuntimeState::new(
            Arc::new(NeverLlm),
            Arc::new(NeverLlm),
            "mock".into(),
        ));
        Arc::new(
            Session::create_with_id(
                store,
                SessionId::from(sid),
                ".",
                "mock",
                None,
                None,
                None,
                runtime,
                test_caps(),
            )
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn register_prevents_duplicate() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;
        let handle =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await })
                .abort_handle();

        assert!(registry.register(sid.clone(), turn_id, handle, session));
        let handle2 =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await })
                .abort_handle();
        let session2 = make_session("session-1b").await;
        assert!(!registry.register(sid.clone(), TurnId::from("turn-2"), handle2, session2));
    }

    #[tokio::test]
    async fn remove_if_matches_only_removes_matching_turn() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;
        let handle =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await })
                .abort_handle();

        registry.register(sid.clone(), turn_id.clone(), handle, session);
        assert!(registry.has_active(&sid));

        assert!(
            registry
                .remove_if_matches(&sid, &TurnId::from("other"))
                .is_none()
        );
        assert!(registry.has_active(&sid));

        assert!(registry.remove_if_matches(&sid, &turn_id).is_some());
        assert!(!registry.has_active(&sid));
    }

    #[tokio::test]
    async fn abort_and_remove_returns_turn_id() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;
        let handle =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await })
                .abort_handle();

        registry.register(sid.clone(), turn_id.clone(), handle, session);
        let (removed_turn_id, _) = registry.abort_and_remove(&sid).unwrap();
        assert_eq!(removed_turn_id, turn_id);
        assert!(!registry.has_active(&sid));
    }
}
