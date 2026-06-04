//! User config at `$XDG_CONFIG_HOME/hugin/config.toml`.
//!
//! Everything is optional — a missing file or missing keys just fall back to
//! defaults. Bad TOML or bad values warn to stderr and the defaults are used
//! (we never refuse to open the picker over a config error — `hugin search
//! -i` should always work). The palette/layout enums and the loader live in
//! `levelup_tui::config`; the one extra knob over munin is `preview`, which
//! toggles the right-side preview pane.

use serde::Deserialize;

use crate::proto::SearchSort;

pub use levelup_tui::config::{ColorName, Layout};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Initial sort mode for the picker. `"relevance"` or `"recent"`.
    pub sort: SearchSort,
    /// Maximum rows fetched per keystroke.
    pub limit: usize,
    /// Where to put the prompt — `"bottom"` (fzf-style) or `"top"`.
    pub layout: Layout,
    /// Show the right-side preview pane with the selected entry's content.
    pub preview: bool,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sort: SearchSort::Relevance,
            limit: 200,
            layout: Layout::Bottom,
            preview: true,
            colors: Colors::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    /// Foreground colour for the highlighted row.
    pub selection_fg: ColorName,
    /// Background colour for the highlighted row.
    pub selection_bg: ColorName,
    /// Colour for `‹match›` highlights inside the snippet.
    pub match_fg: ColorName,
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
            prompt_fg: ColorName::Green,
            status_fg: ColorName::Gray,
        }
    }
}

/// Load config from `$XDG_CONFIG_HOME/hugin/config.toml`. Missing file →
/// defaults. Bad TOML or unknown keys → log a warning and return defaults; we
/// never crash the picker over a config error.
pub fn load_or_default() -> Config {
    levelup_tui::config::load_or_default(levelup_core::xdg::config_file("hugin"))
}
