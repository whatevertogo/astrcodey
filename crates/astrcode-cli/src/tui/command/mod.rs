//! Slash command parsing and palette data.
pub mod slash;
pub use slash::{
    SlashCommand, SlashCommandSpec, builtin_commands, command_line_for, filtered, parse,
};
