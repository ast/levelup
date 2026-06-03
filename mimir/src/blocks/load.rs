//! Load-average block: `1m 5m 15m`. Coloured by the 1-minute load relative to
//! the CPU count (warn at ≥ncpu, critical at ≥2×ncpu).

use super::{Block, Ctx, Level, Segment, icon};
use crate::sys::loadavg;

pub struct Load {
    ncpu: f64,
}

impl Load {
    pub fn new() -> Self {
        let ncpu = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;
        Self { ncpu }
    }
}

impl Default for Load {
    fn default() -> Self {
        Self::new()
    }
}

impl Block for Load {
    fn name(&self) -> &'static str {
        "load"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let glyph = icon("\u{f029a}", "LOAD", ctx.cfg.icons); // md-gauge
        let Some(l) = loadavg::read().ok().flatten() else {
            return vec![Segment::new("load", "…").with_icon(glyph)];
        };
        let level = Level::from_high(l.one, self.ncpu, self.ncpu * 2.0);
        let text = format!("{:>5.2} {:>5.2} {:>5.2}", l.one, l.five, l.fifteen);
        vec![
            Segment::new("load", text)
                .with_icon(glyph)
                .with_level(level),
        ]
    }
}
