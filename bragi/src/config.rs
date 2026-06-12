//! User config at `$XDG_CONFIG_HOME/bragi/config.toml`. Palette/loader come
//! from `levelup_tui::config`; only bragi's fields live here. All optional;
//! bad TOML warns and falls back to defaults.

use serde::Deserialize;

pub use levelup_tui::config::ColorName;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Show the detail strip for the selected row on startup (Ctrl-V toggles).
    pub preview: bool,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
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
    pub match_fg: ColorName,
    pub prompt_fg: ColorName,
    pub status_fg: ColorName,
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

pub fn load_or_default() -> Config {
    levelup_tui::config::load_or_default(levelup_core::xdg::config_file("bragi"))
}
