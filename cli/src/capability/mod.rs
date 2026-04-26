use std::{env, io::IsTerminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Ansi16,
    TrueColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphMode {
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub color: ColorLevel,
    pub glyphs: GlyphMode,
    pub alt_screen: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
}

impl TerminalCapabilities {
    pub fn detect() -> Self {
        let term = env::var("TERM").unwrap_or_default().to_lowercase();
        let color_term = env::var("COLORTERM").unwrap_or_default().to_lowercase();
        let term_program = env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
        let interactive_term = std::io::stdout().is_terminal();
        let no_color = env::var_os("NO_COLOR").is_some();
        let ascii_only = env_flag("ASTRCODE_ASCII_ONLY");
        let disable_alt_screen = env_flag("ASTRCODE_NO_ALT_SCREEN");
        let disable_mouse = env_flag("ASTRCODE_NO_MOUSE");
        let disable_bracketed_paste = env_flag("ASTRCODE_NO_BRACKETED_PASTE");

        let color = if no_color {
            ColorLevel::None
        } else if color_term.contains("truecolor")
            || color_term.contains("24bit")
            || term_program == "wezterm"
            || env::var_os("WT_SESSION").is_some()
        {
            ColorLevel::TrueColor
        } else if interactive_term {
            ColorLevel::Ansi16
        } else if term.is_empty() || term == "dumb" {
            ColorLevel::None
        } else {
            ColorLevel::Ansi16
        };

        let glyphs = if ascii_only || term == "dumb" || !interactive_term {
            GlyphMode::Ascii
        } else {
            GlyphMode::Unicode
        };

        Self {
            color,
            glyphs,
            alt_screen: interactive_term && !disable_alt_screen,
            mouse: interactive_term && !disable_mouse,
            bracketed_paste: interactive_term && !disable_bracketed_paste,
        }
    }

    pub fn ascii_only(self) -> bool {
        matches!(self.glyphs, GlyphMode::Ascii)
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_only_tracks_glyph_mode() {
        assert!(
            TerminalCapabilities {
                color: ColorLevel::None,
                glyphs: GlyphMode::Ascii,
                alt_screen: false,
                mouse: false,
                bracketed_paste: false,
            }
            .ascii_only()
        );
        assert!(
            !TerminalCapabilities {
                color: ColorLevel::Ansi16,
                glyphs: GlyphMode::Unicode,
                alt_screen: true,
                mouse: true,
                bracketed_paste: true,
            }
            .ascii_only()
        );
    }
}
