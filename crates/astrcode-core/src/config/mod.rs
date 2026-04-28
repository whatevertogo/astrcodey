//! astrcode 配置系统。
//!
//! # 架构
//!
//! 所有配置知识都集中在此模块中：
//! - `raw.rs`——从磁盘读取的类型（所有字段可选）
//! - `effective.rs`——已解析的类型（所有字段为具体值）
//! - `resolve.rs`——`Config::into_effective()` + `resolve_api_key()` + `merge_overlay()`
//! - `defaults.rs`——所有默认常量
//!
//! 添加新配置字段只需修改此目录下的 3 个文件：
//! `raw.rs`（添加 Option 字段）→ `effective.rs`（添加具体字段）
//! → `resolve.rs`（添加映射行）。
//!
//! # 使用示例
//!
//! ```ignore
//! let raw: Config = serde_json::from_str(&json)?;
//! let effective = raw.into_effective()?;
//! let provider = OpenAiProvider::new(
//!     LlmClientConfig { base_url: effective.llm.base_url, api_key: effective.llm.api_key, ... },
//!     effective.llm.api_mode,
//!     effective.llm.model_id,
//!     Some(effective.llm.max_tokens),
//!     Some(effective.llm.context_limit),
//! );
//! ```

pub mod defaults;
pub mod effective;
pub mod raw;
pub mod resolve;

// 在 config 模块级别重新导出常用类型
pub use effective::*;
pub use raw::*;
pub use resolve::{ResolveError, merge_overlay, resolve_api_key};

// ─── ConfigStore trait（IO 抽象）──────────────────────────────────────────

/// 配置持久化 trait。
///
/// 实现类（如 FileConfigStore）负责处理 IO 层。
/// 服务器 crate 中的 ConfigService 使用此 trait 加载原始配置，
/// 然后调用 `Config::into_effective()` 进行解析。
#[async_trait::async_trait]
pub trait ConfigStore: Send + Sync {
    /// 加载完整配置。
    async fn load(&self) -> Result<Config, ConfigStoreError>;

    /// 保存配置（原子写入）。
    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError>;

    /// 返回配置文件路径。
    fn path(&self) -> std::path::PathBuf;

    /// 加载项目级覆盖配置。
    async fn load_overlay(
        &self,
        working_dir: &str,
    ) -> Result<Option<ConfigOverlay>, ConfigStoreError>;

    /// 保存项目级覆盖配置。
    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError>;
}

/// 配置操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigStoreError {
    /// IO 错误。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 配置内容无效。
    #[error("Invalid config: {0}")]
    Invalid(String),
    /// 缺少必需字段。
    #[error("Missing required field: {0}")]
    MissingField(String),
}
