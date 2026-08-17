//! Transport features that an extension may require before admission.

use serde::{Deserialize, Serialize};

/// An ingress capability supplied by the active host transport.
///
/// This is intentionally separate from [`crate::wire::ExtensionCapability`]: a capability grants
/// an admitted extension authority, while a transport feature determines whether that extension
/// can be admitted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportFeature {
    AuthenticatedHttp,
}

impl TransportFeature {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthenticatedHttp => "authenticated_http",
        }
    }
}
