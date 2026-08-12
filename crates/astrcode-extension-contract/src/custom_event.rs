//! Custom-event declarations shared by in-process extensions and S5R manifests.

use serde::{Deserialize, Serialize};

pub const DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CUSTOM_EVENT_DURABLE: bool = true;
pub const DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_CUSTOM_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Shared subscription-id limit for in-process and worker registration paths.
pub const MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN: usize = 64;

/// Custom-event type declared by an extension.
///
/// The runtime rejects undeclared event types and payloads above this declaration's limit.
/// The source extension id is host-attributed and is therefore not part of this value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomEventDeclaration {
    pub event_type: String,
    #[serde(default = "default_custom_event_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_custom_event_durable")]
    pub durable: bool,
    #[serde(default = "default_custom_event_max_payload_bytes")]
    pub max_payload_bytes: usize,
}

/// Restricts a custom-event subscription to one producer or accepts every producer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustomEventSourceFilter {
    Any,
    Extension { extension_id: String },
}

/// Exact custom-event subscription registered by a consuming extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomEventSubscription {
    pub id: String,
    #[serde(default = "default_consumer_version")]
    pub consumer_version: u32,
    pub event_type: String,
    pub source: CustomEventSourceFilter,
}

impl CustomEventSubscription {
    /// Subscribes to `event_type` from any producer and derives the id from the event type.
    pub fn any(event_type: impl Into<String>) -> Self {
        let event_type = event_type.into();
        Self {
            id: event_type.clone(),
            consumer_version: default_consumer_version(),
            event_type,
            source: CustomEventSourceFilter::Any,
        }
    }

    /// Subscribes to one producer and derives the id as `{extension_id}:{event_type}`.
    pub fn from_extension(extension_id: impl Into<String>, event_type: impl Into<String>) -> Self {
        let extension_id = extension_id.into();
        let event_type = event_type.into();
        Self {
            id: format!("{extension_id}:{event_type}"),
            consumer_version: default_consumer_version(),
            event_type,
            source: CustomEventSourceFilter::Extension { extension_id },
        }
    }

    pub fn named(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn consumer_version(mut self, version: u32) -> Self {
        self.consumer_version = version;
        self
    }
}

const fn default_consumer_version() -> u32 {
    1
}

const fn default_custom_event_schema_version() -> u32 {
    DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION
}

const fn default_custom_event_durable() -> bool {
    DEFAULT_CUSTOM_EVENT_DURABLE
}

const fn default_custom_event_max_payload_bytes() -> usize {
    DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_strict() {
        assert!(
            serde_json::from_value::<CustomEventDeclaration>(serde_json::json!({
                "event_type": "test.completed",
                "unexpected": true
            }))
            .is_err()
        );
    }
}
