use std::collections::HashMap;

use astrcode_core::LlmMessage;

pub fn tool_call_name_map(messages: &[LlmMessage]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        let LlmMessage::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls {
            names.insert(call.id.clone(), call.name.clone());
        }
    }
    names
}
