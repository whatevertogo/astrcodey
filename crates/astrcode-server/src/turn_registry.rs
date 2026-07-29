//! TurnRegistry — 统一的活跃 turn 进程控制索引。
//!
//! 合并了之前的 `CommandHandler.active_turns` 和 `SessionManager.ActiveExecutionIndex`。
//! 只存进程控制句柄（turn_id + shutdown handle + session 引用），不存业务状态。
//!
//! 注意：`has_active()` 是进程控制层的优化索引，权威状态来自事件日志的 `phase` 字段。
//! 进程重启后 registry 为空，需通过 `TurnScheduler::repair_stale()` 从事件重建一致性。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::{Session, TurnShutdownHandle};
use parking_lot::Mutex;

struct TurnEntry {
    turn_id: TurnId,
    state: TurnEntryState,
}

enum TurnEntryState {
    Reserved,
    Running {
        shutdown_handle: TurnShutdownHandle,
        session: Arc<Session>,
    },
}

pub(crate) struct TurnReservation {
    registry: Arc<TurnRegistry>,
    session_id: SessionId,
    turn_id: TurnId,
    armed: bool,
}

#[derive(Default)]
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
    #[cfg(test)]
    fn register(
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
                state: TurnEntryState::Running {
                    shutdown_handle,
                    session,
                },
            },
        );
        true
    }

    /// 在任何 session I/O 前预留 turn ownership。
    pub(crate) fn reserve(
        self: &Arc<Self>,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Option<TurnReservation> {
        let mut entries = self.entries.lock();
        if entries.contains_key(&session_id) {
            return None;
        }
        entries.insert(
            session_id.clone(),
            TurnEntry {
                turn_id: turn_id.clone(),
                state: TurnEntryState::Reserved,
            },
        );
        Some(TurnReservation {
            registry: Arc::clone(self),
            session_id,
            turn_id,
            armed: true,
        })
    }

    /// 仅在 turn_id 匹配时移除。
    pub fn remove_if_matches(&self, session_id: &SessionId, turn_id: &TurnId) -> Option<()> {
        let mut entries = self.entries.lock();
        if entries
            .get(session_id)
            .is_some_and(|entry| &entry.turn_id == turn_id)
        {
            entries.remove(session_id).map(|_| ())
        } else {
            None
        }
    }

    /// 仅当活跃 turn 的底层 task 已结束时移除，返回其 turn_id。
    pub fn remove_if_finished(&self, session_id: &SessionId) -> Option<TurnId> {
        let mut entries = self.entries.lock();
        if !entries.get(session_id).is_some_and(|entry| {
            matches!(
                &entry.state,
                TurnEntryState::Running {
                    shutdown_handle,
                    ..
                } if shutdown_handle.is_finished()
            )
        }) {
            return None;
        }
        let entry = entries.remove(session_id)?;
        drop(entries);
        Some(entry.turn_id)
    }

    /// 请求活跃 turn 协作式 shutdown，不移除 registry。
    pub fn request_shutdown(&self, session_id: &SessionId) -> Option<TurnId> {
        let entries = self.entries.lock();
        let entry = entries.get(session_id)?;
        let TurnEntryState::Running {
            shutdown_handle, ..
        } = &entry.state
        else {
            return None;
        };
        shutdown_handle.request_shutdown();
        Some(entry.turn_id.clone())
    }

    /// 强制 kill 并移除活跃 turn，返回 turn_id 和 session 用于兜底写终态事件。
    pub fn force_kill_and_remove(
        &self,
        session_id: &SessionId,
        expected_turn_id: &TurnId,
    ) -> Option<(TurnId, Arc<Session>)> {
        let mut entries = self.entries.lock();
        if !entries.get(session_id).is_some_and(|entry| {
            &entry.turn_id == expected_turn_id
                && matches!(entry.state, TurnEntryState::Running { .. })
        }) {
            return None;
        }
        let entry = entries.remove(session_id)?;
        let TurnEntryState::Running {
            shutdown_handle,
            session,
        } = entry.state
        else {
            return None;
        };
        shutdown_handle.force_kill();
        Some((entry.turn_id, session))
    }

    pub fn force_kill_and_remove_if_running(
        &self,
        session_id: &SessionId,
        expected_turn_id: &TurnId,
    ) -> Option<(TurnId, Arc<Session>)> {
        let mut entries = self.entries.lock();
        let entry = entries.get(session_id)?;
        let TurnEntryState::Running {
            shutdown_handle, ..
        } = &entry.state
        else {
            return None;
        };
        if &entry.turn_id != expected_turn_id || shutdown_handle.is_finished() {
            return None;
        }
        let entry = entries.remove(session_id)?;
        let TurnEntryState::Running {
            shutdown_handle,
            session,
        } = entry.state
        else {
            return None;
        };
        shutdown_handle.force_kill();
        Some((entry.turn_id, session))
    }

    pub fn active_is_finished(&self, session_id: &SessionId) -> bool {
        self.entries.lock().get(session_id).is_some_and(|entry| {
            matches!(
                &entry.state,
                TurnEntryState::Running {
                    shutdown_handle,
                    ..
                } if shutdown_handle.is_finished()
            )
        })
    }

    /// 测试和强制清理用：强制 kill 当前活跃 turn，不校验 turn_id。
    pub fn force_kill_current(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        let turn_id = self.active_turn_id(session_id)?;
        self.force_kill_and_remove(session_id, &turn_id)
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

    pub(crate) fn active_session_ids(&self) -> Vec<SessionId> {
        self.entries.lock().keys().cloned().collect()
    }

    /// 获取指定 session 的活跃 turn 与 session。
    pub fn active_execution(&self, session_id: &SessionId) -> Option<(TurnId, Arc<Session>)> {
        self.entries
            .lock()
            .get(session_id)
            .and_then(|entry| match &entry.state {
                TurnEntryState::Reserved => None,
                TurnEntryState::Running { session, .. } => {
                    Some((entry.turn_id.clone(), Arc::clone(session)))
                },
            })
    }
}

impl TurnReservation {
    pub(crate) fn activate(
        mut self,
        shutdown_handle: TurnShutdownHandle,
        session: Arc<Session>,
    ) -> bool {
        let mut entries = self.registry.entries.lock();
        let Some(entry) = entries
            .get_mut(&self.session_id)
            .filter(|entry| entry.turn_id == self.turn_id)
        else {
            return false;
        };
        if !matches!(entry.state, TurnEntryState::Reserved) {
            return false;
        }
        entry.state = TurnEntryState::Running {
            shutdown_handle,
            session,
        };
        self.armed = false;
        true
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        if self.armed {
            self.registry
                .remove_if_matches(&self.session_id, &self.turn_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{
            EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme, ProviderWireFormat,
        },
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use astrcode_session::SessionCreateParams;
    use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};
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

    fn test_runtime_services() -> Arc<astrcode_session::SessionRuntimeServices> {
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
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                model_id: "mock".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                supports_stream_usage: false,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
                thinking: Default::default(),
                thinking_capability: None,
                thinking_configured: false,
            },
            small_llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                model_id: "mock".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                supports_stream_usage: false,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
                thinking: Default::default(),
                thinking_capability: None,
                thinking_configured: false,
            },
            context: Default::default(),
            agent: Default::default(),
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        };
        crate::config_manager::assemble_session_runtime_services(
            Arc::clone(&llm),
            llm,
            effective,
            extension_runner,
            context_assembler,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        )
    }

    async fn make_session(sid: &str) -> Arc<Session> {
        let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
        let session_id = SessionId::from(sid);
        let runtime = Arc::new(astrcode_session::SessionRuntimeState::new(
            session_id, store,
        ));
        Arc::new(
            Session::create_with_params(SessionCreateParams {
                working_dir: ".".into(),
                model_id: "mock".into(),
                parent_session_id: None,
                tool_selection: None,
                source_extension: None,
                extra_system_prompt: None,
                initial_system_prompt: None,
                runtime,
                runtime_services: test_runtime_services(),
            })
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

    #[test]
    fn reservation_blocks_duplicate_start_and_rolls_back_on_drop() {
        let registry = Arc::new(TurnRegistry::new());
        let session_id = SessionId::from("session-reserved");
        let reservation = registry
            .reserve(session_id.clone(), TurnId::from("turn-1"))
            .unwrap();

        assert!(registry.has_active(&session_id));
        assert!(
            registry
                .reserve(session_id.clone(), TurnId::from("turn-2"))
                .is_none()
        );

        drop(reservation);
        assert!(!registry.has_active(&session_id));
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
        let removed_turn_id = registry.remove_if_finished(&sid).unwrap();
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
