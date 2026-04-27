//! Slash command model and palette filtering for the TUI.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub needs_argument: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    New,
    Resume(String),
    Sessions,
    Abort,
    Quit,
    Help,
}

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
        name: "abort",
        usage: "/abort",
        description: "Abort the active turn",
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

pub fn parse(input: &str) -> Option<SlashCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }

    let (cmd, arg) = match input[1..].split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (&input[1..], ""),
    };

    match cmd {
        "new" => Some(SlashCommand::New),
        "resume" | "r" => Some(SlashCommand::Resume(arg.to_string())),
        "sessions" | "ls" => Some(SlashCommand::Sessions),
        "abort" | "stop" => Some(SlashCommand::Abort),
        "quit" | "q" | "exit" => Some(SlashCommand::Quit),
        "help" | "?" => Some(SlashCommand::Help),
        _ => None,
    }
}

pub fn command_line_for(spec: SlashCommandSpec) -> String {
    if spec.needs_argument {
        let command = spec.usage.split_whitespace().next().unwrap_or(spec.usage);
        format!("{command} ")
    } else {
        spec.usage.to_string()
    }
}

pub fn help_text() -> String {
    COMMANDS
        .iter()
        .map(|command| format!("{:<16} {}", command.usage, command.description))
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn rejects_commands_without_server_handlers() {
        assert_eq!(parse("/model gpt-5"), None);
        assert_eq!(parse("/mode plan"), None);
        assert_eq!(parse("/compact"), None);
    }
}
