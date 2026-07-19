use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};

use super::{
    AfterToolResultsHandler, AfterToolResultsRegistration, CommandDiscoveryHandler, CommandHandler,
    CompactEvent, CompactHandler, ContinueAfterStopHandler, ContinueAfterStopOptions,
    ContinueAfterStopRegistration, ExtensionEvent, ExtensionEventDecl, ExtensionEventDeclBuilder,
    ExtensionHttpHandler, ExtensionHttpRoute, ExtensionHttpRouteRegistration, HookMode,
    LifecycleHandler, PostToolUseFailureHandler, PostToolUseHandler, PreToolUseHandler,
    PromptBuildHandler, ProviderEvent, ProviderHandler, SlashCommand, ToolDiscoveryHandler,
    ToolHandler, ToolHookRegistration, ToolHookTarget, UserMessageEnvelopeHandler,
    UserMessageEnvelopeRegistration,
};
use crate::{
    tool::{ToolDefinition, ToolPromptMetadata},
    tool_ui::ToolUiWire,
};

// ─── Registrar ───────────────────────────────────────────────────

/// 扩展能力注册器。
///
/// 在 `Extension::register()` 调用期间有效，扩展通过它声明自己提供的能力。
///
/// 字段全部私有，外部只能通过 `tool` / `command` / `on_pre_tool_use` 等
/// 写入方法和 `tools()` / `commands()` 等读取 accessor 访问。这样保证：
/// 1. 扩展作者只能用受控 API 注册能力，无法旁路构造非法状态；
/// 2. 字段重构（合并、增加索引）不会破坏外部代码；
/// 3. `Registrar` 只在 `Extension::register()` 生命周期内有效，私有字段
///    阻止外部把它当成长寿数据持有。
#[derive(Default)]
pub struct Registrar {
    tools: Vec<(ToolDefinition, Arc<dyn ToolHandler>)>,
    tool_discovery: Vec<Arc<dyn ToolDiscoveryHandler>>,
    tool_metadata: HashMap<String, ToolPromptMetadata>,
    tool_ui: HashMap<String, ToolUiWire>,
    commands: Vec<(SlashCommand, Arc<dyn CommandHandler>)>,
    command_discovery: Vec<Arc<dyn CommandDiscoveryHandler>>,
    http_routes: Vec<ExtensionHttpRouteRegistration>,
    keybindings: Vec<Keybinding>,
    status_items: Vec<StatusItem>,
    pre_tool_use: Vec<ToolHookRegistration<dyn PreToolUseHandler>>,
    post_tool_use: Vec<ToolHookRegistration<dyn PostToolUseHandler>>,
    provider: Vec<(ProviderEvent, HookMode, i32, Arc<dyn ProviderHandler>)>,
    prompt_build: Vec<(i32, Arc<dyn PromptBuildHandler>)>,
    compact: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)>,
    post_tool_use_failure: Vec<(i32, Arc<dyn PostToolUseFailureHandler>)>,
    continue_after_stop: Vec<ContinueAfterStopRegistration<dyn ContinueAfterStopHandler>>,
    user_message_envelope: Vec<UserMessageEnvelopeRegistration<dyn UserMessageEnvelopeHandler>>,
    after_tool_results: Vec<AfterToolResultsRegistration<dyn AfterToolResultsHandler>>,
    lifecycle: Vec<(ExtensionEvent, HookMode, i32, Arc<dyn LifecycleHandler>)>,
    extension_event_decls: Vec<ExtensionEventDecl>,
    needs_extension_data_dir: bool,
}

impl Registrar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool(&mut self, def: ToolDefinition, handler: Arc<dyn ToolHandler>) {
        self.tools.push((def, handler));
    }

    pub fn tool_discovery(&mut self, handler: Arc<dyn ToolDiscoveryHandler>) {
        self.tool_discovery.push(handler);
    }

    pub fn tool_metadata(&mut self, meta: HashMap<String, ToolPromptMetadata>) {
        self.tool_metadata.extend(meta);
    }

    /// 注册工具前端贡献（按 tool name；宿主投影到 metadata，不进 LLM）。
    pub fn tool_ui(&mut self, ui: HashMap<String, ToolUiWire>) {
        self.tool_ui.extend(ui);
    }

    pub fn command(&mut self, cmd: SlashCommand, handler: Arc<dyn CommandHandler>) {
        self.commands.push((cmd, handler));
    }

    pub fn command_discovery(&mut self, handler: Arc<dyn CommandDiscoveryHandler>) {
        self.command_discovery.push(handler);
    }

    pub fn http_route(
        &mut self,
        route: ExtensionHttpRoute,
        handler: Arc<dyn ExtensionHttpHandler>,
    ) {
        self.http_routes
            .push(ExtensionHttpRouteRegistration { route, handler });
    }

    pub fn keybinding(&mut self, binding: Keybinding) {
        self.keybindings.push(binding);
    }

    pub fn status_item(&mut self, item: StatusItem) {
        self.status_items.push(item);
    }

    pub fn on_pre_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PreToolUseHandler>,
    ) {
        self.on_pre_tool_use_for(ToolHookTarget::All, mode, priority, handler);
    }

    pub fn on_pre_tool_use_for(
        &mut self,
        target: ToolHookTarget,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PreToolUseHandler>,
    ) {
        self.pre_tool_use.push(ToolHookRegistration {
            mode,
            priority,
            target,
            handler,
        });
    }

    pub fn on_post_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PostToolUseHandler>,
    ) {
        self.on_post_tool_use_for(ToolHookTarget::All, mode, priority, handler);
    }

    pub fn on_post_tool_use_for(
        &mut self,
        target: ToolHookTarget,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PostToolUseHandler>,
    ) {
        self.post_tool_use.push(ToolHookRegistration {
            mode,
            priority,
            target,
            handler,
        });
    }

    pub fn on_provider(
        &mut self,
        event: ProviderEvent,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn ProviderHandler>,
    ) {
        match event {
            ProviderEvent::BeforeRequest => {
                self.on_before_provider_request(mode, priority, handler);
            },
            ProviderEvent::AfterResponse => {
                if mode != HookMode::Advisory {
                    tracing::warn!(
                        ?mode,
                        "on_provider(AfterResponse) ignores HookMode; use \
                         on_after_provider_response instead"
                    );
                }
                self.on_after_provider_response(priority, handler);
            },
        }
    }

    /// 注册 provider request hook。
    ///
    /// Request 阶段允许 `Blocking` handler 阻断请求或改写 messages。
    pub fn on_before_provider_request(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn ProviderHandler>,
    ) {
        self.provider
            .push((ProviderEvent::BeforeRequest, mode, priority, handler));
    }

    /// 注册 provider response observer。
    ///
    /// Response 阶段只观察结果，不允许阻断或改写后续流程。
    pub fn on_after_provider_response(&mut self, priority: i32, handler: Arc<dyn ProviderHandler>) {
        self.provider.push((
            ProviderEvent::AfterResponse,
            HookMode::Advisory,
            priority,
            handler,
        ));
    }

    pub fn on_prompt_build(&mut self, priority: i32, handler: Arc<dyn PromptBuildHandler>) {
        self.prompt_build.push((priority, handler));
    }

    pub fn on_compact(
        &mut self,
        event: CompactEvent,
        priority: i32,
        handler: Arc<dyn CompactHandler>,
    ) {
        self.compact.push((event, priority, handler));
    }

    pub fn on_post_tool_use_failure(
        &mut self,
        priority: i32,
        handler: Arc<dyn PostToolUseFailureHandler>,
    ) {
        self.post_tool_use_failure.push((priority, handler));
    }

    pub fn on_continue_after_stop(
        &mut self,
        priority: i32,
        options: ContinueAfterStopOptions,
        handler: Arc<dyn ContinueAfterStopHandler>,
    ) {
        self.continue_after_stop
            .push(ContinueAfterStopRegistration {
                priority,
                options,
                handler,
            });
    }

    pub fn on_user_message_envelope(
        &mut self,
        priority: i32,
        handler: Arc<dyn UserMessageEnvelopeHandler>,
    ) {
        self.user_message_envelope
            .push(UserMessageEnvelopeRegistration { priority, handler });
    }

    pub fn on_after_tool_results(
        &mut self,
        priority: i32,
        handler: Arc<dyn AfterToolResultsHandler>,
    ) {
        self.after_tool_results
            .push(AfterToolResultsRegistration { priority, handler });
    }

    pub fn on_event(
        &mut self,
        event: ExtensionEvent,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn LifecycleHandler>,
    ) {
        self.lifecycle.push((event, mode, priority, handler));
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.tool_discovery.is_empty()
            && self.tool_metadata.is_empty()
            && self.tool_ui.is_empty()
            && self.commands.is_empty()
            && self.command_discovery.is_empty()
            && self.http_routes.is_empty()
            && self.keybindings.is_empty()
            && self.status_items.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.provider.is_empty()
            && self.prompt_build.is_empty()
            && self.compact.is_empty()
            && self.post_tool_use_failure.is_empty()
            && self.continue_after_stop.is_empty()
            && self.user_message_envelope.is_empty()
            && self.after_tool_results.is_empty()
            && self.lifecycle.is_empty()
            && self.extension_event_decls.is_empty()
            && !self.needs_extension_data_dir
    }

    /// 声明插件需要专属数据目录（`~/.astrcode/extension_data/<extension_id>/`）。
    ///
    /// 注册后由 runtime 自动创建目录。插件通过 `hostpaths::extension_data_dir()` 获取路径。
    pub fn extension_data_dir(&mut self) {
        self.needs_extension_data_dir = true;
    }

    /// 声明插件可发出的事件类型，返回构建器。
    pub fn extension_event(&mut self, event_type: &str) -> ExtensionEventDeclBuilder<'_> {
        ExtensionEventDeclBuilder::new(self, event_type)
    }

    pub(super) fn register_extension_event_decl(&mut self, declaration: ExtensionEventDecl) {
        self.extension_event_decls.push(declaration);
    }

    pub fn tools(&self) -> &[(ToolDefinition, Arc<dyn ToolHandler>)] {
        &self.tools
    }

    pub fn tool_discoveries(&self) -> &[Arc<dyn ToolDiscoveryHandler>] {
        &self.tool_discovery
    }

    pub fn all_tool_metadata(&self) -> &HashMap<String, ToolPromptMetadata> {
        &self.tool_metadata
    }

    pub fn all_tool_ui(&self) -> &HashMap<String, ToolUiWire> {
        &self.tool_ui
    }

    pub fn commands(&self) -> &[(SlashCommand, Arc<dyn CommandHandler>)] {
        &self.commands
    }

    pub fn command_discoveries(&self) -> &[Arc<dyn CommandDiscoveryHandler>] {
        &self.command_discovery
    }

    pub fn http_routes(&self) -> &[ExtensionHttpRouteRegistration] {
        &self.http_routes
    }

    pub fn pre_tool_use(&self) -> &[ToolHookRegistration<dyn PreToolUseHandler>] {
        &self.pre_tool_use
    }

    pub fn post_tool_use(&self) -> &[ToolHookRegistration<dyn PostToolUseHandler>] {
        &self.post_tool_use
    }

    pub fn provider(&self) -> &[(ProviderEvent, HookMode, i32, Arc<dyn ProviderHandler>)] {
        &self.provider
    }

    pub fn prompt_build(&self) -> &[(i32, Arc<dyn PromptBuildHandler>)] {
        &self.prompt_build
    }

    pub fn compact(&self) -> &[(CompactEvent, i32, Arc<dyn CompactHandler>)] {
        &self.compact
    }

    pub fn post_tool_use_failure(&self) -> &[(i32, Arc<dyn PostToolUseFailureHandler>)] {
        &self.post_tool_use_failure
    }

    pub fn continue_after_stop(
        &self,
    ) -> &[ContinueAfterStopRegistration<dyn ContinueAfterStopHandler>] {
        &self.continue_after_stop
    }

    pub fn user_message_envelope(
        &self,
    ) -> &[UserMessageEnvelopeRegistration<dyn UserMessageEnvelopeHandler>] {
        &self.user_message_envelope
    }

    pub fn after_tool_results(
        &self,
    ) -> &[AfterToolResultsRegistration<dyn AfterToolResultsHandler>] {
        &self.after_tool_results
    }

    pub fn lifecycle(&self) -> &[(ExtensionEvent, HookMode, i32, Arc<dyn LifecycleHandler>)] {
        &self.lifecycle
    }

    pub fn keybindings(&self) -> &[Keybinding] {
        &self.keybindings
    }

    pub fn status_items(&self) -> &[StatusItem] {
        &self.status_items
    }

    pub fn extension_event_decls(&self) -> &[ExtensionEventDecl] {
        &self.extension_event_decls
    }

    pub fn needs_extension_data_dir(&self) -> bool {
        self.needs_extension_data_dir
    }
}

#[cfg(test)]
mod registrar_tests {
    use super::*;

    #[test]
    fn emptiness_tracks_non_handler_registrations() {
        let mut registrar = Registrar::new();
        assert!(registrar.is_empty());

        registrar.tool_ui(HashMap::from([(
            "example".to_owned(),
            ToolUiWire {
                input: None,
                approval: None,
                result: None,
            },
        )]));
        assert!(!registrar.is_empty());

        let mut registrar = Registrar::default();
        registrar.extension_data_dir();
        assert!(!registrar.is_empty());
    }
}

// ─── Keybinding ──────────────────────────────────────────────────────────

/// 插件注册的快捷键绑定。
///
/// 当用户按下对应组合键时，TUI 将执行关联的斜杠命令（如同用户输入该命令）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    /// 快捷键描述（如 "shift+tab", "ctrl+p"）。
    pub key: String,
    /// 按下时执行的斜杠命令名（不含 `/`）。
    pub command: String,
    /// 可选的命令参数。
    #[serde(default)]
    pub arguments: String,
    /// 人类可读描述（用于帮助/UI 展示）。
    pub description: String,
}

// ─── Status Item ─────────────────────────────────────────────────────────

/// 插件注册的状态栏项。
///
/// 显示在 TUI footer 和前端状态栏中。插件可以通过 `StatusItemUpdate`
/// 通知动态更新内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItem {
    /// 唯一标识符（如 "mode"、"git-branch"）。
    pub id: String,
    /// 初始显示文本。
    pub text: String,
    /// 排序优先级（越小越靠左）。
    #[serde(default)]
    pub priority: i32,
    /// 可选的 tooltip 描述。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}
