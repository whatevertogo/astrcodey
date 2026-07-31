//! Anthropic Messages wire implementation.
//!
//! `body` owns the JSON request contract, `parser` owns the SSE event state machine,
//! and `transport` owns response byte decoding. The provider wrapper stays thin and only
//! connects config/model state to these pieces.

pub(crate) mod body;
pub(crate) mod parser;
pub(crate) mod transport;

pub(crate) use body::{
    AnthropicRequestConfig, build_count_tokens_body, build_request_body, count_tokens_endpoint,
    endpoint_url,
};
