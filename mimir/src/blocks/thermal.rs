//! Temperature block: `NN°C` from hwmon, coloured by threshold.

use super::{Block, Ctx, Level, Segment, icon};
use crate::config::ThermalConfig;
use crate::sys::hwmon;

pub struct Thermal {
    hint: String,
    warn: f64,
    critical: f64,
}

impl Thermal {
    pub fn new(cfg: &ThermalConfig) -> Self {
        Self {
            hint: cfg.hwmon_label.clone(),
            warn: cfg.warn,
            critical: cfg.critical,
        }
    }
}

impl Block for Thermal {
    fn name(&self) -> &'static str {
        "thermal"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let glyph = icon("\u{f050f}", "TEMP", ctx.cfg.icons); // md-thermometer
        match hwmon::read(&self.hint) {
            Some(c) => {
                let level = Level::from_high(c, self.warn, self.critical);
                vec![
                    Segment::new("thermal", format!("{c:>3.0}°C"))
                        .with_icon(glyph)
                        .with_level(level),
                ]
            }
            None => vec![Segment::new("thermal", "…").with_icon(glyph)],
        }
    }
}
