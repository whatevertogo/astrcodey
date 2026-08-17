//! In-process Rust extension identity and capability manifest.

use crate::{extension::ExtensionCapability, transport::TransportFeature};

/// Stable authoring manifest returned by [`crate::extension::Extension::manifest`].
///
/// Fields stay private so adding optional display metadata does not turn downstream struct
/// literals into a compatibility boundary. Registrations deliberately live in
/// [`crate::extension::ExtensionRegistrations`], where declarations are bound to handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionManifest {
    id: String,
    name: String,
    version: String,
    description: Option<String>,
    capabilities: Vec<ExtensionCapability>,
    required_transport_features: Vec<TransportFeature>,
}

impl ExtensionManifest {
    pub(crate) fn new(
        id: String,
        name: String,
        version: String,
        description: Option<String>,
        capabilities: Vec<ExtensionCapability>,
        required_transport_features: Vec<TransportFeature>,
    ) -> Self {
        Self {
            id,
            name,
            version,
            description,
            capabilities,
            required_transport_features,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
    }

    pub fn required_transport_features(&self) -> &[TransportFeature] {
        &self.required_transport_features
    }

    pub fn validate(&self) -> Result<(), ExtensionManifestError> {
        validate_extension_id(&self.id)?;
        if self.name.trim().is_empty() {
            return Err(ExtensionManifestError::MissingName {
                id: self.id.clone(),
            });
        }
        if self.version.trim().is_empty() {
            return Err(ExtensionManifestError::MissingVersion {
                id: self.id.clone(),
            });
        }
        if self
            .description
            .as_deref()
            .is_some_and(|description| description.trim().is_empty())
        {
            return Err(ExtensionManifestError::EmptyDescription {
                id: self.id.clone(),
            });
        }
        Ok(())
    }
}

pub fn validate_extension_id(id: &str) -> Result<(), ExtensionManifestError> {
    if id.trim().is_empty() {
        return Err(ExtensionManifestError::MissingId);
    }
    let mut characters = id.chars();
    let starts_with_alphanumeric = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    let remaining_characters_are_valid = characters
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'));
    if !starts_with_alphanumeric || !remaining_characters_are_valid {
        return Err(ExtensionManifestError::InvalidId { id: id.to_owned() });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionManifestError {
    #[error("extension manifest id is required")]
    MissingId,
    #[error(
        "extension manifest id {id:?} must start with an ASCII letter or digit and contain only \
         ASCII letters, digits, '.', '-' or '_'"
    )]
    InvalidId { id: String },
    #[error("extension manifest {id} name is required")]
    MissingName { id: String },
    #[error("extension manifest {id} version is required")]
    MissingVersion { id: String },
    #[error("extension manifest {id} description cannot be empty")]
    EmptyDescription { id: String },
}
