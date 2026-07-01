//! 斜杠命令文本解析。

use super::HandlerError;

/// 解析后的斜杠命令。
pub(in crate::handler) struct ParsedSlashCommand {
    pub name: String,
    pub arguments: String,
}

impl ParsedSlashCommand {
    pub(in crate::handler) fn has_name(&self) -> bool {
        !self.name.trim().trim_start_matches('/').is_empty()
    }
}

/// 解析斜杠命令，如 "/compact arg1 arg2"。
/// 返回 None 表示不是斜杠命令。
pub(in crate::handler) fn parse_slash_command(text: &str) -> Option<ParsedSlashCommand> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?.trim();
    if body.is_empty() {
        return Some(ParsedSlashCommand {
            name: String::new(),
            arguments: String::new(),
        });
    }

    // 分割命令名和参数
    let (name, arguments) = body
        .split_once(char::is_whitespace)
        .map(|(name, arguments)| (name, arguments.trim()))
        .unwrap_or((body, ""));

    Some(ParsedSlashCommand {
        name: name.to_ascii_lowercase(),
        arguments: arguments.to_string(),
    })
}

/// 将 HandlerError 映射为错误码。
pub(in crate::handler) fn command_error_code(error: &HandlerError) -> i32 {
    match error {
        HandlerError::UnknownCommand(_) => 40402,
        _ => -32603,
    }
}
