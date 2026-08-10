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
