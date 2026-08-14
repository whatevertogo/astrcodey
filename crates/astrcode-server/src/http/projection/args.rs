//! 工具调用参数 → 折叠摘要文本格式化。

use crate::presentation::inline_preview;

const MAX_ARGUMENT_SUMMARY_CHARS: usize = 140;

/// 将工具调用参数 JSON 格式化为单行摘要文本。
pub(in crate::http) fn format_args_inline(args: &serde_json::Value) -> String {
    if let Some(summary) = primary_argument(args) {
        return inline_preview(&summary, MAX_ARGUMENT_SUMMARY_CHARS);
    }

    match args {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return String::new();
            }
            let pairs = map
                .iter()
                .take(4)
                .map(|(key, value)| {
                    format!("{key}={}", inline_preview(&json_value_inline(value), 48))
                })
                .collect::<Vec<_>>()
                .join(", ");
            inline_preview(&pairs, MAX_ARGUMENT_SUMMARY_CHARS)
        },
        serde_json::Value::String(s) => inline_preview(s, MAX_ARGUMENT_SUMMARY_CHARS),
        serde_json::Value::Null => String::new(),
        other => inline_preview(&other.to_string(), MAX_ARGUMENT_SUMMARY_CHARS),
    }
}

fn primary_argument(args: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 7] = [
        "description",
        "command",
        "path",
        "pattern",
        "query",
        "prompt",
        "action",
    ];
    KEYS.into_iter()
        .find_map(|key| string_arg(args, key).map(str::to_owned))
}

fn string_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn json_value_inline(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_summary_prefers_stable_human_readable_keys() {
        let args = serde_json::json!({
            "command": "cargo test",
            "path": "Cargo.toml",
            "timeout": 30
        });

        assert_eq!(format_args_inline(&args), "cargo test");
    }
}
