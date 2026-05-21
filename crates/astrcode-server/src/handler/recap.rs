//! Recap 生成逻辑 — `/recap` 命令的服务端实现。

use astrcode_core::{
    event::EventPayload,
    extension::ExtensionEvent,
    llm::{LlmEvent, LlmMessage},
};
use tokio::sync::mpsc;

use super::{CommandHandler, HandlerError};

const RECAP_PROMPT: &str = "The user stepped away and is coming back. Recap concisely some plain \
                            sentences, no markdown. Be brief but include all essential info. Lead \
                            with the overall goal and current task, then the next action. Skip \
                            root-cause narrative, fix internals, secondary to-dos, and em-dash \
                            tangents.";

impl CommandHandler {
    /// 生成当前 session 对话摘要。
    ///
    /// 复用 session 的 system prompt + 历史前缀命中 prompt cache，
    /// 追加 recap prompt 作为末尾 user message，单次 LLM 调用，不创建 turn。
    pub(super) async fn recap_session(&mut self) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;

        if self.active_turns.contains_key(&sid) {
            self.send_error(40900, "Cannot recap during active turn");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        let session = self
            .runtime
            .session_manager
            .open(sid.clone())
            .await
            .map_err(|e| HandlerError::SessionNotFound(format!("{e}")))?;

        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session: {e}")))?;

        if state.messages.is_empty() {
            self.send_error(40400, "Nothing to recap yet");
            return Ok(());
        }

        // 构造 LLM 请求：system + 历史 + recap prompt
        let mut messages = Vec::new();
        if let Some(ref sp) = state.system_prompt {
            messages.push(LlmMessage::system(sp));
        }
        messages.extend(state.provider_messages());
        messages.push(LlmMessage::user(RECAP_PROMPT));

        // 单次调用，无 tools
        let llm = self.runtime.capabilities.llm();
        let rx = llm
            .generate(messages, vec![])
            .await
            .map_err(|e| HandlerError::Other(format!("recap LLM call: {e}")))?;

        let text = collect_llm_text(rx)
            .await
            .map_err(|e| HandlerError::Other(format!("recap LLM stream: {e}")))?;

        // 持久化
        session
            .emit_durable(
                None,
                EventPayload::RecapGenerated {
                    text: text.clone(),
                    source: "manual".into(),
                },
            )
            .await
            .map_err(|e| HandlerError::Other(format!("persist recap: {e}")))?;

        // PostRecap hook (non-blocking)
        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: state.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(state.model_id.clone()),
            plugin_event_sink: None,
        };
        if let Err(e) = self
            .runtime
            .extension_runner
            .emit_lifecycle(ExtensionEvent::PostRecap, lifecycle_ctx)
            .await
        {
            tracing::warn!(error = %e, "PostRecap hook failed");
        }

        Ok(())
    }
}

/// 从 LLM 事件流中收集所有 text delta 拼成完整文本。
async fn collect_llm_text(mut rx: mpsc::UnboundedReceiver<LlmEvent>) -> Result<String, String> {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => return Err(message),
            _ => {},
        }
    }
    Ok(text)
}
