//! Pure tool-call presentation for the main session and child sessions.

use crate::tui::tool_vocab::tool_display_name;

/// tool_completion_summary 的格式化参数，区分主会话与子 agent 两种展示风格。
pub(super) struct ToolSummaryFormat {
    /// 摘要前缀（主会话 "● "，子 agent 无）
    prefix: &'static str,
    /// shell 单行输出的截断长度
    shell_preview_max: usize,
    /// 默认分支单行输出的截断长度
    fallback_preview_max: usize,
    /// shell 无输出时的文案
    shell_empty: &'static str,
    /// 无实质内容时的完成文案
    done: &'static str,
    /// read 分支的动词前缀
    read_verb: &'static str,
    /// glob 分支的动词前缀
    glob_verb: &'static str,
    /// shell 多行输出计数是否加括号
    shell_lines_parens: bool,
}

pub(super) const MAIN_SUMMARY_FORMAT: ToolSummaryFormat = ToolSummaryFormat {
    prefix: "● ",
    shell_preview_max: 80,
    fallback_preview_max: 60,
    shell_empty: "Ran (no output)",
    done: "Done",
    read_verb: "Read ",
    glob_verb: "Found ",
    shell_lines_parens: true,
};

pub(super) const CHILD_SUMMARY_FORMAT: ToolSummaryFormat = ToolSummaryFormat {
    prefix: "",
    shell_preview_max: 50,
    fallback_preview_max: 50,
    shell_empty: "done",
    done: "done",
    read_verb: "",
    glob_verb: "",
    shell_lines_parens: false,
};

/// 工具完成的单行摘要；主会话调用点已分流 is_error，错误分支仅子 agent 路径触发。
pub(super) fn tool_completion_summary(
    tool_name: &str,
    result: &astrcode_core::tool::ToolResult,
    fmt: &ToolSummaryFormat,
) -> String {
    let content = result.content.trim();
    if result.is_error {
        return truncate_first_line(result.error.as_deref().unwrap_or(content), 60);
    }
    match tool_name {
        "shell" | "shell_poll" => {
            let line_count = content.lines().count();
            if line_count <= 1 && !content.is_empty() {
                format!(
                    "{}{}",
                    fmt.prefix,
                    truncate_first_line(content, fmt.shell_preview_max)
                )
            } else if line_count > 1 {
                if fmt.shell_lines_parens {
                    format!("{}({line_count} lines of output)", fmt.prefix)
                } else {
                    format!("{line_count} lines of output")
                }
            } else {
                format!("{}{}", fmt.prefix, fmt.shell_empty)
            }
        },
        "read" => {
            if content.is_empty() && fmt.read_verb.is_empty() {
                format!("{}{}", fmt.prefix, fmt.done)
            } else {
                format!(
                    "{}{}{} line(s)",
                    fmt.prefix,
                    fmt.read_verb,
                    content.lines().count().max(1)
                )
            }
        },
        "write" | "edit" | "patch" => format!("{}{}", fmt.prefix, fmt.done),
        "glob" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{}{}{count} file(s)", fmt.prefix, fmt.glob_verb)
        },
        "grep" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{}{count} match(es)", fmt.prefix)
        },
        _ => {
            if content.is_empty() {
                format!("{}{}", fmt.prefix, fmt.done)
            } else {
                format!(
                    "{}{}",
                    fmt.prefix,
                    truncate_first_line(content, fmt.fallback_preview_max)
                )
            }
        },
    }
}

pub(super) fn truncate_first_line(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.chars().count() <= max_chars {
        return first_line.to_owned();
    }

    let mut truncated = first_line.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

/// Codex-style one-line tool call summary for the status bar.
pub(super) fn tool_call_summary(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    let action = tool_display_name(tool_name);
    match tool_name {
        "shell" => {
            let command = arguments
                .and_then(|arguments| arguments["command"].as_str())
                .unwrap_or("...");
            format!("Running  $ {}", truncate_first_line(command, 60))
        },
        "shell_poll" => {
            let shell_id = arguments
                .and_then(|arguments| arguments["shellId"].as_str())
                .unwrap_or("...");
            format!("Polling {shell_id}")
        },
        "read" => {
            let path = arguments.and_then(|a| a["path"].as_str()).unwrap_or("...");
            format!("Reading {path}")
        },
        "write" | "edit" => {
            let path = arguments.and_then(|a| a["path"].as_str()).unwrap_or("...");
            format!("{action} {path}")
        },
        "glob" => {
            let pattern = arguments
                .and_then(|a| a["pattern"].as_str())
                .unwrap_or("...");
            format!("Finding {pattern}")
        },
        "grep" => {
            let query = arguments
                .and_then(|a| a["pattern"].as_str().or(a["query"].as_str()))
                .unwrap_or("...");
            format!("Searching {query}")
        },
        "agent" => {
            let desc = arguments
                .and_then(|a| a["description"].as_str())
                .unwrap_or("subtask");
            format!("Task: {desc}")
        },
        _ => format!("{action}..."),
    }
}
