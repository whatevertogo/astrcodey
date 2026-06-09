//! astrcode-core：astrcode 平台的共享类型、trait 和数据模型。
//!
//! 本 crate 是基础层——定义了所有其他 crate 实现或消费的公共接口。
//! 不包含业务逻辑。
//!
//! # 模块结构
//!
//! - [`config`]：配置系统（原始类型、解析类型、解析逻辑、默认值）
//! - [`event`]：统一的运行时事件与持久化事件类型
//! - [`extension`]：扩展与钩子系统类型
//! - [`lifecycle`]：session 生命周期相关的 trait
//! - [`llm`]：LLM 提供者抽象与消息类型
//! - [`prompt`]：提示词组合 trait 和类型
//! - [`read_tool_image`]：read 工具内联图片 tool result 契约
//! - [`render`]：结构化 UI 渲染协议
//! - [`storage`]：会话存储 trait
//! - [`tool`]：工具 trait 及关联类型
//! - [`types`]：核心共享标识符和数据类型
//!
//! # 导入约定
//!
//! 下游 crate 应使用完整模块路径导入，如 `use astrcode_core::event::EventPayload`，
//! 而非依赖 crate root 的 glob re-export。

pub mod config;
pub mod context;
pub mod event;
pub mod extension;
pub mod lifecycle;
pub mod llm;
pub mod message_attachment;
pub mod permission;
pub mod prompt;
pub mod read_tool_image;
pub mod render;
pub mod storage;
pub mod tool;
pub mod tool_access;
pub mod tool_ui;
pub mod types;
