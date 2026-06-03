//! CPU power block: scaling governor + current frequency, e.g.
//! `powersave 2.4GHz`.

use super::{Block, Ctx, Segment, icon};
use crate::sys::cpufreq;

#[derive(Default)]
pub struct Power;

impl Block for Power {
    fn name(&self) -> &'static str {
        "power"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let glyph = icon("\u{f04c5}", "PWR", ctx.cfg.icons); // md-speedometer
        match cpufreq::read() {
            Some(f) => vec![
                Segment::new(
                    "power",
                    format!("{} {:>6}", f.governor, cpufreq::fmt_khz(f.cur_khz)),
                )
                .with_icon(glyph),
            ],
            None => vec![],
        }
    }
}
