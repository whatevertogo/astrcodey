//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。
//!
//! 负责管理扩展注册、事件分发，并强制执行 HookMode 语义：
//! - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
//! - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
//! - Advisory: 结果仅记录日志，不强制执行

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    config::ModelSelection,
    extension::*,
    llm::LlmMessage,
    tool::{Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult},
};
use tokio::sync::RwLock;

use crate::runtime::{ExtensionRuntime, SessionSpawner, SpawnRequest};

/// 将生命周期事件分发到所有已注册的扩展。
///
/// 强制执行 HookMode 语义：
/// - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
/// - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
/// - Advisory: 结果仅记录日志，不强制执行
pub struct ExtensionRunner {
    /// 已注册的扩展列表（读写锁保护）
    extensions: RwLock<Vec<Arc<dyn Extension>>>,
    /// 共享的扩展运行时
    runtime: Arc<ExtensionRuntime>,
    /// 钩子执行超时时间
    timeout: Duration,
}

struct OrderedExtension {
    ext: Arc<dyn Extension>,
    mode: HookMode,
}

impl ExtensionRunner {
    /// 创建新的扩展运行器。
    ///
    /// # 参数
    /// - `timeout`: 阻塞钩子的执行超时时间
    /// - `runtime`: 共享的扩展运行时实例
    pub fn new(timeout: Duration, runtime: Arc<ExtensionRuntime>) -> Self {
        Self {
            extensions: RwLock::new(Vec::new()),
            runtime,
            timeout,
        }
    }

    /// 注册一个扩展。
    pub async fn register(&self, ext: Arc<dyn Extension>) {
        let mut exts = self.extensions.write().await;
        exts.push(ext);
    }

    /// 绑定会话创建能力到共享运行时。
    /// 在服务器启动后、任何工具执行之前调用一次。
    pub fn bind(&self, spawner: Arc<dyn SessionSpawner>) {
        self.runtime.bind(spawner);
    }

    /// 将事件分发到所有订阅的扩展。
    ///
    /// 在迭代前复制扩展列表，这样在钩子执行期间不会持有读锁。
    pub async fn dispatch(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<(), ExtensionError> {
        for ordered in self.ordered_extensions_for(&event).await {
            let ext = ordered.ext;

            match ordered.mode {
                HookMode::Blocking => {
                    // 带超时的同步执行
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    if let HookEffect::Block { reason } = result {
                        return Err(ExtensionError::Blocked { reason });
                    }
                    // Modified* 效果在非工具事件上无意义 — 记录警告并继续
                    if matches!(
                        result,
                        HookEffect::ModifiedInput { .. }
                            | HookEffect::ModifiedResult { .. }
                            | HookEffect::ModifiedMessages { .. }
                            | HookEffect::AppendMessages { .. }
                            | HookEffect::ModifiedOutput { .. }
                            | HookEffect::PromptContributions(_)
                            | HookEffect::CompactContributions(_)
                    ) {
                        tracing::warn!(
                            "extension returned {:?} on {:?} — effect ignored (only \
                             PreToolUse/PostToolUse/BeforeProviderRequest support modification)",
                            result,
                            event
                        );
                    }
                },
                HookMode::NonBlocking => {
                    let evt = event.clone();
                    // 使用快照以在派生前释放借用
                    let snap_ctx = ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    // 执行但不强制执行结果
                    let _ = ext.on_event(event.clone(), ctx).await;
                },
            }
        }

        Ok(())
    }

    /// 分发 PreToolUse 或 PostToolUse 事件，并收集第一个
    /// Blocking 结果（ModifiedInput / ModifiedResult / Block）。
    ///
    /// # 返回
    /// 返回 [`ToolHookOutcome`] 表示所有扩展处理后的综合结果。
    pub async fn dispatch_tool_hook(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<ToolHookOutcome, ExtensionError> {
        let mut modified_input: Option<serde_json::Value> = None;
        let mut modified_result: Option<String> = None;

        for ordered in self.ordered_extensions_for(&event).await {
            let ext = ordered.ext;

            match ordered.mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    match result {
                        HookEffect::Block { reason } => {
                            // 阻止效果立即返回
                            return Ok(ToolHookOutcome::Blocked { reason });
                        },
                        HookEffect::ModifiedInput { tool_input } => {
                            modified_input = Some(tool_input);
                        },
                        HookEffect::ModifiedResult { content } => {
                            modified_result = Some(content);
                        },
                        HookEffect::ModifiedMessages { .. }
                        | HookEffect::AppendMessages { .. }
                        | HookEffect::ModifiedOutput { .. }
                        | HookEffect::PromptContributions(_)
                        | HookEffect::CompactContributions(_)
                        | HookEffect::Allow => {},
                    }
                },
                HookMode::NonBlocking => {
                    let evt = event.clone();
                    let snap_ctx = ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), ctx).await;
                },
            }
        }

        // 优先级: ModifiedInput > ModifiedResult > Allow
        Ok(match (modified_input, modified_result) {
            (Some(input), _) => ToolHookOutcome::ModifiedInput { tool_input: input },
            (_, Some(content)) => ToolHookOutcome::ModifiedResult { content },
            _ => ToolHookOutcome::Allow,
        })
    }

    /// 分发提供者级别的钩子，收集消息变更。
    ///
    /// 用于 BeforeProviderRequest/AfterProviderResponse 事件，
    /// 允许扩展修改发送给 LLM 的消息列表。
    pub async fn dispatch_provider_hook(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<ProviderHookOutcome, ExtensionError> {
        let mut current_messages = ctx.provider_messages();
        let mut modified_messages = false;

        for ordered in self.ordered_extensions_for(&event).await {
            let ext = ordered.ext;
            let hook_ctx = ProviderMessagesContext {
                base: ctx,
                messages: current_messages.clone(),
            };

            match ordered.mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), &hook_ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    match result {
                        HookEffect::Block { reason } => {
                            return Ok(ProviderHookOutcome::Blocked { reason });
                        },
                        HookEffect::ModifiedMessages { messages } => {
                            current_messages = Some(messages);
                            modified_messages = true;
                        },
                        HookEffect::AppendMessages { mut messages } => {
                            current_messages
                                .get_or_insert_with(Vec::new)
                                .append(&mut messages);
                            modified_messages = true;
                        },
                        HookEffect::Allow
                        | HookEffect::ModifiedInput { .. }
                        | HookEffect::ModifiedResult { .. }
                        | HookEffect::ModifiedOutput { .. }
                        | HookEffect::PromptContributions(_)
                        | HookEffect::CompactContributions(_) => {},
                    }
                },
                HookMode::NonBlocking => {
                    let evt = event.clone();
                    let snap_ctx = hook_ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), &hook_ctx).await;
                },
            }
        }

        Ok(match (modified_messages, current_messages) {
            (true, Some(messages)) => ProviderHookOutcome::ModifiedMessages { messages },
            _ => ProviderHookOutcome::Allow,
        })
    }

    /// 分发 PromptBuild hook，收集插件提供的 system/skills/agents 片段。
    pub async fn collect_prompt_contributions(
        &self,
        ctx: &dyn ExtensionContext,
    ) -> Result<PromptContributions, ExtensionError> {
        let mut collected = PromptContributions::default();

        for ordered in self
            .ordered_extensions_for(&ExtensionEvent::PromptBuild)
            .await
        {
            let ext = ordered.ext;

            match ordered.mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(
                        self.timeout,
                        ext.on_event(ExtensionEvent::PromptBuild, ctx),
                    )
                    .await
                    .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;

                    match result {
                        HookEffect::PromptContributions(contributions) => {
                            collected.merge(contributions);
                        },
                        HookEffect::Block { reason } => {
                            return Err(ExtensionError::Blocked { reason });
                        },
                        _ => {},
                    }
                },
                HookMode::Advisory => {
                    if let HookEffect::PromptContributions(contributions) =
                        ext.on_event(ExtensionEvent::PromptBuild, ctx).await?
                    {
                        collected.merge(contributions);
                    }
                },
                HookMode::NonBlocking => {
                    tracing::warn!(
                        "extension {} subscribes to PromptBuild as NonBlocking; prompt \
                         contributions require Blocking or Advisory mode",
                        ext.id()
                    );
                },
            }
        }

        Ok(collected)
    }

    /// 分发 PreCompact hook，收集插件提供的 compact 摘要指令。
    pub async fn collect_compact_contributions(
        &self,
        ctx: &dyn ExtensionContext,
    ) -> Result<CompactContributions, ExtensionError> {
        let mut collected = CompactContributions::default();

        for ordered in self
            .ordered_extensions_for(&ExtensionEvent::PreCompact)
            .await
        {
            let ext = ordered.ext;

            match ordered.mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(
                        self.timeout,
                        ext.on_event(ExtensionEvent::PreCompact, ctx),
                    )
                    .await
                    .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;

                    match result {
                        HookEffect::CompactContributions(contributions) => {
                            collected.merge(contributions);
                        },
                        HookEffect::Block { reason } => {
                            return Err(ExtensionError::Blocked { reason });
                        },
                        _ => {},
                    }
                },
                HookMode::Advisory => {
                    if let HookEffect::CompactContributions(contributions) =
                        ext.on_event(ExtensionEvent::PreCompact, ctx).await?
                    {
                        collected.merge(contributions);
                    }
                },
                HookMode::NonBlocking => {
                    tracing::warn!(
                        "extension {} subscribes to PreCompact as NonBlocking; compact \
                         contributions require Blocking or Advisory mode",
                        ext.id()
                    );
                },
            }
        }

        Ok(collected)
    }

    /// 当前已注册的扩展数量。
    pub async fn count(&self) -> usize {
        self.extensions.read().await.len()
    }

    /// 从所有已注册的扩展收集工具定义。
    pub async fn collect_tools(&self) -> Vec<astrcode_core::tool::ToolDefinition> {
        let exts = self.extensions.read().await;
        let mut tools = Vec::new();
        for ext in exts.iter() {
            tools.extend(ext.tools());
        }
        tools
    }

    /// 从所有已注册的扩展收集可执行的工具适配器。
    pub async fn collect_tool_adapters(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        let exts = self.extensions.read().await;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for ext in exts.iter() {
            for def in ext.tools() {
                tools.push(Arc::new(ExtensionTool {
                    extension: Arc::clone(ext),
                    definition: def,
                    working_dir: working_dir.to_string(),
                    runtime: Arc::clone(&self.runtime),
                }));
            }
        }
        tools
    }

    /// 从所有已注册的扩展收集斜杠命令。
    pub async fn collect_commands(&self) -> Vec<astrcode_core::extension::SlashCommand> {
        let exts = self.extensions.read().await;
        let mut cmds = Vec::new();
        for ext in exts.iter() {
            cmds.extend(ext.slash_commands());
        }
        cmds
    }

    async fn ordered_extensions_for(&self, event: &ExtensionEvent) -> Vec<OrderedExtension> {
        let exts: Vec<Arc<dyn Extension>> = { self.extensions.read().await.clone() };
        let mut matched = exts
            .into_iter()
            .enumerate()
            .filter_map(|(index, ext)| {
                let subscription = ext
                    .hook_subscriptions()
                    .into_iter()
                    .find(|sub| &sub.event == event)?;
                Some((index, subscription.priority, subscription.mode, ext))
            })
            .collect::<Vec<_>>();

        matched.sort_by(
            |(left_index, left_priority, _, _), (right_index, right_priority, _, _)| {
                right_priority
                    .cmp(left_priority)
                    .then_with(|| left_index.cmp(right_index))
            },
        );

        matched
            .into_iter()
            .map(|(_, _, mode, ext)| OrderedExtension { ext, mode })
            .collect()
    }
}

/// 扩展工具适配器，将扩展注册的工具包装为 `Tool` trait 实现。
struct ExtensionTool {
    /// 所属扩展引用
    extension: Arc<dyn Extension>,
    /// 工具定义
    definition: ToolDefinition,
    /// 工作目录
    working_dir: String,
    /// 共享运行时（用于处理 RunSession 声明式结果）
    runtime: Arc<ExtensionRuntime>,
}

#[async_trait::async_trait]
impl Tool for ExtensionTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    /// 扩展工具的实际执行逻辑。
    ///
    /// 调用扩展的工具回调，并处理声明式 RunSession 结果：
    /// 如果工具返回 RunSession，则通过运行时派生子会话。
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let mut result = self
            .extension
            .execute_tool(&self.definition.name, arguments, &self.working_dir, _ctx)
            .await
            .map_err(extension_error_to_tool_error)?;

        // 处理声明式结果: RunSession → 派生子会话
        if let Some(outcome_value) = result.metadata.remove("extension_tool_outcome") {
            if let Ok(ExtensionToolOutcome::RunSession {
                name,
                system_prompt,
                user_prompt,
                allowed_tools,
                model_preference,
            }) = serde_json::from_value(outcome_value)
            {
                // 如果未指定允许的工具，则继承父会话的所有工具
                let effective_tools = if allowed_tools.is_empty() {
                    _ctx.available_tools
                        .iter()
                        .map(|t| t.name.clone())
                        .collect()
                } else {
                    allowed_tools
                };

                let request = SpawnRequest {
                    name,
                    system_prompt,
                    user_prompt,
                    working_dir: _ctx.working_dir.clone(),
                    allowed_tools: effective_tools,
                    model_preference,
                    tool_call_id: _ctx.tool_call_id.clone(),
                    event_tx: _ctx.event_tx.clone(),
                };

                match self.runtime.spawn(&_ctx.session_id, request).await {
                    Ok(output) => {
                        result.content = output.content;
                        result
                            .metadata
                            .insert("child_session_id".into(), output.child_session_id.into());
                    },
                    Err(e) => {
                        result.content = format!("Failed to spawn child session: {e}");
                        result.is_error = true;
                        result.error = Some(e);
                    },
                }
            }
        }

        Ok(result)
    }
}

/// 将 [`ExtensionError`] 转换为 [`ToolError`]。
fn extension_error_to_tool_error(err: ExtensionError) -> ToolError {
    match err {
        ExtensionError::NotFound(name) => ToolError::NotFound(name),
        ExtensionError::Timeout(ms) => ToolError::Timeout(ms),
        ExtensionError::Blocked { reason } => ToolError::Blocked(reason),
        ExtensionError::Internal(message) => ToolError::Execution(message),
    }
}

/// 工具级别钩子分发的结果。
#[derive(Debug, Clone)]
pub enum ToolHookOutcome {
    /// 允许继续执行
    Allow,
    /// 阻止执行，附带阻止原因
    Blocked { reason: String },
    /// 修改了工具输入
    ModifiedInput { tool_input: serde_json::Value },
    /// 修改了工具结果
    ModifiedResult { content: String },
}

/// 提供者级别钩子分发的结果。
#[derive(Debug, Clone)]
pub enum ProviderHookOutcome {
    /// 允许继续执行
    Allow,
    /// 阻止执行，附带阻止原因
    Blocked { reason: String },
    /// 修改了发送给提供者的消息列表
    ModifiedMessages {
        messages: Vec<astrcode_core::llm::LlmMessage>,
    },
}

struct ProviderMessagesContext<'a> {
    base: &'a dyn ExtensionContext,
    messages: Option<Vec<LlmMessage>>,
}

#[async_trait::async_trait]
impl ExtensionContext for ProviderMessagesContext<'_> {
    fn session_id(&self) -> &str {
        self.base.session_id()
    }

    fn working_dir(&self) -> &str {
        self.base.working_dir()
    }

    fn model_selection(&self) -> ModelSelection {
        self.base.model_selection()
    }

    fn config_value(&self, key: &str) -> Option<String> {
        self.base.config_value(key)
    }

    async fn emit_custom_event(&self, name: &str, data: serde_json::Value) {
        self.base.emit_custom_event(name, data).await;
    }

    fn find_tool(&self, name: &str) -> Option<ToolDefinition> {
        self.base.find_tool(name)
    }

    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.base.pre_tool_use_input()
    }

    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.base.post_tool_use_input()
    }

    fn pre_compact_input(&self) -> Option<PreCompactInput> {
        self.base.pre_compact_input()
    }

    fn post_compact_input(&self) -> Option<PostCompactInput> {
        self.base.post_compact_input()
    }

    fn register_tool(&self, def: ToolDefinition) {
        self.base.register_tool(def);
    }

    fn drain_registered_tools(&self) -> Vec<ToolDefinition> {
        self.base.drain_registered_tools()
    }

    fn provider_messages(&self) -> Option<Vec<LlmMessage>> {
        self.messages.clone()
    }

    fn log_warn(&self, msg: &str) {
        self.base.log_warn(msg);
    }

    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ProviderMessagesSnapshot {
            base: self.base.snapshot(),
            messages: self.messages.clone(),
        })
    }
}

struct ProviderMessagesSnapshot {
    base: Arc<dyn ExtensionContext>,
    messages: Option<Vec<LlmMessage>>,
}

#[async_trait::async_trait]
impl ExtensionContext for ProviderMessagesSnapshot {
    fn session_id(&self) -> &str {
        self.base.session_id()
    }

    fn working_dir(&self) -> &str {
        self.base.working_dir()
    }

    fn model_selection(&self) -> ModelSelection {
        self.base.model_selection()
    }

    fn config_value(&self, key: &str) -> Option<String> {
        self.base.config_value(key)
    }

    async fn emit_custom_event(&self, name: &str, data: serde_json::Value) {
        self.base.emit_custom_event(name, data).await;
    }

    fn find_tool(&self, name: &str) -> Option<ToolDefinition> {
        self.base.find_tool(name)
    }

    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.base.pre_tool_use_input()
    }

    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.base.post_tool_use_input()
    }

    fn pre_compact_input(&self) -> Option<PreCompactInput> {
        self.base.pre_compact_input()
    }

    fn post_compact_input(&self) -> Option<PostCompactInput> {
        self.base.post_compact_input()
    }

    fn provider_messages(&self) -> Option<Vec<LlmMessage>> {
        self.messages.clone()
    }

    fn log_warn(&self, msg: &str) {
        self.base.log_warn(msg);
    }

    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ProviderMessagesSnapshot {
            base: self.base.snapshot(),
            messages: self.messages.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use astrcode_core::{config::ModelSelection, extension::PromptContributions};

    use super::*;

    struct PromptContributionExtension;
    struct ProviderReplaceExtension;
    struct ProviderAppendExtension;
    struct OrderedProviderAppendExtension {
        id: &'static str,
        text: &'static str,
        priority: i32,
    }
    struct OrderedToolExtension {
        id: &'static str,
        label: &'static str,
        priority: i32,
        blocks: bool,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl Extension for PromptContributionExtension {
        fn id(&self) -> &str {
            "prompt-contribution"
        }

        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::PromptBuild,
                mode: HookMode::Blocking,
                priority: 0,
            }]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            _ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            assert_eq!(event, ExtensionEvent::PromptBuild);
            Ok(HookEffect::PromptContributions(PromptContributions {
                system_prompts: vec!["system".to_string()],
                skills: vec!["skill".to_string()],
                agents: vec!["agent".to_string()],
            }))
        }
    }

    #[async_trait::async_trait]
    impl Extension for ProviderReplaceExtension {
        fn id(&self) -> &str {
            "provider-replace"
        }

        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::BeforeProviderRequest,
                mode: HookMode::Blocking,
                priority: 0,
            }]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            _ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            assert_eq!(event, ExtensionEvent::BeforeProviderRequest);
            Ok(HookEffect::ModifiedMessages {
                messages: vec![LlmMessage::user("replaced")],
            })
        }
    }

    #[async_trait::async_trait]
    impl Extension for ProviderAppendExtension {
        fn id(&self) -> &str {
            "provider-append"
        }

        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::BeforeProviderRequest,
                mode: HookMode::Blocking,
                priority: 0,
            }]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            assert_eq!(event, ExtensionEvent::BeforeProviderRequest);
            let messages = ctx
                .provider_messages()
                .expect("provider hook should see current messages");
            assert!(message_texts(&messages).contains(&String::from("replaced")));
            Ok(HookEffect::AppendMessages {
                messages: vec![LlmMessage::user("appended")],
            })
        }
    }

    #[async_trait::async_trait]
    impl Extension for OrderedProviderAppendExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::BeforeProviderRequest,
                mode: HookMode::Blocking,
                priority: self.priority,
            }]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            _ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            assert_eq!(event, ExtensionEvent::BeforeProviderRequest);
            Ok(HookEffect::AppendMessages {
                messages: vec![LlmMessage::user(self.text)],
            })
        }
    }

    #[async_trait::async_trait]
    impl Extension for OrderedToolExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::PreToolUse,
                mode: HookMode::Blocking,
                priority: self.priority,
            }]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            _ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            assert_eq!(event, ExtensionEvent::PreToolUse);
            self.seen
                .lock()
                .expect("record hook order")
                .push(self.label);
            if self.blocks {
                return Ok(HookEffect::Block {
                    reason: self.label.to_string(),
                });
            }
            Ok(HookEffect::Allow)
        }
    }

    struct TestContext;

    #[async_trait::async_trait]
    impl ExtensionContext for TestContext {
        fn session_id(&self) -> &str {
            "session"
        }

        fn working_dir(&self) -> &str {
            "."
        }

        fn model_selection(&self) -> ModelSelection {
            ModelSelection {
                profile_name: String::new(),
                model: "mock".to_string(),
                provider_kind: String::new(),
            }
        }

        fn config_value(&self, _key: &str) -> Option<String> {
            None
        }

        async fn emit_custom_event(&self, _name: &str, _data: serde_json::Value) {}

        fn find_tool(&self, _name: &str) -> Option<ToolDefinition> {
            None
        }

        fn provider_messages(&self) -> Option<Vec<LlmMessage>> {
            Some(vec![LlmMessage::user("original")])
        }

        fn log_warn(&self, _msg: &str) {}

        fn snapshot(&self) -> Arc<dyn ExtensionContext> {
            Arc::new(TestContext)
        }
    }

    fn message_texts(messages: &[LlmMessage]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                astrcode_core::llm::LlmContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn collect_prompt_contributions_merges_prompt_build_hook_output() {
        let runner =
            ExtensionRunner::new(Duration::from_secs(1), Arc::new(ExtensionRuntime::new()));
        runner.register(Arc::new(PromptContributionExtension)).await;

        let contributions = runner
            .collect_prompt_contributions(&TestContext)
            .await
            .expect("collect contributions");

        assert_eq!(contributions.system_prompts, ["system"]);
        assert_eq!(contributions.skills, ["skill"]);
        assert_eq!(contributions.agents, ["agent"]);
    }

    #[tokio::test]
    async fn provider_message_hooks_replace_then_append() {
        let runner =
            ExtensionRunner::new(Duration::from_secs(1), Arc::new(ExtensionRuntime::new()));
        runner.register(Arc::new(ProviderReplaceExtension)).await;
        runner.register(Arc::new(ProviderAppendExtension)).await;

        let outcome = runner
            .dispatch_provider_hook(ExtensionEvent::BeforeProviderRequest, &TestContext)
            .await
            .expect("provider hook dispatch");

        let ProviderHookOutcome::ModifiedMessages { messages } = outcome else {
            panic!("provider hooks should produce modified messages");
        };
        assert_eq!(message_texts(&messages), ["replaced", "appended"]);
    }

    #[tokio::test]
    async fn provider_hooks_use_priority_before_registration_order() {
        let runner =
            ExtensionRunner::new(Duration::from_secs(1), Arc::new(ExtensionRuntime::new()));
        runner
            .register(Arc::new(OrderedProviderAppendExtension {
                id: "low",
                text: "low",
                priority: -1,
            }))
            .await;
        runner
            .register(Arc::new(OrderedProviderAppendExtension {
                id: "high",
                text: "high",
                priority: 10,
            }))
            .await;

        let outcome = runner
            .dispatch_provider_hook(ExtensionEvent::BeforeProviderRequest, &TestContext)
            .await
            .expect("provider hook dispatch");

        let ProviderHookOutcome::ModifiedMessages { messages } = outcome else {
            panic!("provider hooks should produce modified messages");
        };
        assert_eq!(message_texts(&messages), ["original", "high", "low"]);
    }

    #[tokio::test]
    async fn provider_hooks_keep_registration_order_for_equal_priority() {
        let runner =
            ExtensionRunner::new(Duration::from_secs(1), Arc::new(ExtensionRuntime::new()));
        runner
            .register(Arc::new(OrderedProviderAppendExtension {
                id: "first",
                text: "first",
                priority: 0,
            }))
            .await;
        runner
            .register(Arc::new(OrderedProviderAppendExtension {
                id: "second",
                text: "second",
                priority: 0,
            }))
            .await;

        let outcome = runner
            .dispatch_provider_hook(ExtensionEvent::BeforeProviderRequest, &TestContext)
            .await
            .expect("provider hook dispatch");

        let ProviderHookOutcome::ModifiedMessages { messages } = outcome else {
            panic!("provider hooks should produce modified messages");
        };
        assert_eq!(message_texts(&messages), ["original", "first", "second"]);
    }

    #[tokio::test]
    async fn tool_hooks_stop_after_higher_priority_block() {
        let runner =
            ExtensionRunner::new(Duration::from_secs(1), Arc::new(ExtensionRuntime::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        runner
            .register(Arc::new(OrderedToolExtension {
                id: "low",
                label: "low",
                priority: -1,
                blocks: false,
                seen: Arc::clone(&seen),
            }))
            .await;
        runner
            .register(Arc::new(OrderedToolExtension {
                id: "high",
                label: "high",
                priority: 10,
                blocks: true,
                seen: Arc::clone(&seen),
            }))
            .await;

        let outcome = runner
            .dispatch_tool_hook(ExtensionEvent::PreToolUse, &TestContext)
            .await
            .expect("tool hook dispatch");

        let ToolHookOutcome::Blocked { reason } = outcome else {
            panic!("higher priority hook should block");
        };
        assert_eq!(reason, "high");
        assert_eq!(seen.lock().expect("read hook order").as_slice(), ["high"]);
    }
}
