//! Author-facing helpers for the contract-owned `handler.invoke` result.

pub use astrcode_extension_contract::effects::{CallContinuation, HandlerResult};
use astrcode_extension_contract::effects::{
    EFFECT_CUSTOM_EVENT_ACK, EFFECT_CUSTOM_EVENT_DEAD_LETTER, EFFECT_CUSTOM_EVENT_RETRY,
};
use serde_json::Value;

use crate::extension::CustomEventDisposition;

impl From<CustomEventDisposition> for HandlerResult {
    fn from(disposition: CustomEventDisposition) -> Self {
        match disposition {
            CustomEventDisposition::Ack => Self::effect(EFFECT_CUSTOM_EVENT_ACK, Value::Null),
            CustomEventDisposition::Retry { reason } => Self::effect(
                EFFECT_CUSTOM_EVENT_RETRY,
                serde_json::json!({ "reason": reason }),
            ),
            CustomEventDisposition::DeadLetter { reason } => Self::effect(
                EFFECT_CUSTOM_EVENT_DEAD_LETTER,
                serde_json::json!({ "reason": reason }),
            ),
        }
    }
}
