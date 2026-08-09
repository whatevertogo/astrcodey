//! 配置系统的所有默认值。
//!
//! 集中定义配置常量和 serde 默认值函数，便于统一管理和修改。

use std::path::{Path, PathBuf};

const TEST_HOME_ENV: &str = "ASTRCODE_TEST_HOME";
const USER_HOME_ENV: &str = "ASTRCODE_HOME_DIR";

/// 返回 AstrCode 使用的用户主目录。
///
/// 测试隔离目录优先于用户覆盖；均未设置时使用系统主目录，无法解析则回退到当前目录。
pub fn user_home_dir() -> PathBuf {
    env_path(TEST_HOME_ENV)
        .or_else(|| env_path(USER_HOME_ENV))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 返回 AstrCode 的进程级数据目录，默认是 `~/.astrcode`。
pub fn astrcode_dir() -> PathBuf {
    user_home_dir().join(".astrcode")
}

/// 返回扩展在指定存储基目录下的数据目录：`<base>/extension_data/<extension_id>`。
pub fn extension_data_dir(base: &Path, extension_id: &str) -> PathBuf {
    base.join("extension_data").join(extension_id)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// ── 配置文件版本与默认选项 ──────────────────────────────────────────────

/// 配置文件格式的默认版本号。
pub(crate) const DEFAULT_VERSION: &str = "1";
/// 默认激活的配置文件名称。
pub(crate) const DEFAULT_ACTIVE_PROFILE: &str = "";
/// 默认激活的模型标识。
pub(crate) const DEFAULT_ACTIVE_MODEL: &str = "";

// ── LLM 连接参数默认值 ─────────────────────────────────────────────────

/// LLM 连接超时时间（秒）。
pub(crate) const DEFAULT_LLM_CONNECT_TIMEOUT_SECS: u64 = 10;
/// LLM 读取超时时间（秒）。
pub(crate) const DEFAULT_LLM_READ_TIMEOUT_SECS: u64 = 90;
/// LLM 最大重试次数。
pub(crate) const DEFAULT_LLM_MAX_RETRIES: u32 = 5;
/// LLM 重试的指数退避基础延迟（毫秒）。
pub(crate) const DEFAULT_LLM_RETRY_BASE_DELAY_MS: u64 = 1_000;
/// 模型未显式配置 `maxTokens` 时的默认值。
pub(crate) const DEFAULT_LLM_MAX_TOKENS: u32 = 8192;
/// 模型未显式配置 `contextLimit` 时的默认值。
pub(crate) const DEFAULT_LLM_CONTEXT_LIMIT: usize = 65536;

// ── Compact 参数默认值 ──────────────────────────────────────────────────

/// 是否启用自动压缩。
pub(crate) const DEFAULT_COMPACT_AUTO_ENABLED: bool = true;
/// 触发自动压缩的上下文占用百分比阈值。
pub(crate) const DEFAULT_COMPACT_THRESHOLD_PERCENT: f32 = 83.5;
/// 压缩失败时的最大重试次数。
pub(crate) const DEFAULT_COMPACT_MAX_RETRY_ATTEMPTS: u8 = 3;
/// LLM 压缩输出的最大 token 数。
pub(crate) const DEFAULT_COMPACT_MAX_OUTPUT_TOKENS: usize = 20_000;
/// 自动/反应式 compact 默认保留的最近完整 turn 数。
pub(crate) const DEFAULT_COMPACT_KEEP_RECENT_TURNS: Option<usize> = Some(1);
/// auto-compact LLM 熔断器触发阈值。
pub(crate) const DEFAULT_COMPACT_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
/// auto-compact LLM 熔断器冷却时间（秒）。
pub(crate) const DEFAULT_COMPACT_CIRCUIT_BREAKER_COOLDOWN_SECS: u64 = 60;
/// 是否启用预测性 compact。
pub(crate) const DEFAULT_PREDICTIVE_COMPACT_ENABLED: bool = false;
/// 预测下一轮 token 增长时的保底值。
pub(crate) const DEFAULT_PREDICTIVE_COMPACT_BASELINE_GROWTH_TOKENS: usize = 15_000;
/// 压缩后恢复的最近读取文件数量上限。
pub(crate) const DEFAULT_POST_COMPACT_MAX_FILES: usize = 5;
/// 压缩后恢复文件的总 token 预算。
pub(crate) const DEFAULT_POST_COMPACT_TOKEN_BUDGET: usize = 50_000;
/// 单个恢复文件的最大 token 数。
pub(crate) const DEFAULT_POST_COMPACT_MAX_TOKENS_PER_FILE: usize = 5_000;

// ── Agent 限制默认值 ────────────────────────────────────────────────────

/// 子 agent 最大嵌套深度（root=0, child=1, grandchild=2）。
pub(crate) const DEFAULT_AGENT_MAX_DEPTH: usize = 2;
/// 单轮中允许同时执行的并行工具调用数上限。
pub(crate) const DEFAULT_AGENT_TOOL_MAX_PARALLEL_CALLS: usize = 5;
/// Shell 工具默认超时时间（秒）。足以覆盖多数构建/安装命令。
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;

// ── Serde 默认值函数 ──────────────────────────────────────────────────

/// serde 用：返回默认配置版本号。
pub(crate) fn default_version() -> String {
    DEFAULT_VERSION.into()
}

/// serde 用：返回默认激活配置文件名。
pub(crate) fn default_active_profile() -> String {
    DEFAULT_ACTIVE_PROFILE.into()
}

/// serde 用：返回默认激活模型标识。
pub(crate) fn default_active_model() -> String {
    DEFAULT_ACTIVE_MODEL.into()
}

/// serde 用：返回内置的默认配置文件列表。
pub(crate) fn default_profiles() -> Vec<super::raw::Profile> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    #[test]
    fn application_directories_follow_test_home() {
        let _guard = env_lock().lock().unwrap();
        let previous = std::env::var_os(TEST_HOME_ENV);
        // SAFETY: tests accessing this variable serialize through `env_lock`.
        unsafe { std::env::set_var(TEST_HOME_ENV, "/tmp/astrcode-config-defaults") };

        assert_eq!(
            user_home_dir(),
            PathBuf::from("/tmp/astrcode-config-defaults")
        );
        assert_eq!(
            astrcode_dir(),
            PathBuf::from("/tmp/astrcode-config-defaults/.astrcode")
        );

        match previous {
            // SAFETY: tests accessing this variable serialize through `env_lock`.
            Some(value) => unsafe { std::env::set_var(TEST_HOME_ENV, value) },
            // SAFETY: tests accessing this variable serialize through `env_lock`.
            None => unsafe { std::env::remove_var(TEST_HOME_ENV) },
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
