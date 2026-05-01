//! 扩展事件定义 — 从 astrcode-core 重新导出。
//!
//! 本模块统一导出扩展系统所需的事件类型和能力声明，
//! 包括扩展能力、事件枚举、钩子效果、钩子模式和斜杠命令。

pub use astrcode_core::extension::{
    CompactContributions, CompactTrigger, ExtensionCapabilities, ExtensionEvent, HookEffect,
    HookMode, HookSubscription, PostCompactInput, PreCompactInput, SlashCommand,
};
