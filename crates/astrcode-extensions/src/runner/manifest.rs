use astrcode_extension_sdk::extension::{ExtensionManifest, ExtensionRegistrations};

/// 宿主完成注册解析与校验后的运行时清单。
///
/// 所有扩展来源都经 `Extension` 与 `Registrar` 收敛到这里；索引、快照与能力检查只读取它。
pub(super) struct ResolvedExtensionManifest {
    pub(super) author: ExtensionManifest,
    pub(super) registrations: ExtensionRegistrations,
}

impl ResolvedExtensionManifest {
    pub(super) fn id(&self) -> &str {
        self.author.id()
    }

    pub(super) fn capabilities(&self) -> &[astrcode_extension_sdk::extension::ExtensionCapability] {
        self.author.capabilities()
    }

    pub(super) fn required_transport_features(
        &self,
    ) -> &[astrcode_extension_sdk::extension::TransportFeature] {
        self.author.required_transport_features()
    }
}
