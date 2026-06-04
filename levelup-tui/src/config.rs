//! Shared config primitives: the named-ANSI colour palette, the layout enum,
//! and a generic loader. Each tool keeps its own `Config`/`Colors` struct
//! (fields differ — sort, dir_fg/file_fg, warn_fg, preview) but reuses these.

use std::path::PathBuf;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::warn;

/// Where the prompt sits: `"bottom"` (fzf-style) or `"top"`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Bottom,
    Top,
}

/// A subset of ANSI colour names. Hex / 24-bit can be added later without
/// breaking existing configs (palettes are not `deny_unknown_fields`).
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

/// Load a tool's config from `path` (usually `levelup_core::xdg::config_file`).
/// Missing file → defaults; bad TOML → warn and defaults. We never refuse to
/// open the picker over a config error.
pub fn load_or_default<T: Default + DeserializeOwned>(path: Option<PathBuf>) -> T {
    let Some(path) = path else {
        return T::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "config read failed; using defaults");
            return T::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "config parse failed; using defaults");
            T::default()
        }
    }
}
