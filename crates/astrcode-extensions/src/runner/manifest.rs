use astrcode_extension_sdk::extension::{ExtensionCapability, Registrar};

/// 宿主完成注册解析与校验后的运行时清单。
///
/// 所有扩展来源都经 `Extension` 与 `Registrar` 收敛到这里；索引、快照与能力检查只读取它。
pub(super) struct ResolvedExtensionManifest {
    pub(super) id: String,
    pub(super) capabilities: Vec<ExtensionCapability>,
    pub(super) registrations: Registrar,
}
