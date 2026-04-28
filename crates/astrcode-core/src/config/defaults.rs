//! 配置系统的所有默认值。
//!
//! 集中定义配置常量和 serde 默认值函数，便于统一管理和修改。

// ── 配置文件版本与默认选项 ──────────────────────────────────────────────

/// 配置文件格式的默认版本号。
pub const DEFAULT_VERSION: &str = "1";
/// 默认激活的配置文件名称。
pub const DEFAULT_ACTIVE_PROFILE: &str = "deepseek";
/// 默认激活的模型标识。
pub const DEFAULT_ACTIVE_MODEL: &str = "deepseek-chat";

// ── LLM 连接参数默认值 ─────────────────────────────────────────────────

/// LLM 连接超时时间（秒）。
pub const DEFAULT_LLM_CONNECT_TIMEOUT_SECS: u64 = 10;
/// LLM 读取超时时间（秒）。
pub const DEFAULT_LLM_READ_TIMEOUT_SECS: u64 = 90;
/// LLM 最大重试次数。
pub const DEFAULT_LLM_MAX_RETRIES: u32 = 2;
/// LLM 重试的指数退避基础延迟（毫秒）。
pub const DEFAULT_LLM_RETRY_BASE_DELAY_MS: u64 = 250;

// ── Serde 默认值函数 ──────────────────────────────────────────────────

/// serde 用：返回默认配置版本号。
pub fn default_version() -> String {
    DEFAULT_VERSION.into()
}

/// serde 用：返回默认激活配置文件名。
pub fn default_active_profile() -> String {
    DEFAULT_ACTIVE_PROFILE.into()
}

/// serde 用：返回默认激活模型标识。
pub fn default_active_model() -> String {
    DEFAULT_ACTIVE_MODEL.into()
}

/// serde 用：返回内置的默认配置文件列表。
pub fn default_profiles() -> Vec<super::raw::Profile> {
    super::raw::raw_default_profiles()
}
