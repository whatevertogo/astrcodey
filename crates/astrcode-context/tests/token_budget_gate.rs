//! token_budget 与 compact 阈值集成冒烟。

use astrcode_context::token_budget::{
    build_prompt_snapshot, compact_threshold_tokens, should_compact,
};
use astrcode_core::llm::{LlmMessage, ModelLimits};

#[test]
fn compact_gate_triggers_at_configured_fraction() {
    let limits = ModelLimits {
        max_input_tokens: 10_000,
        max_output_tokens: 1_024,
    };
    let threshold = compact_threshold_tokens(limits.max_input_tokens, 80.0);
    let messages = vec![LlmMessage::user("x".repeat(threshold.saturating_mul(4)))];
    let snapshot = build_prompt_snapshot(&messages, None, limits, 80.0);
    assert!(should_compact(snapshot));
    assert_eq!(snapshot.threshold_tokens, threshold);
}
