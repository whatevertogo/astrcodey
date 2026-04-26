//! Slash command model for the lightweight TUI.

#[derive(Debug, Clone, Copy)]
pub struct SlashCommandInfo {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub takes_arg: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    New,
    Resume(String),
    Model(String),
    Mode(String),
    Compact,
    Quit,
    Help,
}

const COMMANDS: &[SlashCommandInfo] = &[
    SlashCommandInfo {
        name: "new",
        usage: "/new",
        description: "Start a new session",
        takes_arg: false,
    },
    SlashCommandInfo {
        name: "resume",
        usage: "/resume <id>",
        description: "Switch to an existing session",
        takes_arg: true,
    },
    SlashCommandInfo {
        name: "model",
        usage: "/model <name>",
        description: "Switch model",
        takes_arg: true,
    },
    SlashCommandInfo {
        name: "mode",
        usage: "/mode <name>",
        description: "Switch mode",
        takes_arg: true,
    },
    SlashCommandInfo {
        name: "compact",
        usage: "/compact",
        description: "Compact the current session",
        takes_arg: false,
    },
    SlashCommandInfo {
        name: "quit",
        usage: "/quit",
        description: "Exit astrcode",
        takes_arg: false,
    },
    SlashCommandInfo {
        name: "help",
        usage: "/help",
        description: "Show slash command help",
        takes_arg: false,
    },
];

pub fn commands() -> &'static [SlashCommandInfo] {
    COMMANDS
}

pub fn filtered(query: &str) -> Vec<&'static SlashCommandInfo> {
    let query = query.trim().trim_start_matches('/');
    if query.is_empty() {
        return commands().iter().collect();
    }

    commands()
        .iter()
        .filter(|cmd| cmd.name.contains(query) || cmd.usage.contains(query))
        .collect()
}

pub fn parse(input: &str) -> Option<SlashCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }

    let (cmd, arg) = match input[1..].split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (&input[1..], ""),
    };

    match cmd {
        "new" => Some(SlashCommand::New),
        "resume" | "r" => Some(SlashCommand::Resume(arg.to_string())),
        "model" | "m" => Some(SlashCommand::Model(arg.to_string())),
        "mode" => Some(SlashCommand::Mode(arg.to_string())),
        "compact" => Some(SlashCommand::Compact),
        "quit" | "q" | "exit" => Some(SlashCommand::Quit),
        "help" | "?" => Some(SlashCommand::Help),
        _ => None,
    }
}

pub fn completion_text(command: &SlashCommandInfo) -> String {
    if command.takes_arg {
        format!("/{} ", command.name)
    } else {
        command.usage.to_string()
    }
}
