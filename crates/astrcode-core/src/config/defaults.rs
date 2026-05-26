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

// ── Compact 参数默认值 ──────────────────────────────────────────────────

/// 是否启用自动压缩。
pub const DEFAULT_COMPACT_AUTO_ENABLED: bool = true;
/// 触发自动压缩的上下文占用百分比阈值。
pub const DEFAULT_COMPACT_THRESHOLD_PERCENT: f32 = 83.5;
/// 压缩失败时的最大重试次数。
pub const DEFAULT_COMPACT_MAX_RETRY_ATTEMPTS: u8 = 3;
/// LLM 压缩输出的最大 token 数。
pub const DEFAULT_COMPACT_MAX_OUTPUT_TOKENS: usize = 20_000;
/// 自动/反应式 compact 默认保留的最近完整 turn 数。
pub const DEFAULT_COMPACT_KEEP_RECENT_TURNS: Option<usize> = Some(1);
/// auto-compact LLM 熔断器触发阈值。
pub const DEFAULT_COMPACT_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
/// auto-compact LLM 熔断器冷却时间（秒）。
pub const DEFAULT_COMPACT_CIRCUIT_BREAKER_COOLDOWN_SECS: u64 = 60;
/// 是否启用预测性 compact。
pub const DEFAULT_PREDICTIVE_COMPACT_ENABLED: bool = false;
/// 预测下一轮 token 增长时的保底值。
pub const DEFAULT_PREDICTIVE_COMPACT_BASELINE_GROWTH_TOKENS: usize = 15_000;
/// 压缩后恢复的最近读取文件数量上限。
pub const DEFAULT_POST_COMPACT_MAX_FILES: usize = 5;
/// 压缩后恢复文件的总 token 预算。
pub const DEFAULT_POST_COMPACT_TOKEN_BUDGET: usize = 50_000;
/// 单个恢复文件的最大 token 数。
pub const DEFAULT_POST_COMPACT_MAX_TOKENS_PER_FILE: usize = 5_000;

// ── Agent 限制默认值 ────────────────────────────────────────────────────

/// 子 agent 最大嵌套深度（root=0, child=1, grandchild=2）。
pub const DEFAULT_AGENT_MAX_DEPTH: usize = 2;
/// 单轮中允许同时执行的并行工具调用数上限。
pub const DEFAULT_AGENT_TOOL_MAX_PARALLEL_CALLS: usize = 5;
/// Shell 工具默认超时时间（秒）。足以覆盖多数构建/安装命令。
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;

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
