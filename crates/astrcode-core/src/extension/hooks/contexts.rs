//! Context structs passed to hook handlers by the runtime.

use std::{fmt, path::PathBuf, sync::Arc};

use super::types::{AfterToolResult, CompactTrigger, ExchangeSummary};
use crate::{
    config::ModelSelection,
    extension::ExtensionEventSink,
    message_attachment::MessageAttachment,
    tool::{ToolDefinition, ToolResult},
};

/// LLM 自然结束后的扩展决策钩子上下文。
#[derive(Debug, Clone)]
pub struct ContinueAfterStopContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub assistant_text: String,
    pub finish_reason: String,
    pub continuations_this_turn: u32,
}

/// 用户消息写入 transcript 前的扩展变换上下文。
#[derive(Debug, Clone)]
pub struct UserMessageEnvelopeContext {
    pub session_id: String,
    pub turn_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub text: String,
    pub attachments: Vec<MessageAttachment>,
    pub session_store_dir: Option<PathBuf>,
}

/// 工具结果批次落盘后的继续/结束决策上下文。
#[derive(Debug, Clone)]
pub struct AfterToolResultsContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_results: Vec<AfterToolResult>,
    pub session_store_dir: Option<PathBuf>,
}

/// PostToolUseFailure 钩子上下文。
#[derive(Debug, Clone)]
pub struct PostToolUseFailureContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub error: String,
    pub tool_result: ToolResult,
}

/// PreToolUse 钩子上下文。
#[derive(Clone)]
pub struct PreToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub approval_mode: crate::permission::ApprovalMode,
    pub available_tools: Vec<ToolDefinition>,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    pub session_store_dir: Option<PathBuf>,
}

impl fmt::Debug for PreToolUseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreToolUseContext")
            .field("session_id", &self.session_id)
            .field("tool_name", &self.tool_name)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .finish_non_exhaustive()
    }
}

/// PostToolUse 钩子上下文。
#[derive(Clone)]
pub struct PostToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_result: ToolResult,
    pub is_error: bool,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    pub session_store_dir: Option<PathBuf>,
}

impl fmt::Debug for PostToolUseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostToolUseContext")
            .field("session_id", &self.session_id)
            .field("tool_name", &self.tool_name)
            .field("is_error", &self.is_error)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .finish_non_exhaustive()
    }
}

/// Provider 钩子上下文。
#[derive(Debug, Clone)]
pub struct ProviderContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub messages: Vec<crate::llm::LlmMessage>,
    pub session_store_dir: Option<PathBuf>,
}

/// PromptBuild 钩子上下文。
#[derive(Debug, Clone)]
pub struct PromptBuildContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tools: Vec<ToolDefinition>,
}

/// Compact 钩子上下文。
#[derive(Debug, Clone)]
pub struct CompactContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub trigger: CompactTrigger,
    pub message_count: usize,
    pub pre_tokens: Option<usize>,
    pub post_tokens: Option<usize>,
    pub summary: Option<String>,
}

/// 通用生命周期钩子上下文。
#[derive(Clone)]
pub struct LifecycleContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    pub last_exchange: Option<ExchangeSummary>,
    pub mid_turn_user_messages_synced: u32,
}

impl LifecycleContext {
    pub fn for_step_start(mut self, mid_turn_user_messages_synced: u32) -> Self {
        self.mid_turn_user_messages_synced = mid_turn_user_messages_synced;
        self
    }
}

impl fmt::Debug for LifecycleContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LifecycleContext")
            .field("session_id", &self.session_id)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .field("last_exchange", &self.last_exchange)
            .field(
                "mid_turn_user_messages_synced",
                &self.mid_turn_user_messages_synced,
            )
            .finish_non_exhaustive()
    }
}

/// 命令执行上下文。
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub session_store_dir: Option<PathBuf>,
}
