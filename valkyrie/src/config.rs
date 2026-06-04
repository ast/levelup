//! User config at `$XDG_CONFIG_HOME/valkyrie/config.toml`.
//!
//! Ported from sleipnir's `config.rs`. Everything optional; bad TOML warns and
//! falls back to defaults — we never refuse to open the picker over a config
//! error.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum rows shown in the picker.
    pub limit: usize,
    /// `"bottom"` (fzf-style) or `"top"`.
    pub layout: Layout,
    /// Show the right-side preview pane (process detail).
    pub preview: bool,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limit: 500,
            layout: Layout::Bottom,
            preview: true,
            colors: Colors::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Bottom,
    Top,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub selection_fg: ColorName,
    pub selection_bg: ColorName,
    /// `‹match›` highlights in the command.
    pub match_fg: ColorName,
    /// Prompt `›` glyph.
    pub prompt_fg: ColorName,
    /// Status line / column labels.
    pub status_fg: ColorName,
    /// Transient action messages and the hold-to-kill meter (e.g. "needs root").
    pub warn_fg: ColorName,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            selection_fg: ColorName::Black,
            selection_bg: ColorName::Cyan,
            match_fg: ColorName::Yellow,
            prompt_fg: ColorName::Green,
            status_fg: ColorName::DarkGray,
            warn_fg: ColorName::Red,
        }
    }
}

/// A subset of ANSI colour names. Hex / 24-bit can be added later without
/// breaking existing configs (the palette is not `deny_unknown_fields`).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorName {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
}

impl ColorName {
    pub fn to_ratatui(self) -> ratatui::style::Color {
        use ratatui::style::Color::*;
        match self {
            Self::Black => Black,
            Self::Red => Red,
            Self::Green => Green,
            Self::Yellow => Yellow,
            Self::Blue => Blue,
            Self::Magenta => Magenta,
            Self::Cyan => Cyan,
            Self::White => White,
            Self::Gray => Gray,
            Self::DarkGray => DarkGray,
            Self::LightRed => LightRed,
            Self::LightGreen => LightGreen,
            Self::LightYellow => LightYellow,
            Self::LightBlue => LightBlue,
            Self::LightMagenta => LightMagenta,
            Self::LightCyan => LightCyan,
        }
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("valkyrie").join("config.toml"))
}

pub fn load_or_default() -> Config {
    let Some(path) = default_config_path() else {
        return Config::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "config read failed; using defaults");
            return Config::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "config parse failed; using defaults");
            Config::default()
        }
    }
}
