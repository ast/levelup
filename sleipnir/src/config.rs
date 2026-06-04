//! User config at `$XDG_CONFIG_HOME/sleipnir/config.toml`.
//!
//! Ported from munin's `config.rs` (copy-now, extract-later). Everything is
//! optional — a missing file or missing keys fall back to defaults. Bad TOML
//! warns to stderr and the defaults are used; we never refuse to open the
//! picker over a config error.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum rows shown in the picker. The frecency pool is bounded anyway;
    /// this caps how tall the list can get.
    pub limit: usize,
    /// Where to put the prompt — `"bottom"` (fzf-style) or `"top"`.
    pub layout: Layout,
    /// Show the right-side preview pane (dir listing / file head).
    pub preview: bool,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limit: 200,
            layout: Layout::Bottom,
            preview: true,
            colors: Colors::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Prompt at the bottom, results growing upward (fzf-style).
    Bottom,
    /// Prompt at the top, results growing downward.
    Top,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    /// Foreground colour for the highlighted row.
    pub selection_fg: ColorName,
    /// Background colour for the highlighted row.
    pub selection_bg: ColorName,
    /// Colour for `‹match›` highlights inside a row.
    pub match_fg: ColorName,
    /// Colour for directory rows (Norman signifier: dirs read distinct from
    /// files at a glance).
    pub dir_fg: ColorName,
    /// Colour for file rows.
    pub file_fg: ColorName,
    /// Colour for the prompt `›` glyph.
    pub prompt_fg: ColorName,
    /// Colour for the status line.
    pub status_fg: ColorName,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            selection_fg: ColorName::Black,
            selection_bg: ColorName::Cyan,
            match_fg: ColorName::Yellow,
            dir_fg: ColorName::Blue,
            file_fg: ColorName::White,
            prompt_fg: ColorName::Green,
            status_fg: ColorName::DarkGray,
        }
    }
}

/// A subset of ANSI colour names. Hex / 24-bit colours can be added later
/// without breaking existing configs (the palette is not `deny_unknown_fields`).
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

/// Default path: `$XDG_CONFIG_HOME/sleipnir/config.toml`, falling back to
/// `$HOME/.config/sleipnir/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sleipnir").join("config.toml"))
}

/// Load config from the default path. Missing file → defaults. Bad TOML or
/// unknown keys → warn and return defaults; we never crash the picker over a
/// config error.
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
