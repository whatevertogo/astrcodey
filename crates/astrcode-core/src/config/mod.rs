//! astrcode 配置系统。
//!
//! # 架构
//!
//! 配置知识集中在本目录，按「磁盘配置 → 有效配置 → 运行时消费」分层：
//!
//! | 模块 | 类型 | 职责 |
//! |------|------|------|
//! | [`raw`] | `Config`, `ConfigOverlay`, `Profile`, `RuntimeSection` | 与 `config.toml` 字段一一对应的 serde 类型（字段多为 `Option`） |
//! | [`effective`] | `EffectiveConfig`, `LlmSettings`, … | 解析后的具体值，供 LLM / compact / 扩展加载使用 |
//! | [`resolve`] | `into_effective()`, `merge_overlay()`, `resolve_api_key()` | 纯函数解析与项目覆盖合并 |
//! | [`defaults`] | 常量与 serde 默认值函数 | 内置默认 profile、超时、compact 阈值等 |
//!
//! # 配置文件
//!
//! | 路径 | 格式 | 说明 |
//! |------|------|------|
//! | `~/.astrcode/config.toml` | [`Config`] | 全局主配置；缺失时兼容旧 `config.json` |
//! | `<workspace>/.astrcode/config.toml` | [`ConfigOverlay`] | 项目覆盖（启动时合并进全局）；缺失时兼容旧 `config.json` |
//! | `~/.astrcode/mcp.json` | MCP 专用 JSON | MCP 服务器（**不**走 `extensions` 段） |
//! | `<workspace>/.astrcode/mcp.json` | 同上 | 项目 MCP（需 `ASTRCODE_ENABLE_PROJECT_MCP=1`） |
//!
//! 持久化由 [`ConfigStore`] trait 抽象；默认实现见 `astrcode-storage::FileConfigStore`。
//!
//! # 解析流程
//!
//! 1. 加载 `~/.astrcode/config.toml`（不存在则写入内置默认；旧 `config.json` 作为 fallback）。
//! 2. 若存在 `<startup_cwd>/.astrcode/config.toml`，[`merge_overlay`] 合并 [`ConfigOverlay`]。
//! 3. [`Config::effective_from()`] 解析主模型 / 小模型、API key、runtime、permissions、extensions。
//! 4. 解析失败时服务端回退到 `.last-known-good.toml`、旧 `.last-known-good.json` 或内置 dummy
//!    LLM（见 `astrcode-server::bootstrap::config_resolve`）。
//!
//! # 新增字段
//!
//! 在以下三处同步添加：`raw.rs`（`Option` 字段）→ `effective.rs`（具体字段，若需）
//! → `resolve.rs`（`build_*` 映射）；项目覆盖若需支持，在 [`ConfigOverlay`] 与 [`merge_overlay`]
//! 中补充。
//!
//! 用户可见说明见仓库根目录 [`docs/configuration.md`](../../../../docs/configuration.md)。

pub mod defaults;
pub mod effective;
pub mod provider_catalog;
pub mod raw;
pub mod resolve;

pub use effective::*;
pub use provider_catalog::*;
pub use raw::*;
pub use resolve::{ResolveError, merge_overlay, profile_has_resolvable_api_key, resolve_api_key};

/// 配置持久化 trait。
///
/// 实现类（如 `FileConfigStore`）负责 IO；`ConfigManager` 加载原始配置后调用
/// [`Config::into_effective()`] 解析。
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
