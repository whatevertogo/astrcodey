//! 工具调用参数 → 折叠摘要文本格式化。

use astrcode_support::text::compact_inline;

const MAX_ARGUMENT_SUMMARY_CHARS: usize = 140;

/// 将工具调用参数 JSON 格式化为单行摘要文本。
pub(in crate::http) fn format_args_inline(tool_name: &str, args: &serde_json::Value) -> String {
    if let Some(summary) = tool_argument_summary(tool_name, args) {
        return compact_inline(&summary, MAX_ARGUMENT_SUMMARY_CHARS);
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
                    format!("{key}={}", compact_inline(&json_value_inline(value), 48))
                })
                .collect::<Vec<_>>()
                .join(", ");
            compact_inline(&pairs, MAX_ARGUMENT_SUMMARY_CHARS)
        },
        serde_json::Value::String(s) => compact_inline(s, MAX_ARGUMENT_SUMMARY_CHARS),
        serde_json::Value::Null => String::new(),
        other => compact_inline(&other.to_string(), MAX_ARGUMENT_SUMMARY_CHARS),
    }
}

fn tool_argument_summary(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    match tool_name {
        "agent" => {
            let description = string_arg(args, "description");
            let subagent_type =
                string_arg(args, "subagentType").or_else(|| string_arg(args, "subagent_type"));
            match (description, subagent_type) {
                (Some(description), Some(subagent_type)) => {
                    Some(format!("{description} ({subagent_type})"))
                },
                (Some(description), None) => Some(description.to_string()),
                (None, Some(subagent_type)) => Some(format!("subagent: {subagent_type}")),
                (None, None) => string_arg(args, "prompt").map(str::to_string),
            }
        },
        "shell" => string_arg(args, "command").map(|command| format!("$ {command}")),
        "read" | "write" | "edit" => string_arg(args, "path").map(str::to_string),
        "glob" => string_arg(args, "pattern").map(|pattern| format!("pattern: {pattern}")),
        "grep" => {
            let pattern = string_arg(args, "pattern").or_else(|| string_arg(args, "query"));
            let path = string_arg(args, "path").or_else(|| string_arg(args, "glob"));
            match (pattern, path) {
                (Some(pattern), Some(path)) => Some(format!("{pattern} in {path}")),
                (Some(pattern), None) => Some(format!("pattern: {pattern}")),
                (None, Some(path)) => Some(path.to_string()),
                (None, None) => None,
            }
        },
        "patch" => Some("workspace patch".into()),
        "todoWrite" => todo_write_argument_summary(args),
        _ => None,
    }
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

fn todo_write_argument_summary(args: &serde_json::Value) -> Option<String> {
    let todos = args.get("todos")?.as_array()?;
    if todos.is_empty() {
        return Some("todoWrite · no items".into());
    }

    let mut pending = 0u32;
    let mut in_progress = 0u32;
    let mut completed = 0u32;
    for item in todos {
        match item.get("status").and_then(serde_json::Value::as_str) {
            Some("pending") => pending += 1,
            Some("in_progress") => in_progress += 1,
            Some("completed") => completed += 1,
            _ => {},
        }
    }

    let mut parts = vec!["todoWrite".to_string()];
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    if in_progress > 0 {
        parts.push(format!("{in_progress} in-progress"));
    }
    if completed > 0 {
        parts.push(format!("{completed} done"));
    }
    Some(parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_write_argument_summary_counts_statuses() {
        let summary = todo_write_argument_summary(&serde_json::json!({
            "todos": [
                { "content": "A", "activeForm": "Doing A", "status": "in_progress" },
                { "content": "B", "activeForm": "Doing B", "status": "pending" },
                { "content": "C", "activeForm": "Doing C", "status": "completed" },
            ]
        }))
        .expect("summary");

        assert_eq!(summary, "todoWrite · 1 pending · 1 in-progress · 1 done");
    }
}
