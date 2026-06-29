//! TurnRegistry — 统一的活跃 turn 进程控制索引。
//!
//! 合并了之前的 `CommandHandler.active_turns` 和 `SessionManager.ActiveExecutionIndex`。
//! 只存进程控制句柄（turn_id + shutdown handle + session 引用），不存业务状态。
//!
//! 注意：`has_active()` 是进程控制层的优化索引，权威状态来自事件日志的 `phase` 字段。
//! 进程重启后 registry 为空，需通过 `TurnScheduler::repair_stale()` 从事件重建一致性。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::{Session, turn_handle::TurnShutdownHandle};
use parking_lot::Mutex;

struct TurnEntry {
    turn_id: TurnId,
    shutdown_handle: TurnShutdownHandle,
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
        shutdown_handle: TurnShutdownHandle,
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
                shutdown_handle,
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

    /// 仅当活跃 turn 的底层 task 已结束时移除，返回其 turn_id。
    pub fn remove_if_finished(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        let mut entries = self.entries.lock();
        if !entries
            .get(session_id)
            .is_some_and(|entry| entry.shutdown_handle.is_finished())
        {
            return None;
        }
        entries.remove(session_id).map(|e| (e.turn_id, e.session))
    }

    /// 请求活跃 turn 协作式 shutdown，不移除 registry。
    pub fn request_shutdown(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        let entries = self.entries.lock();
        let entry = entries.get(session_id)?;
        entry.shutdown_handle.request_shutdown();
        Some((entry.turn_id.clone(), Arc::clone(&entry.session)))
    }

    /// 强制 kill 并移除活跃 turn，返回 turn_id 和 session 用于兜底写终态事件。
    pub fn force_kill_and_remove(
        &self,
        session_id: &SessionId,
        expected_turn_id: &TurnId,
    ) -> Option<(TurnId, Arc<Session>)> {
        let mut entries = self.entries.lock();
        if !entries
            .get(session_id)
            .is_some_and(|entry| &entry.turn_id == expected_turn_id)
        {
            return None;
        }
        let entry = entries.remove(session_id)?;
        entry.shutdown_handle.force_kill();
        Some((entry.turn_id, entry.session))
    }

    pub fn force_kill_and_remove_if_running(
        &self,
        session_id: &SessionId,
        expected_turn_id: &TurnId,
    ) -> Option<(TurnId, Arc<Session>)> {
        let mut entries = self.entries.lock();
        let entry = entries.get(session_id)?;
        if &entry.turn_id != expected_turn_id || entry.shutdown_handle.is_finished() {
            return None;
        }
        let entry = entries.remove(session_id)?;
        entry.shutdown_handle.force_kill();
        Some((entry.turn_id, entry.session))
    }

    pub fn active_is_finished(&self, session_id: &SessionId) -> bool {
        self.entries
            .lock()
            .get(session_id)
            .is_some_and(|entry| entry.shutdown_handle.is_finished())
    }

    /// 测试和强制清理用：强制 kill 当前活跃 turn，不校验 turn_id。
    pub fn force_kill_current(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        let turn_id = self.active_turn_id(session_id)?;
        self.force_kill_and_remove(session_id, &turn_id)
    }

    /// 仅移除（不 kill）。用于已完成的 turn 清理。
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
    use tokio_util::sync::CancellationToken;

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
        let effective = EffectiveConfig {
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
                supports_stream_usage: false,
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
                supports_stream_usage: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
            },
            context: Default::default(),
            agent: Default::default(),
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        };
        Arc::new(astrcode_session::SessionRuntimeServices::new(
            Arc::clone(&llm),
            llm,
            effective,
            crate::default_host::first_party_host_services(
                extension_runner,
                context_assembler,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ),
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

    fn test_shutdown_handle() -> TurnShutdownHandle {
        let handle =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await })
                .abort_handle();
        TurnShutdownHandle::new(CancellationToken::new(), handle)
    }

    #[tokio::test]
    async fn register_prevents_duplicate() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;

        assert!(registry.register(sid.clone(), turn_id, test_shutdown_handle(), session));
        let session2 = make_session("session-1b").await;
        assert!(!registry.register(
            sid.clone(),
            TurnId::from("turn-2"),
            test_shutdown_handle(),
            session2
        ));
    }

    #[tokio::test]
    async fn remove_if_matches_only_removes_matching_turn() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;

        registry.register(
            sid.clone(),
            turn_id.clone(),
            test_shutdown_handle(),
            session,
        );
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
    async fn remove_if_finished_only_removes_completed_turn() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;
        let finished = tokio::spawn(async {}).abort_handle();

        registry.register(
            sid.clone(),
            turn_id.clone(),
            TurnShutdownHandle::new(CancellationToken::new(), finished),
            session,
        );

        for _ in 0..50 {
            if registry.active_is_finished(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let (removed_turn_id, _) = registry.remove_if_finished(&sid).unwrap();
        assert_eq!(removed_turn_id, turn_id);
        assert!(!registry.has_active(&sid));
    }

    #[tokio::test]
    async fn force_kill_current_returns_turn_id() {
        let registry = TurnRegistry::new();
        let sid = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let session = make_session("session-1").await;

        registry.register(
            sid.clone(),
            turn_id.clone(),
            test_shutdown_handle(),
            session,
        );
        let (removed_turn_id, _) = registry.force_kill_current(&sid).unwrap();
        assert_eq!(removed_turn_id, turn_id);
        assert!(!registry.has_active(&sid));
    }
}
