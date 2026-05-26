//! Recap 生成逻辑 — `/recap` 命令的服务端实现。

use astrcode_core::{
    event::EventPayload,
    extension::ExtensionEvent,
    llm::{LlmEvent, LlmMessage},
};
use tokio::sync::mpsc;

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
        let llm = self.runtime.capabilities().llm();
        let rx = llm
            .generate(messages, vec![])
            .await
            .map_err(HandlerError::Llm)?;

        let text = collect_llm_text(rx)
            .await
            .map_err(HandlerError::InvalidRequest)?;

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
            .map_err(HandlerError::Session)?;

        // PostRecap hook (non-blocking)
        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: state.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(state.model_id.clone()),
            event_tx: None,
            extension_event_sink: None,
            last_exchange: None,
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
    Ok(strip_dsml_tags(&text))
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
