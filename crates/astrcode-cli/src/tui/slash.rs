//! 斜杠命令模型与面板过滤。
//!
//! 定义了 TUI 中支持的斜杠命令（如 /new、/resume、/quit 等），
//! 提供命令规范、输入解析、面板过滤和帮助文本生成等功能。

/// 斜杠命令的静态规范描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    /// 命令名称（不含斜杠前缀）
    pub name: &'static str,
    /// 用法示例（含斜杠前缀和参数占位符）
    pub usage: &'static str,
    /// 命令描述
    pub description: &'static str,
    /// 是否需要额外参数
    pub needs_argument: bool,
}

/// 已解析的斜杠命令枚举。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// 创建新会话
    New,
    /// 恢复指定 ID 的会话
    Resume(String),
    /// 列出所有已知会话
    Sessions,
    /// 退出 astrcode
    Quit,
    /// 显示帮助信息
    Help,
}

/// 所有支持的斜杠命令规范列表。
const COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "new",
        usage: "/new",
        description: "Create a fresh session",
        needs_argument: false,
    },
    SlashCommandSpec {
        name: "resume",
        usage: "/resume <id>",
        description: "Resume a previous session",
        needs_argument: true,
    },
    SlashCommandSpec {
        name: "sessions",
        usage: "/sessions",
        description: "List known sessions",
        needs_argument: false,
    },
    SlashCommandSpec {
        name: "help",
        usage: "/help",
        description: "Show command help",
        needs_argument: false,
    },
    SlashCommandSpec {
        name: "quit",
        usage: "/quit",
        description: "Exit astrcode",
        needs_argument: false,
    },
];

/// 根据过滤字符串筛选匹配的斜杠命令。
///
/// 过滤逻辑：去掉前导 `/` 后按前缀匹配命令名称或用法。
/// 空过滤字符串返回全部命令。
pub fn filtered(filter: &str) -> Vec<SlashCommandSpec> {
    let filter = filter.trim_start_matches('/').trim();
    if filter.is_empty() {
        return COMMANDS.to_vec();
    }

    COMMANDS
        .iter()
        .copied()
        .filter(|command| {
            command.name.starts_with(filter)
                || command.usage.trim_start_matches('/').starts_with(filter)
        })
        .collect()
}

/// 尝试将输入字符串解析为斜杠命令。
///
/// 输入必须以 `/` 开头。支持命令别名（如 `/q` = `/quit`，`/ls` = `/sessions`）。
/// 返回 `None` 表示输入不是斜杠命令。
pub fn parse(input: &str) -> Option<SlashCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }

    // 分离命令名和参数
    let (cmd, arg) = match input[1..].split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (&input[1..], ""),
    };

    match cmd {
        "new" => Some(SlashCommand::New),
        "resume" | "r" => Some(SlashCommand::Resume(arg.to_string())),
        "sessions" | "ls" => Some(SlashCommand::Sessions),
        "quit" | "q" | "exit" => Some(SlashCommand::Quit),
        "help" | "?" => Some(SlashCommand::Help),
        _ => None,
    }
}

/// 根据命令规范生成输入框中应显示的命令行文本。
///
/// 需要参数的命令仅返回命令前缀加空格（等待用户输入参数），
/// 不需要参数的命令返回完整用法字符串。
pub fn command_line_for(spec: SlashCommandSpec) -> String {
    if spec.needs_argument {
        let command = spec.usage.split_whitespace().next().unwrap_or(spec.usage);
        format!("{command} ")
    } else {
        spec.usage.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_commands_by_prefix() {
        let commands = filtered("re");
        assert!(commands.iter().any(|command| command.name == "resume"));
    }

    #[test]
    fn parses_aliases() {
        assert_eq!(parse("/q"), Some(SlashCommand::Quit));
        assert_eq!(parse("/ls"), Some(SlashCommand::Sessions));
    }
}
