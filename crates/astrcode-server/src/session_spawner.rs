//! 子会话派生器 — 当扩展返回 `RunSession` 时的处理逻辑。
//!
//! [`ServerSessionSpawner`] 创建子会话、用子 Agent 执行一轮对话，
//! 将事件持久化到子会话存储，并经由 [`ProgressTx`] 将关键进展
//! 转译为 [`ToolOutputDelta`] 实时反馈给父会话的 TUI。

use std::{collections::HashSet, sync::Arc};

use astrcode_context::manager::LlmContextAssembler;
use astrcode_core::{
    event::{Event, EventPayload, ToolOutputStream},
    types::{new_message_id, new_turn_id},
};
use astrcode_extensions::{
    runner::ExtensionRunner,
    runtime::{SpawnRequest, SpawnResult},
};
use tokio::sync::mpsc;

use crate::{
    agent_loop::{Agent, AgentServices, drive_agent, tool_name_matches_allowlist},
    bootstrap::{build_system_prompt_snapshot, build_tool_registry_snapshot},
    session::SessionManager,
};

/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct ServerSessionSpawner {
    pub(crate) session_manager: Arc<SessionManager>,
    pub(crate) llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) context_assembler: Arc<LlmContextAssembler>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    pub(crate) read_timeout_secs: u64,
}

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let progress = ProgressTx::new(request.tool_call_id, request.event_tx);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();
        let model_id = match request.model_preference.clone() {
            Some(model) => model,
            None => {
                let parent_session = self
                    .session_manager
                    .get(&parent_session_id.to_string())
                    .await
                    .ok_or_else(|| format!("parent session {parent_session_id} not found"))?;
                let parent_model_id = parent_session.state.read().await.model_id.clone();
                drop(parent_session);
                parent_model_id
            },
        };

        let create_event = self
            .session_manager
            .create(
                &request.working_dir,
                &model_id,
                2048,
                Some(parent_session_id),
            )
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let child_sid = create_event.session_id.to_string();
        let child_turn_id = new_turn_id();

        let tool_registry = build_tool_registry_snapshot(
            &self.extension_runner,
            &request.working_dir,
            self.read_timeout_secs,
        )
        .await;

        let allowed_tools: HashSet<String> = request.allowed_tools.iter().cloned().collect();
        let mut prompt_tools = tool_registry.list_definitions();
        if !allowed_tools.is_empty() {
            prompt_tools.retain(|tool| tool_name_matches_allowlist(&allowed_tools, &tool.name));
        }
        let (system_prompt, fingerprint) = build_system_prompt_snapshot(
            &self.extension_runner,
            &child_sid,
            &request.working_dir,
            &model_id,
            &prompt_tools,
            Some(&request.system_prompt),
        )
        .await
        .map_err(|e| format!("build child system prompt: {e}"))?;

        append_child_session_payload(
            self.session_manager.as_ref(),
            &child_sid,
            EventPayload::SystemPromptConfigured {
                text: system_prompt.clone(),
                fingerprint,
            },
        )
        .await?;

        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::TurnStarted,
        )
        .await?;
        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: user_prompt.clone(),
            },
        )
        .await?;

        progress.emit(
            ToolOutputStream::Stdout,
            format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
        );

        let agent = Agent::new(
            child_sid.clone(),
            request.working_dir.clone(),
            system_prompt,
            model_id,
            AgentServices {
                llm: Arc::clone(&self.llm),
                tool_registry,
                extension_runner: Arc::clone(&self.extension_runner),
                context_assembler: Arc::clone(&self.context_assembler),
                session_manager: Arc::clone(&self.session_manager),
            },
        )
        .with_tool_allowlist(request.allowed_tools);

        let cs = child_sid.clone();
        let cti = child_turn_id.clone();
        let sm = Arc::clone(&self.session_manager);
        let pf = progress.clone();
        let (output, emitted_error) =
            drive_agent(&agent, &user_prompt, Vec::new(), move |payload| {
                let sm = sm.clone();
                let cs = cs.clone();
                let cti = cti.clone();
                let p = pf.clone();
                async move {
                    let _ = append_child_payload(&sm, &cs, &cti, payload.clone()).await;
                    p.forward(&payload);
                }
            })
            .await;

        match output {
            Ok(output) => {
                append_child_payload(
                    self.session_manager.as_ref(),
                    &child_sid,
                    &child_turn_id,
                    EventPayload::TurnCompleted {
                        finish_reason: output.finish_reason.clone(),
                    },
                )
                .await?;
                progress.emit(
                    ToolOutputStream::Stdout,
                    format!("child turn completed: {}\n", output.finish_reason),
                );
                Ok(SpawnResult {
                    content: output.text,
                    child_session_id: child_sid,
                })
            },
            Err(e) => Ok(SpawnResult {
                content: {
                    if !emitted_error {
                        append_child_payload(
                            self.session_manager.as_ref(),
                            &child_sid,
                            &child_turn_id,
                            EventPayload::ErrorOccurred {
                                code: -32603,
                                message: e.to_string(),
                                recoverable: false,
                            },
                        )
                        .await?;
                    }
                    append_child_payload(
                        self.session_manager.as_ref(),
                        &child_sid,
                        &child_turn_id,
                        EventPayload::TurnCompleted {
                            finish_reason: "error".into(),
                        },
                    )
                    .await?;
                    progress.emit(
                        ToolOutputStream::Stderr,
                        format!("child agent error: {e}\n"),
                    );
                    format!("child agent error: {e}")
                },
                child_session_id: child_sid,
            }),
        }
    }
}

/// 将子 agent 事件转发为父级工具调用的 [`ToolOutputDelta`] 进度事件。
///
/// 持有父级工具调用 ID 和事件发送通道。`emit` 发送字符串消息，
/// `forward` 将子 agent 事件自动转译为对应的进度描述。
#[derive(Clone)]
struct ProgressTx {
    call_id: Option<String>,
    tx: Option<mpsc::UnboundedSender<EventPayload>>,
}

impl ProgressTx {
    fn new(call_id: Option<String>, tx: Option<mpsc::UnboundedSender<EventPayload>>) -> Self {
        Self { call_id, tx }
    }

    fn emit(&self, stream: ToolOutputStream, delta: impl Into<String>) {
        let Some(call_id) = &self.call_id else { return };
        let Some(tx) = &self.tx else { return };
        let delta = delta.into();
        if delta.is_empty() {
            return;
        }
        let _ = tx.send(EventPayload::ToolOutputDelta {
            call_id: call_id.clone(),
            stream,
            delta,
        });
    }

    fn forward(&self, payload: &EventPayload) {
        if let Some((stream, delta)) = child_progress_delta(payload) {
            self.emit(stream, delta);
        }
    }
}

async fn append_child_payload(
    session_manager: &SessionManager,
    child_sid: &str,
    child_turn_id: &str,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(
                child_sid.to_string(),
                Some(child_turn_id.to_string()),
                payload,
            ))
            .await
            .map_err(|e| format!("append child event: {e}"))?;
    }
    Ok(())
}

async fn append_child_session_payload(
    session_manager: &SessionManager,
    child_sid: &str,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(child_sid.to_string(), None, payload))
            .await
            .map_err(|e| format!("append child session event: {e}"))?;
    }
    Ok(())
}

fn child_progress_delta(payload: &EventPayload) -> Option<(ToolOutputStream, String)> {
    match payload {
        EventPayload::AssistantMessageStarted { .. } => {
            Some((ToolOutputStream::Stdout, "child assistant started\n".into()))
        },
        EventPayload::AssistantTextDelta { delta, .. } => {
            if delta.is_empty() {
                None
            } else {
                Some((ToolOutputStream::Stdout, delta.clone()))
            }
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            let summary = one_line_summary(text);
            if summary.is_empty() {
                None
            } else {
                Some((
                    ToolOutputStream::Stdout,
                    format!("child assistant completed: {summary}\n"),
                ))
            }
        },
        EventPayload::ToolCallStarted { tool_name, .. } => Some((
            ToolOutputStream::Stdout,
            format!("child tool started: {tool_name}\n"),
        )),
        EventPayload::ToolOutputDelta { stream, delta, .. } => {
            Some((*stream, format!("child tool output: {delta}")))
        },
        EventPayload::ToolCallCompleted {
            tool_name, result, ..
        } => {
            let stream = if result.is_error {
                ToolOutputStream::Stderr
            } else {
                ToolOutputStream::Stdout
            };
            let detail = one_line_summary(result.error.as_deref().unwrap_or(&result.content));
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            Some((
                stream,
                format!("child tool completed: {tool_name}{suffix}\n"),
            ))
        },
        EventPayload::ErrorOccurred { message, .. } => Some((
            ToolOutputStream::Stderr,
            format!("child error: {message}\n"),
        )),
        EventPayload::TurnCompleted { finish_reason } => Some((
            ToolOutputStream::Stdout,
            format!("child turn completed: {finish_reason}\n"),
        )),
        _ => None,
    }
}

fn one_line_summary(text: &str) -> String {
    let mut summary = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 160;
    if summary.chars().count() > MAX_CHARS {
        let truncated: String = summary.chars().take(MAX_CHARS - 1).collect();
        summary = truncated;
        summary.push('…');
    }
    summary
}
