//! 扩展工具的执行上下文。

use std::{ops::Deref, sync::Arc};

use crate::{extension::ExtensionEventSink, tool::ToolExecutionContext};

/// 扩展工具可见的上下文。
///
/// 核心工具上下文只保留通用工具能力；扩展专有能力由本类型在扩展运行时边界
/// 注入，避免 `astrcode-core` 依赖扩展 SDK。
#[derive(Clone)]
pub struct ExtensionToolContext {
    core: ToolExecutionContext,
    pub events: Option<Arc<dyn ExtensionEventSink>>,
}

impl ExtensionToolContext {
    pub fn new(core: ToolExecutionContext, events: Option<Arc<dyn ExtensionEventSink>>) -> Self {
        Self { core, events }
    }

    pub fn core(&self) -> &ToolExecutionContext {
        &self.core
    }
}

impl Deref for ExtensionToolContext {
    type Target = ToolExecutionContext;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl std::fmt::Debug for ExtensionToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionToolContext")
            .field("core", &self.core)
            .field("events", &self.events.as_ref().map(|_| "<event_sink>"))
            .finish()
    }
}
