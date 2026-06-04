//! User config at `$XDG_CONFIG_HOME/mimir/config.toml`.
//!
//! Everything is optional — a missing file or missing keys fall back to
//! defaults. Bad TOML logs a warning and the defaults are used; mimir never
//! refuses to start over a config error (the bar should always come up).
//! Ported from `munin::config`.
//!
//! Forward-compat note: `deny_unknown_fields` is intentionally **not** set, so
//! a config written for a newer mimir (with keys this build doesn't know) still
//! parses instead of falling back to all-defaults.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::warn;

/// The documented default config, emitted verbatim by `mimir print-config`.
/// Kept as a string (rather than serialising `Config::default()`) because the
/// workspace `toml` dependency is parse-only, and a hand-written file can carry
/// explanatory comments.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global refresh tick in milliseconds.
    pub interval_ms: u64,
    /// `"pango"` enables markup (dynamic values are escaped); `"none"` is plain.
    pub markup: Markup,
    /// `"nerdfont"` uses glyph icons; `"text"` uses short ASCII labels.
    pub icons: Icons,
    /// Block render order (left→right). Names not present are skipped.
    pub order: Vec<String>,
    pub clock: ClockConfig,
    pub cpu: Threshold,
    pub memory: Threshold,
    pub disk: DiskConfig,
    pub net: NetConfig,
    pub thermal: ThermalConfig,
    pub battery: BatteryConfig,
    pub colors: Colors,
    /// Per-block click→command bindings.
    pub bindings: Vec<Binding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            markup: Markup::Pango,
            icons: Icons::Nerdfont,
            // `power` (cpu governor/freq) is implemented but off by default;
            // add it to `order` to re-enable.
            order: [
                "clock", "net", "disk", "memory", "load", "cpu", "thermal", "battery",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            clock: ClockConfig::default(),
            cpu: Threshold::pct(80, 95),
            memory: Threshold::pct(80, 95),
            disk: DiskConfig::default(),
            net: NetConfig::default(),
            thermal: ThermalConfig::default(),
            battery: BatteryConfig::default(),
            colors: Colors::default(),
            bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Markup {
    Pango,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Icons {
    Nerdfont,
    Text,
}

/// A warn/critical threshold pair. For most blocks "higher is worse" (CPU,
/// memory, disk, temperature); the battery block reads it as "lower is worse".
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Threshold {
    pub warn: f64,
    pub critical: f64,
}

impl Threshold {
    const fn pct(warn: u32, critical: u32) -> Self {
        Self {
            warn: warn as f64,
            critical: critical as f64,
        }
    }
}

impl Default for Threshold {
    fn default() -> Self {
        Self::pct(80, 95)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// Leading local date, `chrono` strftime (empty = no date).
    pub date_format: String,
    /// Time format applied to every zone, `chrono` strftime.
    pub time_format: String,
    /// Label for the implicit local zone.
    pub local_label: String,
    /// Extra labelled zones shown after local (IANA tz names).
    pub zones: Vec<ZoneConfig>,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            date_format: "%a %d %b".into(),
            time_format: "%H:%M".into(),
            local_label: "LOC".into(),
            zones: vec![
                ZoneConfig {
                    label: "UTC".into(),
                    tz: "UTC".into(),
                },
                ZoneConfig {
                    label: "NAT".into(),
                    tz: "America/Fortaleza".into(),
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneConfig {
    pub label: String,
    /// IANA timezone name, e.g. `"UTC"` or `"America/Fortaleza"`.
    pub tz: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiskConfig {
    pub mounts: Vec<MountConfig>,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            mounts: vec![MountConfig {
                path: "/".into(),
                label: String::new(),
                warn: 85.0,
                critical: 95.0,
            }],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MountConfig {
    pub path: String,
    /// Display label; empty → use the path.
    pub label: String,
    pub warn: f64,
    pub critical: f64,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            path: "/".into(),
            label: String::new(),
            warn: 85.0,
            critical: 95.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    /// Interfaces to show; empty = auto-detect (first non-loopback with traffic).
    pub interfaces: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThermalConfig {
    /// hwmon `name` to prefer (e.g. `"coretemp"`, `"k10temp"`); empty = first found.
    pub hwmon_label: String,
    /// Warn/critical in °C.
    pub warn: f64,
    pub critical: f64,
}

impl Default for ThermalConfig {
    fn default() -> Self {
        Self {
            hwmon_label: String::new(),
            warn: 75.0,
            critical: 90.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BatteryConfig {
    /// Warn/critical when capacity drops *below* these percentages.
    pub warn: f64,
    pub critical: f64,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            warn: 20.0,
            critical: 10.0,
        }
    }
}

/// Cockpit-style palette (Airbus EFIS conventions), as `#rrggbb`. An empty
/// string means "use swaybar's default colour". `label` paints the icons
/// (cyan, "fixed labels"); `data` the live values (green); `time` the clock
/// (white); `warn`/`critical` are the caution/warning overrides (amber/red).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub label: String,
    pub data: String,
    pub time: String,
    pub warn: String,
    pub critical: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            label: "#1ec8e6".into(),    // cyan — labels/units
            data: "#34d63a".into(),     // green — normal live values
            time: "#e8e8e8".into(),     // white — present time
            warn: "#ffb000".into(),     // amber — caution
            critical: "#ff3b30".into(), // red — warning
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Binding {
    /// Block `name` this binding applies to (e.g. `"clock"`).
    pub block: String,
    /// Mouse button: 1=left, 2=middle, 3=right, 4=scroll-up, 5=scroll-down.
    pub button: u8,
    /// Shell command to spawn (via `sh -c`), detached and fire-and-forget.
    pub command: String,
}

/// Default path: `$XDG_CONFIG_HOME/mimir/config.toml`, falling back to
/// `$HOME/.config/mimir/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    levelup_core::xdg::config_file("mimir")
}

/// Load config from `path` (or the default path when `None`). Missing file →
/// defaults. Bad TOML → warn and return defaults; never crash.
pub fn load_or_default(path: Option<&std::path::Path>) -> Config {
    let path = match path.map(PathBuf::from).or_else(default_config_path) {
        Some(p) => p,
        None => return Config::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.interval_ms, 1000);
        assert_eq!(c.markup, Markup::Pango);
        assert_eq!(c.clock.zones.len(), 2);
        assert_eq!(c.clock.zones[1].tz, "America/Fortaleza");
        assert_eq!(c.disk.mounts[0].path, "/");
    }

    #[test]
    fn partial_toml_merges_over_defaults() {
        // Only override one key; everything else must keep its default.
        let c: Config = toml::from_str("interval_ms = 2000\nicons = \"text\"\n").unwrap();
        assert_eq!(c.interval_ms, 2000);
        assert_eq!(c.icons, Icons::Text);
        assert_eq!(c.markup, Markup::Pango); // untouched
        assert_eq!(c.clock.local_label, "LOC"); // nested default preserved
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // Forward-compat: a key from a future mimir must not break parsing.
        let c: Config = toml::from_str("future_knob = true\ninterval_ms = 500\n").unwrap();
        assert_eq!(c.interval_ms, 500);
    }

    #[test]
    fn bundled_default_config_parses() {
        let c: Config = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(c.interval_ms, 1000);
    }
}
