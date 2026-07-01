//! Wire-level codecs and request builders.
//!
//! Provider wrappers own lifecycle and transport orchestration; wire modules own protocol shape.

pub(crate) mod anthropic;
pub(crate) mod google_genai;
pub(crate) mod openai;
