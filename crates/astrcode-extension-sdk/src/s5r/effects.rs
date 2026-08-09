//! Author-facing helpers for the contract-owned `handler.invoke` result.

pub use astrcode_extension_contract::effects::{CallContinuation, HandlerResult};
use serde_json::Value;

use crate::extension::CustomEventDisposition;

impl From<CustomEventDisposition> for HandlerResult {
    fn from(disposition: CustomEventDisposition) -> Self {
        match disposition {
            CustomEventDisposition::Ack => Self::effect("custom_event_ack", Value::Null),
            CustomEventDisposition::Retry { reason } => Self::effect(
                "custom_event_retry",
                serde_json::json!({ "reason": reason }),
            ),
            CustomEventDisposition::DeadLetter { reason } => Self::effect(
                "custom_event_dead_letter",
                serde_json::json!({ "reason": reason }),
            ),
        }
    }
}
