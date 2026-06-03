//! The bar's blocks. Each owns its sampling state (previous counters for
//! rate/usage deltas) plus an `expanded` toggle, and implements [`Block`].
//! `render.rs` turns the [`Segment`]s they emit into protocol blocks.

pub mod battery;
pub mod clock;
pub mod cpu;
pub mod disk;
pub mod load;
pub mod memory;
pub mod net;
pub mod power;
pub mod thermal;

use crate::config::{Config, Icons};
use crate::protocol::ClickEvent;

/// Severity of a value relative to its thresholds — drives colour + urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warn,
    Critical,
}

impl Level {
    /// Higher is worse (CPU, memory, disk, temperature).
    pub fn from_high(value: f64, warn: f64, critical: f64) -> Level {
        if value >= critical {
            Level::Critical
        } else if value >= warn {
            Level::Warn
        } else {
            Level::Normal
        }
    }

    /// Lower is worse (battery charge).
    pub fn from_low(value: f64, warn: f64, critical: f64) -> Level {
        if value <= critical {
            Level::Critical
        } else if value <= warn {
            Level::Warn
        } else {
            Level::Normal
        }
    }
}

/// The display role of a segment's value, mapped to a cockpit colour:
/// `Data` = live system value (green / amber / red by [`Level`]); `Time` =
/// the clock (white, no alerting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Data,
    Time,
}

/// A rendered piece of the bar, before protocol serialisation. The `icon`
/// (cyan "label" in the cockpit palette) is kept separate from `text` (the
/// value) so each can be coloured independently.
#[derive(Debug, Clone)]
pub struct Segment {
    pub name: String,
    pub instance: Option<String>,
    /// Optional leading icon, coloured as a label (cyan).
    pub icon: Option<String>,
    pub text: String,
    pub short: Option<String>,
    pub level: Level,
    pub role: Role,
}

impl Segment {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instance: None,
            icon: None,
            text: text.into(),
            short: None,
            level: Level::Normal,
            role: Role::Data,
        }
    }
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }
}

/// Per-tick context handed to every block's `render`.
pub struct Ctx<'a> {
    pub cfg: &'a Config,
    /// Seconds since the previous **tick**, for rate calculations. `0.0` on
    /// the first tick (blocks then show a `…` placeholder).
    pub dt_secs: f64,
    /// `true` on a timer tick (blocks advance their delta baseline); `false`
    /// on a click-driven re-render (blocks reuse the last tick's baseline so a
    /// click can't produce a spurious throughput spike).
    pub advance: bool,
}

pub trait Block {
    /// Stable name, echoed back in click events.
    fn name(&self) -> &'static str;
    /// Sample fresh data and render. May emit several segments (e.g. one per
    /// disk mount); an empty vec means "show nothing" (e.g. no battery).
    fn render(&mut self, ctx: &Ctx) -> Vec<Segment>;
    /// React to a click; return `true` if it changed state worth re-rendering.
    /// Default: ignore (e.g. the clock overrides this to toggle seconds).
    fn on_click(&mut self, _ev: &ClickEvent) -> bool {
        false
    }
}

/// Build the enabled blocks in config order. Unknown names are skipped (warn).
pub fn build(cfg: &Config) -> Vec<Box<dyn Block>> {
    cfg.order
        .iter()
        .filter_map(|name| make(name, cfg))
        .collect()
}

fn make(name: &str, cfg: &Config) -> Option<Box<dyn Block>> {
    Some(match name {
        "clock" => Box::new(clock::Clock::new(&cfg.clock)),
        "cpu" => Box::new(cpu::Cpu::default()),
        "load" => Box::new(load::Load::new()),
        "memory" => Box::new(memory::Memory::default()),
        "disk" => Box::new(disk::Disk::new(&cfg.disk)),
        "net" => Box::new(net::Net::new(&cfg.net)),
        "thermal" => Box::new(thermal::Thermal::new(&cfg.thermal)),
        "battery" => Box::new(battery::BatteryBlock::default()),
        "power" => Box::new(power::Power),
        other => {
            tracing::warn!(block = other, "unknown block in `order`; skipping");
            return None;
        }
    })
}

/// Pick the glyph or text label for a block per the icon mode.
pub fn icon(nerd: &'static str, text: &'static str, icons: Icons) -> &'static str {
    match icons {
        Icons::Nerdfont => nerd,
        Icons::Text => text,
    }
}
