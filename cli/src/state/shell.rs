use std::path::PathBuf;

use astrcode_client::{CurrentModelInfoDto, ModeSummaryDto, ModelOptionDto};

use crate::capability::TerminalCapabilities;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellState {
    pub connection_origin: String,
    pub working_dir: Option<PathBuf>,
    pub capabilities: TerminalCapabilities,
    pub current_model: Option<CurrentModelInfoDto>,
    pub model_options: Vec<ModelOptionDto>,
    pub available_modes: Vec<ModeSummaryDto>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            connection_origin: String::new(),
            working_dir: None,
            capabilities: TerminalCapabilities {
                color: crate::capability::ColorLevel::None,
                glyphs: crate::capability::GlyphMode::Ascii,
                alt_screen: false,
                mouse: false,
                bracketed_paste: false,
            },
            current_model: None,
            model_options: Vec::new(),
            available_modes: Vec::new(),
        }
    }
}

impl ShellState {
    pub fn new(
        connection_origin: String,
        working_dir: Option<PathBuf>,
        capabilities: TerminalCapabilities,
    ) -> Self {
        Self {
            connection_origin,
            working_dir,
            capabilities,
            current_model: None,
            model_options: Vec::new(),
            available_modes: Vec::new(),
        }
    }
}
