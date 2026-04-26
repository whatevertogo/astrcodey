use ratatui::style::{Color, Modifier, Style};

use crate::{
    capability::{ColorLevel, TerminalCapabilities},
    state::{WrappedLineStyle, WrappedSpanStyle},
};

pub trait ThemePalette {
    fn line_style(&self, style: WrappedLineStyle) -> Style;
    fn span_style(&self, style: WrappedSpanStyle) -> Style;
    fn glyph(&self, unicode: &'static str, ascii: &'static str) -> &'static str;
    fn divider(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy)]
pub struct CodexTheme {
    capabilities: TerminalCapabilities,
}

impl CodexTheme {
    pub fn new(capabilities: TerminalCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn menu_block_style(&self) -> Style {
        Style::default().fg(self.text_primary())
    }

    fn surface_alt(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(56, 52, 48),
            ColorLevel::Ansi16 => Color::DarkGray,
            ColorLevel::None => Color::Reset,
        }
    }

    fn accent(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(224, 128, 82),
            _ => Color::Yellow,
        }
    }

    fn accent_soft(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(196, 124, 88),
            _ => Color::Yellow,
        }
    }

    fn thinking(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(241, 151, 104),
            _ => Color::Yellow,
        }
    }

    fn text_primary(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(237, 229, 219),
            _ => Color::White,
        }
    }

    fn text_secondary(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(196, 186, 173),
            _ => Color::Gray,
        }
    }

    fn text_muted(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(136, 126, 114),
            _ => Color::DarkGray,
        }
    }

    fn error(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(227, 111, 111),
            _ => Color::Red,
        }
    }

    fn selection(&self) -> Color {
        match self.capabilities.color {
            ColorLevel::TrueColor => Color::Rgb(70, 65, 60),
            ColorLevel::Ansi16 => Color::DarkGray,
            ColorLevel::None => Color::Reset,
        }
    }
}

impl ThemePalette for CodexTheme {
    fn line_style(&self, style: WrappedLineStyle) -> Style {
        let base = Style::default();
        if matches!(self.capabilities.color, ColorLevel::None) {
            return match style {
                WrappedLineStyle::Plain
                | WrappedLineStyle::ThinkingBody
                | WrappedLineStyle::ToolBody
                | WrappedLineStyle::Notice
                | WrappedLineStyle::PaletteItem => base,
                WrappedLineStyle::Selection
                | WrappedLineStyle::PromptEcho
                | WrappedLineStyle::ToolLabel
                | WrappedLineStyle::ErrorText
                | WrappedLineStyle::PaletteSelected => base.add_modifier(Modifier::BOLD),
                WrappedLineStyle::ThinkingLabel => {
                    base.add_modifier(Modifier::BOLD | Modifier::ITALIC)
                },
                WrappedLineStyle::Muted | WrappedLineStyle::ThinkingPreview => {
                    base.add_modifier(Modifier::DIM)
                },
            };
        }

        match style {
            WrappedLineStyle::Plain => base.fg(self.text_primary()),
            WrappedLineStyle::Muted | WrappedLineStyle::ThinkingPreview => {
                base.fg(self.text_muted())
            },
            WrappedLineStyle::Selection => base
                .fg(self.text_primary())
                .bg(self.selection())
                .add_modifier(Modifier::BOLD),
            WrappedLineStyle::PromptEcho => base
                .fg(self.text_primary())
                .bg(self.surface_alt())
                .add_modifier(Modifier::BOLD),
            WrappedLineStyle::ThinkingLabel => base
                .fg(self.thinking())
                .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            WrappedLineStyle::ThinkingBody => base.fg(self.text_secondary()),
            WrappedLineStyle::ToolLabel => base.fg(self.accent_soft()).add_modifier(Modifier::BOLD),
            WrappedLineStyle::ToolBody => base.fg(self.text_secondary()),
            WrappedLineStyle::Notice => base.fg(self.text_secondary()),
            WrappedLineStyle::ErrorText => base.fg(self.error()).add_modifier(Modifier::BOLD),
            WrappedLineStyle::PaletteItem => base.fg(self.text_secondary()),
            WrappedLineStyle::PaletteSelected => {
                base.fg(self.accent()).add_modifier(Modifier::BOLD)
            },
        }
    }

    fn glyph(&self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.capabilities.ascii_only() {
            ascii
        } else {
            unicode
        }
    }

    fn divider(&self) -> &'static str {
        self.glyph("─", "-")
    }

    fn span_style(&self, style: WrappedSpanStyle) -> Style {
        let base = Style::default();
        if matches!(self.capabilities.color, ColorLevel::None) {
            return match style {
                WrappedSpanStyle::Strong
                | WrappedSpanStyle::Heading
                | WrappedSpanStyle::TableHeader => base.add_modifier(Modifier::BOLD),
                WrappedSpanStyle::Emphasis => base.add_modifier(Modifier::ITALIC),
                WrappedSpanStyle::Link => base.add_modifier(Modifier::UNDERLINED),
                WrappedSpanStyle::InlineCode
                | WrappedSpanStyle::CodeFence
                | WrappedSpanStyle::CodeText
                | WrappedSpanStyle::TextArt
                | WrappedSpanStyle::TableBorder
                | WrappedSpanStyle::ListMarker
                | WrappedSpanStyle::QuoteMarker
                | WrappedSpanStyle::HeadingRule => base.add_modifier(Modifier::DIM),
            };
        }

        match style {
            WrappedSpanStyle::Strong => base.add_modifier(Modifier::BOLD),
            WrappedSpanStyle::Emphasis => base.add_modifier(Modifier::ITALIC),
            WrappedSpanStyle::Heading => base.fg(self.accent()).add_modifier(Modifier::BOLD),
            WrappedSpanStyle::HeadingRule => base.fg(self.text_muted()),
            WrappedSpanStyle::TableBorder => base.fg(self.text_muted()),
            WrappedSpanStyle::TableHeader => {
                base.fg(self.accent_soft()).add_modifier(Modifier::BOLD)
            },
            WrappedSpanStyle::InlineCode => base.fg(self.accent_soft()),
            WrappedSpanStyle::Link => base.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
            WrappedSpanStyle::ListMarker | WrappedSpanStyle::QuoteMarker => {
                base.fg(self.accent_soft()).add_modifier(Modifier::BOLD)
            },
            WrappedSpanStyle::CodeFence => base.fg(self.text_muted()).add_modifier(Modifier::DIM),
            WrappedSpanStyle::CodeText => base.fg(self.text_primary()),
            WrappedSpanStyle::TextArt => base.fg(self.text_primary()),
        }
    }
}
