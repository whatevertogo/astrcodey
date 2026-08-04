//! Best-effort repair of tool-call argument JSON.
//!
//! Some LLM providers emit JSON that is almost valid, especially when a stream
//! ends mid-value. This module repairs a small, deterministic set of mistakes
//! while ensuring unsupported malformed input never reaches a tool.
//!
//! Repair is a hand-written single-pass state machine rather than regex or
//! iterative re-parsing: one scan, no backtracking, fully deterministic output
//! for a given input, and independence from `serde_json`'s own error taxonomy
//! (when repair fails, the original parse error is returned unchanged).

use serde_json::Value;

/// Parse tool-call arguments and repair common provider mistakes.
///
/// The normal path parses without allocating. If parsing fails, one scan:
///
/// - escapes raw control characters inside strings;
/// - removes a comma before a closing bracket or after a complete root value;
/// - closes a string or nested container truncated at the end;
/// - completes a literal truncated mid-value (`{"ok": tru`).
///
/// If the repaired candidate is still invalid, the original parse error is
/// returned so the caller can reject the call and preserve the provider output.
pub(crate) fn parse_and_repair_json(arguments: &str, tool_name: &str) -> serde_json::Result<Value> {
    let original_error = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };

    tracing::warn!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(200).collect::<String>(),
        arguments_len = arguments.len(),
        "Failed to parse tool call arguments, attempting repair"
    );

    let Some(repaired) = repair_tool_arguments(arguments.trim()) else {
        log_repair_failure(tool_name, arguments, &original_error);
        return Err(original_error);
    };

    match serde_json::from_str::<Value>(&repaired) {
        Ok(value) => {
            tracing::debug!(tool = %tool_name, "Successfully repaired tool call arguments");
            Ok(value)
        },
        Err(_) => {
            log_repair_failure(tool_name, arguments, &original_error);
            Err(original_error)
        },
    }
}

/// Produce one repaired candidate, or `None` when the input was unchanged.
fn repair_tool_arguments(arguments: &str) -> Option<String> {
    let mut repaired = String::with_capacity(arguments.len());
    let mut open_containers = Vec::new();
    let mut comma_pending = false;
    let mut whitespace_after_comma = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;

    for ch in arguments.chars() {
        if in_string {
            if escaped {
                escaped = false;
                if ch.is_control() {
                    changed = true;
                    push_escaped_control_char(&mut repaired, ch, false);
                } else {
                    repaired.push(ch);
                }
                continue;
            }

            match ch {
                '\\' => {
                    escaped = true;
                    repaired.push(ch);
                },
                '"' => {
                    in_string = false;
                    repaired.push(ch);
                },
                control if control.is_control() => {
                    changed = true;
                    push_escaped_control_char(&mut repaired, control, true);
                },
                _ => repaired.push(ch),
            }
            continue;
        }

        if comma_pending {
            // 上一个字符是逗号：先缓冲后续空白。只有看到逗号后的第一个非空白字符
            // 才能判断它是「尾逗号」还是正常分隔——缓冲保证修复后不丢失元素间空白。
            if ch.is_whitespace() {
                whitespace_after_comma.push(ch);
                continue;
            }

            if matches!(ch, '}' | ']') {
                // 尾逗号：丢弃逗号本身，仅保留其后的空白（当前 `}`/`]` 交由下方主分支处理）。
                repaired.push_str(&whitespace_after_comma);
                changed = true;
            } else {
                // 正常分隔：把缓冲的逗号和空白写回，再按普通字符处理当前字符。
                repaired.push(',');
                repaired.push_str(&whitespace_after_comma);
            }
            comma_pending = false;
            whitespace_after_comma.clear();
        }

        match ch {
            '"' => {
                in_string = true;
                repaired.push(ch);
            },
            '{' => {
                open_containers.push(OpenContainer::Object);
                repaired.push(ch);
            },
            '[' => {
                open_containers.push(OpenContainer::Array);
                repaired.push(ch);
            },
            '}' if open_containers.last() == Some(&OpenContainer::Object) => {
                open_containers.pop();
                repaired.push(ch);
            },
            ']' if open_containers.last() == Some(&OpenContainer::Array) => {
                open_containers.pop();
                repaired.push(ch);
            },
            ',' => comma_pending = true,
            _ => repaired.push(ch),
        }
    }

    // 输入结束：根级尾逗号直接丢弃；容器内的尾逗号保留（逗号+空白位于随后补上的
    // 闭合括号之前，仍是合法 JSON）。
    if comma_pending {
        if open_containers.is_empty() {
            changed = true;
        } else {
            repaired.push(',');
            repaired.push_str(&whitespace_after_comma);
        }
    }

    if escaped {
        // 字符串在反斜杠后截断（如 `{"text":"abc\`）：去掉悬空的 `\`。
        repaired.pop();
        changed = true;
    }
    if in_string {
        // 字符串在流结束时未闭合：补上闭合引号。
        repaired.push('"');
        changed = true;
    }

    if complete_truncated_literal(&mut repaired, &open_containers) {
        changed = true;
    }

    while let Some(opening) = open_containers.pop() {
        // 按打开顺序逆序补闭合括号（栈顶即最近打开的容器），修复截断的嵌套结构。
        repaired.push(match opening {
            OpenContainer::Object => '}',
            OpenContainer::Array => ']',
        });
        changed = true;
    }

    changed.then_some(repaired)
}

/// 补全在值位置被截断的 JSON 字面量（`tru` → `true`、`fals` → `false`、`nul` → `null`）。
///
/// 仅当截断 token 处于值位置（根级、冒号或数组元素之后）时补全；对象中逗号之后是
/// key 位置（`{"a": 1, tru`），key 截断（`{"tru`）无法安全推断，保持不修复，
/// 由调用方按原始解析错误拒绝。
fn complete_truncated_literal(output: &mut String, open_containers: &[OpenContainer]) -> bool {
    let trimmed_end = output.trim_end();
    let Some(last) = trimmed_end.chars().next_back() else {
        return false;
    };
    if !last.is_ascii_alphabetic() {
        return false;
    }

    // 末尾 token 起点：从后往前第一个非字母字符之后的位置（全字母则为 0）。
    let token_start = trimmed_end
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_ascii_alphabetic())
        .map_or(0, |(idx, ch)| idx + ch.len_utf8());

    let full_literal = match &trimmed_end[token_start..] {
        "t" | "tr" | "tru" => "true",
        "f" | "fa" | "fal" | "fals" => "false",
        "n" | "nu" | "nul" => "null",
        _ => return false,
    };

    let in_value_position = match trimmed_end[..token_start].trim_end().chars().next_back() {
        None => true,      // 根级值，如 `tru`
        Some(':') => true, // 对象/根级冒号之后，如 `{"ok": tru`
        Some('[') => true, // 数组第一个元素，如 `[tru`
        // `[1, tru` 可补；对象中逗号之后是 key 位置，不补。
        Some(',') => matches!(open_containers.last(), Some(OpenContainer::Array)),
        // key 截断（`{"tru`）或紧随完整值（`{"a": "x" tru`），不推断。
        _ => false,
    };
    if !in_value_position {
        return false;
    }

    output.truncate(token_start);
    output.push_str(full_literal);
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenContainer {
    Object,
    Array,
}

fn push_escaped_control_char(output: &mut String, ch: char, include_backslash: bool) {
    if include_backslash {
        output.push('\\');
    }
    match ch {
        '\n' => output.push('n'),
        '\r' => output.push('r'),
        '\t' => output.push('t'),
        '\u{0008}' => output.push('b'),
        '\u{000C}' => output.push('f'),
        other => {
            const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
            let value = other as usize;
            output.push('u');
            for shift in [12, 8, 4, 0] {
                output.push(HEX_DIGITS[(value >> shift) & 0x0f] as char);
            }
        },
    }
}

fn log_repair_failure(tool_name: &str, arguments: &str, error: &serde_json::Error) {
    tracing::error!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(500).collect::<String>(),
        error = %error,
        "All JSON repair attempts failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_common_provider_mistakes() {
        let cases = [
            (
                "valid input",
                r#"{"detail":true}"#,
                serde_json::json!({"detail": true}),
            ),
            (
                "comma after root",
                r#"{"detail":true},"#,
                serde_json::json!({"detail": true}),
            ),
            (
                "trailing commas",
                "{\"items\":[1,2, \n],\"nested\":{\"ready\":true, },}",
                serde_json::json!({"items": [1, 2], "nested": {"ready": true}}),
            ),
            (
                "truncated containers",
                r#"{"todos":[{"status":"com"#,
                serde_json::json!({"todos": [{"status": "com"}]}),
            ),
            (
                "raw control characters",
                "{\"text\":\"line1\ncol1\tcol2\rline2\u{0001}\"}",
                serde_json::json!({"text": "line1\ncol1\tcol2\rline2\u{0001}"}),
            ),
            (
                "backslash before raw newline",
                "{\"text\":\"line1\\\nline2\"}",
                serde_json::json!({"text": "line1\nline2"}),
            ),
            (
                "truncated after backslash",
                r#"{"text":"abc\"#,
                serde_json::json!({"text": "abc"}),
            ),
            (
                "raw newline and truncation",
                "{\"text\":\"line1\nline2",
                serde_json::json!({"text": "line1\nline2"}),
            ),
            (
                "truncated literal true",
                r#"{"ok": tru"#,
                serde_json::json!({"ok": true}),
            ),
            (
                "truncated literal false",
                r#"{"ok": fals"#,
                serde_json::json!({"ok": false}),
            ),
            (
                "truncated literal null in array",
                "[nul",
                serde_json::json!([null]),
            ),
            (
                "truncated literal after array comma",
                "[1, tru",
                serde_json::json!([1, true]),
            ),
            ("truncated literal at root", "tru", serde_json::json!(true)),
        ];

        for (case, input, expected) in cases {
            let actual = parse_and_repair_json(input, "testTool")
                .unwrap_or_else(|error| panic!("{case} was not repaired: {error}"));
            assert_eq!(actual, expected, "{case}");
        }
    }

    #[test]
    fn reports_original_error_for_unsupported_malformed_json() {
        let cases = [
            r#"{"segments":[{"emotion":"NORMAL","text">"news"}]}"#,
            r#"{"items":[1,2,"#,
            // 结构完整但字面量不完整：模型写错而非流截断，不推断。
            r#"{"ok": tru}"#,
            // key 截断无法安全补全。
            r#"{"tru"#,
            // 对象中逗号之后是 key 位置，不是值位置。
            r#"{"a": 1, tru"#,
            // 不是任何字面量的前缀。
            r#"trut"#,
        ];

        for input in cases {
            let original_error = serde_json::from_str::<Value>(input).unwrap_err();
            let repaired_error = parse_and_repair_json(input, "interact").unwrap_err();
            assert_eq!(repaired_error.to_string(), original_error.to_string());
        }
    }
}
