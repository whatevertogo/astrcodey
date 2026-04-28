//! 扩展上下文 — 为扩展提供受限的会话/服务视图。
//!
//! 本模块实现了 [`ExtensionContext`] trait 的具体类型，为扩展提供受控的、
//! 以读为主的会话和服务访问能力。扩展不能直接修改会话状态，
//! 必须通过钩子和事件发射机制进行交互。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionContext, PostToolUseInput, PreToolUseInput},
    llm::LlmMessage,
    tool::ToolDefinition,
};
use tokio::sync::mpsc;

/// [`ExtensionContext`] 的具体实现。
///
/// 为扩展提供受控的、以读为主的会话和服务视图。
/// 扩展不能直接修改会话状态，必须通过钩子和事件发射机制进行交互。
pub struct ServerExtensionContext {
    /// 当前会话 ID
    session_id: String,
    /// 当前工作目录
    working_dir: String,
    /// 当前模型选择配置
    model_selection: ModelSelection,
    /// 配置键值对
    config_values: HashMap<String, String>,
    /// 可用的工具定义
    tool_defs: HashMap<String, ToolDefinition>,
    /// 自定义事件发送通道
    custom_event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    /// PreToolUse 钩子的输入数据
    pre_tool_use_input: Option<PreToolUseInput>,
    /// PostToolUse 钩子的输入数据
    post_tool_use_input: Option<PostToolUseInput>,
    /// 扩展注册的待处理工具列表
    pending_tools: Mutex<Vec<ToolDefinition>>,
    /// LLM 提供者消息列表
    provider_messages: Option<Vec<LlmMessage>>,
}

impl ServerExtensionContext {
    /// 创建新的扩展上下文。
    ///
    /// # 参数
    /// - `session_id`: 会话唯一标识
    /// - `working_dir`: 当前工作目录路径
    /// - `model_selection`: 当前模型选择配置
    pub fn new(session_id: String, working_dir: String, model_selection: ModelSelection) -> Self {
        Self {
            session_id,
            working_dir,
            model_selection,
            config_values: HashMap::new(),
            tool_defs: HashMap::new(),
            custom_event_tx: None,
            pre_tool_use_input: None,
            post_tool_use_input: None,
            pending_tools: Mutex::new(Vec::new()),
            provider_messages: None,
        }
    }

    /// 设置扩展可访问的配置值。
    pub fn set_config(&mut self, values: HashMap<String, String>) {
        self.config_values = values;
    }

    /// 设置扩展可见的工具定义。
    pub fn set_tools(&mut self, tools: HashMap<String, ToolDefinition>) {
        self.tool_defs = tools;
    }

    /// 设置用于向会话日志发送自定义事件的通道。
    pub fn set_custom_event_sender(&mut self, tx: mpsc::UnboundedSender<EventPayload>) {
        self.custom_event_tx = Some(tx);
    }

    /// 附加当前工具调用的输入数据，供 PreToolUse 钩子使用。
    pub fn set_pre_tool_use_input(&mut self, input: PreToolUseInput) {
        self.pre_tool_use_input = Some(input);
        self.post_tool_use_input = None;
    }

    /// 附加当前工具调用的结果数据，供 PostToolUse 钩子使用。
    pub fn set_post_tool_use_input(&mut self, input: PostToolUseInput) {
        self.pre_tool_use_input = None;
        self.post_tool_use_input = Some(input);
    }

    /// 附加 LLM 提供者消息，供 BeforeProviderRequest 钩子使用。
    pub fn set_provider_messages(&mut self, messages: Vec<LlmMessage>) {
        self.provider_messages = Some(messages);
    }

    /// 取出所有待处理的工具注册（消费式取出）。
    pub fn take_pending_tools(&mut self) -> Vec<ToolDefinition> {
        std::mem::take(&mut *self.pending_tools.lock().unwrap())
    }

    /// 构建当前上下文的轻量级快照，可跨线程共享。
    pub fn snapshot(&self) -> Arc<ServerExtensionContextSnapshot> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
            provider_messages: self.provider_messages.clone(),
        })
    }
}

/// 轻量级上下文快照，用于 NonBlocking 即发即弃钩子。
///
/// 不包含可变状态（如工具注册、事件通道），仅保留只读的会话信息。
pub struct ServerExtensionContextSnapshot {
    session_id: String,
    working_dir: String,
    model_selection: ModelSelection,
    pre_tool_use_input: Option<PreToolUseInput>,
    post_tool_use_input: Option<PostToolUseInput>,
    provider_messages: Option<Vec<LlmMessage>>,
}

#[async_trait::async_trait]
impl ExtensionContext for ServerExtensionContextSnapshot {
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn working_dir(&self) -> &str {
        &self.working_dir
    }
    fn model_selection(&self) -> ModelSelection {
        self.model_selection.clone()
    }
    /// 快照不支持配置查询，始终返回 None
    fn config_value(&self, _key: &str) -> Option<String> {
        None
    }
    /// 快照不支持自定义事件发射
    async fn emit_custom_event(&self, _name: &str, _data: serde_json::Value) {}
    /// 快照不支持工具查找
    fn find_tool(&self, _name: &str) -> Option<ToolDefinition> {
        None
    }
    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.pre_tool_use_input.clone()
    }
    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.post_tool_use_input.clone()
    }
    fn provider_messages(&self) -> Option<Vec<LlmMessage>> {
        self.provider_messages.clone()
    }
    fn log_warn(&self, msg: &str) {
        tracing::warn!("[extension] {msg}");
    }
    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
            provider_messages: self.provider_messages.clone(),
        })
    }
}

#[async_trait::async_trait]
impl ExtensionContext for ServerExtensionContext {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn working_dir(&self) -> &str {
        &self.working_dir
    }

    fn model_selection(&self) -> ModelSelection {
        self.model_selection.clone()
    }

    /// 根据键名查询配置值
    fn config_value(&self, key: &str) -> Option<String> {
        self.config_values.get(key).cloned()
    }

    /// 通过通道发送自定义事件到会话日志
    async fn emit_custom_event(&self, name: &str, data: serde_json::Value) {
        if let Some(tx) = &self.custom_event_tx {
            let _ = tx.send(EventPayload::Custom {
                name: name.into(),
                data,
            });
        }
    }

    /// 按名称查找工具定义
    fn find_tool(&self, name: &str) -> Option<ToolDefinition> {
        self.tool_defs.get(name).cloned()
    }

    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.pre_tool_use_input.clone()
    }

    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.post_tool_use_input.clone()
    }

    /// 注册一个工具定义到待处理列表
    fn register_tool(&self, def: ToolDefinition) {
        self.pending_tools.lock().unwrap().push(def);
    }

    /// 取出所有已注册的工具定义（消费式取出）
    fn drain_registered_tools(&self) -> Vec<ToolDefinition> {
        std::mem::take(&mut *self.pending_tools.lock().unwrap())
    }

    fn provider_messages(&self) -> Option<Vec<LlmMessage>> {
        self.provider_messages.clone()
    }

    fn log_warn(&self, msg: &str) {
        tracing::warn!("[extension] {msg}");
    }

    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
            provider_messages: self.provider_messages.clone(),
        })
    }
}
