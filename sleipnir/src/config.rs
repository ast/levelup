//! User config at `$XDG_CONFIG_HOME/sleipnir/config.toml`. The colour palette,
//! layout enum and loader come from `levelup_tui::config`; only the
//! sleipnir-specific fields (dir_fg/file_fg, preview) live here.

use serde::Deserialize;

pub use levelup_tui::config::{ColorName, Layout};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum rows shown in the picker.
    pub limit: usize,
    /// `"bottom"` (fzf-style) or `"top"`.
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub selection_fg: ColorName,
    pub selection_bg: ColorName,
    /// `‹match›` highlights inside a row.
    pub match_fg: ColorName,
    /// Directory rows (Norman signifier: dirs read distinct from files).
    pub dir_fg: ColorName,
    /// File rows.
    pub file_fg: ColorName,
    pub prompt_fg: ColorName,
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

pub fn load_or_default() -> Config {
    levelup_tui::config::load_or_default(levelup_core::xdg::config_file("sleipnir"))
}
