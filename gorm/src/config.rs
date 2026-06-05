//! User config at `$XDG_CONFIG_HOME/gorm/config.toml`. Palette/layout/loader
//! come from `levelup_tui::config`; only gorm's fields live here. All optional;
//! bad TOML warns and falls back to defaults.

use serde::Deserialize;

pub use levelup_tui::config::{ColorName, Layout};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub limit: usize,
    pub layout: Layout,
    pub preview: bool,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limit: 1000,
            layout: Layout::Top,
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
    levelup_tui::config::load_or_default(levelup_core::xdg::config_file("gorm"))
}
