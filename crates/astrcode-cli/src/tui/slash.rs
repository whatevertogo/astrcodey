//! Slash command model for the lightweight TUI.

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
