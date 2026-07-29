//! Recap 生成逻辑 — `/recap` 命令的服务端实现。

use astrcode_context::ContextSnapshot;
use astrcode_core::{event::DurableEventPayload, llm};
use astrcode_extension_sdk::extension::ExtensionEvent;

use super::{CommandHandler, HandlerError};

const RECAP_PROMPT: &str = "The user stepped away and is coming back. Write exactly 1-3 short \
                            summary. Start by stating the high-level task — what they are \
                            building or debugging, not implementation details. Next: the concrete \
                            next step. 
                            Skip status reports and commit recaps.";

impl CommandHandler {
    /// 生成当前 session 对话摘要。
    ///
    /// 复用 session 的 system prompt + 历史前缀命中 prompt cache，
    /// 追加 recap prompt 作为末尾 user message，单次 LLM 调用，不创建 turn。
    pub(super) async fn recap_session(&mut self) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;

        if self.scheduler.registry().has_active(&sid) {
            self.send_error(40900, "Cannot recap during active turn");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        let session = self
            .runtime
            .session_manager
            .open(sid.clone())
            .await
            .map_err(|e| HandlerError::SessionNotFound(e.to_string()))?;

        let state = session.read_model().await.map_err(HandlerError::Session)?;

        if state.transcript.messages.is_empty() {
            self.send_error(40400, "Nothing to recap yet");
            return Ok(());
        }

        let snapshot = ContextSnapshot::new(
            state.stats.last_seq,
            state.system_prompt.text.clone(),
            state
                .transcript
                .messages
                .iter()
                .map(|message| message.message.clone())
                .collect(),
        );
        let mut transcript = snapshot.messages.clone();
        transcript.push(astrcode_core::llm::LlmMessage::user(RECAP_PROMPT));
        let messages = snapshot.request_messages(transcript);

        // 单次调用，无 tools
        let llm = self.runtime.runtime_services().llm();
        let rx = llm
            .generate(messages, vec![])
            .await
            .map_err(HandlerError::Llm)?;

        let text = llm::collect_stream_text(rx)
            .await
            .map_err(|e| HandlerError::InvalidRequest(e.to_string()))?;
        let text = strip_dsml_tags(&text);

        // 持久化
        session
            .emit_durable(
                None,
                DurableEventPayload::RecapGenerated {
                    text: text.clone(),
                    source: "manual".into(),
                },
            )
            .await
            .map_err(HandlerError::Session)?;

        // PostRecap hook (non-blocking)
        let lifecycle_ctx = astrcode_extension_sdk::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: state.identity.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(state.identity.model_id.clone()),
            event_tx: None,
            extension_event_sink: None,
            last_exchange: None,
            mid_turn_user_messages_synced: 0,
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

/// 剥离模型内部 tool call 格式标签（如 DeepSeek 的 `<｜｜DSML｜｜...>`）。
/// 模型在无 tools 的请求中偶尔会把内部格式当纯文本输出。
fn strip_dsml_tags(text: &str) -> String {
    const DSML_OPEN: &str = "<｜｜DSML｜｜";
    if !text.contains(DSML_OPEN) {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut remaining: &str = text;
    while let Some(start) = remaining.find(DSML_OPEN) {
        result.push_str(&remaining[..start]);
        // 跳过整个 DSML 块：从 <｜｜DSML｜｜...> 到匹配的 </｜｜DSML｜｜...>
        remaining = &remaining[start..];
        let end_tag = "</｜｜DSML｜｜";
        if let Some(end) = remaining.find(end_tag) {
            // 找到闭合标签的 '>' 之后继续
            let after_close = &remaining[end..];
            remaining = after_close
                .find('>')
                .map(|i| &after_close[i + 1..])
                .unwrap_or("");
        } else {
            // 没有闭合标签，跳到行尾
            remaining = remaining.find('\n').map(|i| &remaining[i..]).unwrap_or("");
        }
    }
    result.push_str(remaining);
    let cleaned = result.trim().to_string();
    if cleaned.is_empty() {
        text.to_string()
    } else {
        cleaned
    }
}
