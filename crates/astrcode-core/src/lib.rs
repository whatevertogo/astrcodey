//! astrcode-core：astrcode 平台的共享类型、trait 和数据模型。
//!
//! 本 crate 是基础层——定义了所有其他 crate 实现或消费的公共接口。
//! 内容限定为:契约类型(wire/持久化格式)、trait 抽象、紧贴这些类型的
//! 投影逻辑。agent loop、turn 调度、compact 编排等运行时行为不在这里,见 `astrcode-session` /
//! `astrcode-context`。
//!
//! # 模块结构
//!
//! - [`config`]：配置系统（原始类型、解析类型、解析逻辑、默认值）
//! - [`event`]：统一的运行时事件与持久化事件类型
//! - [`llm`]：LLM 提供者抽象与消息类型
//! - [`tool`]：工具 trait 及关联类型
//! - [`types`]：核心共享标识符和数据类型
//! - [`hostpaths`] / [`shell`] / [`frontmatter`] / [`discovery`]：跨扩展与宿主共用的工具原语 （原居
//!   `astrcode-extension-sdk`，SDK 保留 re-export）
//!
//! # 导入约定
//!
//! 下游 crate 应使用完整模块路径导入，如 `use astrcode_core::event::EventPayload`，
//! 而非依赖 crate root 的 glob re-export。

pub mod compaction;
pub mod config;
pub mod discovery;
pub mod event;
pub mod frontmatter;
pub mod hostpaths;
pub mod llm;
pub mod message_attachment;
pub mod permission;
pub mod shell;
pub mod tool;
pub mod types;
pub mod user_input;
