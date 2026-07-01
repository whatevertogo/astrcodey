//! OpenAI-compatible wire implementation.
//!
//! `body` owns JSON request construction and endpoints, `parser` owns SSE event
//! state machines, and `transport` owns response byte decoding. Provider
//! wrappers stay thin and only connect config/model state to these pieces.

pub(crate) mod body;
pub mod parser;
pub(crate) mod transport;

pub(crate) use body::{
    OpenAiRequestConfig, build_input_token_count_body, build_request_body, endpoint_url,
    input_tokens_endpoint,
};
