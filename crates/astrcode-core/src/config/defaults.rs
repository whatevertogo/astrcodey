//! All default values for configuration.

// ── Profile ──────────────────────────────────────────────────────────────

pub const DEFAULT_VERSION: &str = "1";
pub const DEFAULT_ACTIVE_PROFILE: &str = "deepseek";
pub const DEFAULT_ACTIVE_MODEL: &str = "deepseek-chat";

// ── LLM ──────────────────────────────────────────────────────────────────

pub const DEFAULT_LLM_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LLM_READ_TIMEOUT_SECS: u64 = 90;
pub const DEFAULT_LLM_MAX_RETRIES: u32 = 2;
pub const DEFAULT_LLM_RETRY_BASE_DELAY_MS: u64 = 250;

// ── Serde default functions ──────────────────────────────────────────────

pub fn default_version() -> String {
    DEFAULT_VERSION.into()
}
pub fn default_active_profile() -> String {
    DEFAULT_ACTIVE_PROFILE.into()
}
pub fn default_active_model() -> String {
    DEFAULT_ACTIVE_MODEL.into()
}
pub fn default_profiles() -> Vec<super::raw::Profile> {
    super::raw::raw_default_profiles()
}
