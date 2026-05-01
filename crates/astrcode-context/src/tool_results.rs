use std::collections::HashMap;

use astrcode_core::llm::{LlmContent, LlmMessage};

pub fn tool_call_name_map(messages: &[LlmMessage]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        for content in &message.content {
            let LlmContent::ToolCall { call_id, name, .. } = content else {
                continue;
            };
            names.insert(call_id.clone(), name.clone());
        }
    }
    names
}
