//! `astrcode.*` 能力与 [`ExtensionCapability`] 的映射。
//!
//! 线缆名与授权目录名在 `astrcode-extension-contract` 的 `extension_capabilities!`
//! 宏里单点声明；这里只保留以 sdk 路径提供的历史函数名。

use crate::extension::ExtensionCapability;

/// 将 enum 能力映射为 s5r 线缆名（snake_case 请求名）。
pub fn capability_to_wire(cap: ExtensionCapability) -> &'static str {
    cap.as_str()
}

/// manifest / Initialize 请求中的 snake_case 名 → 能力；未知名返回 `None`。
pub fn capability_from_wire(name: &str) -> Option<ExtensionCapability> {
    ExtensionCapability::parse(name)
}

/// 将 enum 能力映射为授权目录名（`astrcode.*` 前缀）。
pub fn astrcode_capability_name(cap: ExtensionCapability) -> &'static str {
    cap.grant_name()
}

pub fn is_astrcode_capability(name: &str) -> bool {
    name.starts_with("astrcode.")
}

pub fn is_reserved_capability_prefix(name: &str) -> bool {
    name.starts_with("handler.") || name.starts_with("astrcode.") || name.starts_with("internal.")
}

/// `astrcode.session.control` 子动作。
pub fn session_control_action(cap: &str) -> Option<&str> {
    cap.strip_prefix("astrcode.session.control.")
}
