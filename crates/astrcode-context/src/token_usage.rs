//! Token estimation and usage tracking.

pub struct TokenUsageTracker {
    reported_input_tokens: usize,
    reported_output_tokens: usize,
}

impl TokenUsageTracker {
    pub fn new() -> Self {
        Self {
            reported_input_tokens: 0,
            reported_output_tokens: 0,
        }
    }

    /// Estimate tokens for a request using 4/3 multiplier for padding.
    pub fn estimate_request_tokens(&self, text: &str) -> usize {
        (text.len() as f64 * 4.0 / 3.0) as usize
    }

    /// Anchor to provider-reported actual token counts.
    pub fn anchor_actuals(&mut self, input: usize, output: usize) {
        self.reported_input_tokens = input;
        self.reported_output_tokens = output;
    }
}
