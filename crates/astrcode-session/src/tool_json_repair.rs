//! JSON 参数修复。
//!
//! 某些 LLM 提供者可能生成格式不正确的 JSON。
//! 本模块提供解析和修复常见问题的工具函数。

use std::borrow::Cow;

/// 解析并尝试修复 JSON 参数。
///
/// 某些 LLM 提供者（如 glm-5.1）可能生成格式不正确的 JSON。
/// 此函数尝试修复常见问题，如：
/// - 字符串值内包含原始控制字符（如真实换行符而非 `\n`）
/// - 末尾缺少闭合括号
/// - 末尾有多余的逗号
/// - 引号不匹配
pub fn parse_and_repair_json(arguments: &str, tool_name: &str) -> serde_json::Value {
    // 首先尝试直接解析
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        return value;
    }

    // 记录原始错误信息
    tracing::warn!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(200).collect::<String>(),
        arguments_len = arguments.len(),
        "Failed to parse tool call arguments, attempting repair"
    );

    let trimmed = arguments.trim();

    // 尝试修复策略 1：去除末尾的逗号
    if let Some(repaired) = trimmed.strip_suffix(',') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(repaired) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by removing trailing comma"
            );
            return value;
        }
    }

    // 尝试修复策略 2：转义字符串值内的原始控制字符
    // 某些 LLM（如 glm-5.1）会在 JSON 字符串内直接输出换行、制表符等，
    // 这不符合 JSON 规范（控制字符必须转义）。
    let escaped = escape_control_chars_in_json_strings(trimmed);
    if escaped.as_ref() != trimmed {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(escaped.as_ref()) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by escaping control characters in strings"
            );
            return value;
        }
    }

    // 尝试修复策略 3：转义控制字符 + 关闭截断的字符串并补全缺失的闭合括号
    let repaired = close_truncated_json(escaped.as_ref());
    if repaired != escaped.as_ref() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&repaired) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by escaping control chars and closing truncated content"
            );
            return value;
        }
    }

    // 尝试修复策略 4：仅关闭截断（不转义控制字符）
    let repaired = close_truncated_json(trimmed);
    if repaired != trimmed {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&repaired) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by closing truncated content"
            );
            return value;
        }
    }

    // 所有修复尝试都失败，返回空对象
    tracing::error!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(500).collect::<String>(),
        "All JSON repair attempts failed, using empty object"
    );
    serde_json::json!({})
}

/// 将 JSON 字符串值内的原始控制字符转义为 JSON 合法形式。
///
/// 扫描输入，在 JSON 字符串值（双引号内）遇到未转义的控制字符时，
/// 将其替换为对应的 JSON 转义序列（`\n`、`\r`、`\t`、`\uXXXX`）。
/// 不在字符串内的内容（键名、括号、数字等）保持不变。
fn escape_control_chars_in_json_strings(s: &str) -> Cow<'_, str> {
    let mut result = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape_next = false;
    let mut has_changes = false;

    for ch in s.chars() {
        if escape_next {
            escape_next = false;
            // 如果反斜杠后面跟的是控制字符，需要转义它
            // 例如 LLM 输出 \ 后跟真实换行 → 变成 \\n
            if ch.is_control() {
                has_changes = true;
                match ch {
                    '\n' => result.push('n'),
                    '\r' => result.push('r'),
                    '\t' => result.push('t'),
                    '\u{0008}' => result.push('b'),
                    '\u{000C}' => result.push('f'),
                    c => {
                        // 已经有一个 \ 前缀，追加 uXXXX
                        result.push_str(&format!("u{:04x}", c as u32));
                    },
                }
            } else {
                result.push(ch);
            }
            continue;
        }
        if ch == '\\' {
            escape_next = true;
            result.push(ch);
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            continue;
        }
        if in_string && ch.is_control() {
            has_changes = true;
            match ch {
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                '\u{0008}' => result.push_str("\\b"),
                '\u{000C}' => result.push_str("\\f"),
                c => {
                    // 其他控制字符用 \uXXXX 表示
                    result.push_str(&format!("\\u{:04x}", c as u32));
                },
            }
        } else {
            result.push(ch);
        }
    }

    // 快速路径：没有控制字符需要转义，直接返回原字符串
    if !has_changes {
        return Cow::Borrowed(s);
    }
    Cow::Owned(result)
}

/// 关闭截断的 JSON：补上未闭合的字符串引号和缺失的括号。
///
/// 常见场景：LLM 流式响应被中断，导致工具调用参数 JSON 被截断，
/// 如 `{"todos": [{"status": "com` → `{"todos": [{"status": "com"}]}`。
fn close_truncated_json(s: &str) -> String {
    let mut result = s.to_string();

    // 用栈跟踪嵌套层级，确保按正确逆序关闭括号
    let mut in_string = false;
    let mut escape_next = false;
    let mut bracket_stack: Vec<char> = Vec::new();

    for ch in result.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            match ch {
                '{' | '[' => bracket_stack.push(ch),
                '}' if bracket_stack.last() == Some(&'{') => {
                    bracket_stack.pop();
                },
                ']' if bracket_stack.last() == Some(&'[') => {
                    bracket_stack.pop();
                },
                _ => {},
            }
        }
    }

    // 如果末尾有未完成的转义序列（如截断在 \ 后），先移除尾部的 \
    // 否则后续补的 " 会被 \" 转义掉，导致字符串未真正闭合
    if escape_next && result.ends_with('\\') {
        result.pop();
    }

    // 补上缺失的闭合引号
    if in_string {
        result.push('"');
    }

    // 按嵌套逆序关闭剩余未闭合的括号
    while let Some(opening) = bracket_stack.pop() {
        match opening {
            '{' => result.push('}'),
            '[' => result.push(']'),
            _ => {},
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_truncated_json_closes_open_string() {
        let result = close_truncated_json(r#"{"todos": [{"status": "com"#);
        assert_eq!(result, r#"{"todos": [{"status": "com"}]}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["todos"][0]["status"], "com");
    }

    #[test]
    fn close_truncated_json_handles_escaped_quotes() {
        let result = close_truncated_json(r#"{"text": "say \"hello"#);
        assert_eq!(result, r#"{"text": "say \"hello"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], r#"say "hello"#);
    }

    #[test]
    fn close_truncated_json_adds_brackets_without_string() {
        let result = close_truncated_json(r#"{"key": {"nested": [1, 2"#);
        assert_eq!(result, r#"{"key": {"nested": [1, 2]}}"#);
        let _: serde_json::Value = serde_json::from_str(&result).unwrap();
    }

    #[test]
    fn close_truncated_json_no_change_for_valid_json() {
        let input = r#"{"todos": []}"#;
        assert_eq!(close_truncated_json(input), input);
    }

    #[test]
    fn parse_and_repair_json_handles_truncated_string() {
        let result = parse_and_repair_json(r#"{"todos": [{"status": "com"#, "testTool");
        assert_eq!(result["todos"][0]["status"], "com");
    }

    #[test]
    fn parse_and_repair_json_returns_empty_on_garbage() {
        let result = parse_and_repair_json("not json at all {{{", "testTool");
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn escape_control_chars_escapes_raw_newlines() {
        let input = "{\"text\": \"line1\nline2\"}";
        let result = escape_control_chars_in_json_strings(input);
        assert_eq!(result, r#"{"text": "line1\nline2"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], "line1\nline2");
    }

    #[test]
    fn escape_control_chars_escapes_tab_and_carriage_return() {
        let input = "{\"text\": \"col1\tcol2\r\nend\"}";
        let result = escape_control_chars_in_json_strings(input);
        assert_eq!(result, r#"{"text": "col1\tcol2\r\nend"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], "col1\tcol2\r\nend");
    }

    #[test]
    fn escape_control_chars_no_change_for_valid_json() {
        let input = r#"{"text": "already\\nescaped"}"#;
        assert_eq!(escape_control_chars_in_json_strings(input), input);
    }

    #[test]
    fn parse_and_repair_json_handles_raw_newlines_in_string() {
        // Simulates what weak LLMs produce: real newlines inside JSON string values
        let input = "{\"newStr\": \"use std::sync::Arc;\n\nuse agent::AgentConfig;\"}";
        let result = parse_and_repair_json(input, "edit");
        assert_eq!(
            result["newStr"],
            "use std::sync::Arc;\n\nuse agent::AgentConfig;"
        );
    }

    #[test]
    fn parse_and_repair_json_handles_raw_newlines_and_truncation() {
        // Both raw newlines AND truncation
        let input = "{\"newStr\": \"line1\nline2";
        let result = parse_and_repair_json(input, "edit");
        assert_eq!(result["newStr"], "line1\nline2");
    }

    #[test]
    fn escape_control_chars_preserves_non_string_content() {
        // Control chars outside strings should NOT be escaped
        let input = "{\n  \"key\": \"value\"\n}";
        // The \n between { and "key" are outside strings — should be preserved as-is
        let result = escape_control_chars_in_json_strings(input);
        assert_eq!(result, input);
    }

    #[test]
    fn escape_control_chars_handles_backslash_before_control_char() {
        // LLM outputs \ followed by real newline inside string → should become \n
        let input = "{\"text\": \"line1\\\nline2\"}";
        let result = escape_control_chars_in_json_strings(input);
        assert_eq!(result, r#"{"text": "line1\nline2"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], "line1\nline2");
    }

    #[test]
    fn parse_and_repair_json_handles_backslash_before_newline_in_large_edit() {
        // Simulates the actual failure scenario from logs:
        // LLM generates large edit JSON with \+real-newline in string values
        let input = "{\"edits\": [{\"newStr\": \"use std::sync::Arc;\\\n\\\nuse \
                     agent::AgentConfig;\", \"oldStr\": \"use std::sync::Arc;\"}]}";
        let result = parse_and_repair_json(input, "edit");
        assert_eq!(
            result["edits"][0]["newStr"],
            "use std::sync::Arc;\n\nuse agent::AgentConfig;"
        );
        assert_eq!(result["edits"][0]["oldStr"], "use std::sync::Arc;");
    }

    #[test]
    fn close_truncated_json_handles_trailing_backslash() {
        // Truncated right after a backslash inside a string
        let result = close_truncated_json(r#"{"text": "abc\"#);
        assert_eq!(result, r#"{"text": "abc"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], "abc");
    }
}
