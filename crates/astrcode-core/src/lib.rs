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
//! - [`llm`]：LLM 提供者抽象与消息类型
//! - [`prompt`]：提示词组合 trait 和类型
//! - [`storage`]：会话存储 trait
//! - [`tool`]：工具 trait 及关联类型
//! - [`types`]：核心共享标识符和数据类型

pub mod config;
pub mod event;
pub mod extension;
pub mod llm;
pub mod prompt;
pub mod storage;
pub mod tool;
pub mod types;

// 重新导出常用类型，方便外部 crate 直接使用
pub use config::*;
pub use event::*;
pub use extension::*;
pub use llm::*;
pub use prompt::*;
pub use storage::*;
pub use tool::*;
pub use types::*;
