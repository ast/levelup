//! CPU usage block. Aggregate % by default; expanded shows per-core %.

use super::{Block, Ctx, Level, Segment, icon};
use crate::protocol::ClickEvent;
use crate::sys::procstat::{self, CpuLine};

#[derive(Default)]
pub struct Cpu {
    /// All `cpu*` lines from the previous sample (index 0 = aggregate).
    prev: Vec<CpuLine>,
    expanded: bool,
}

impl Block for Cpu {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let now = procstat::read().unwrap_or_default();
        let glyph = icon("\u{f0ee0}", "CPU", ctx.cfg.icons); // md-cpu-64-bit

        let agg_pct = match (now.first(), self.prev.first()) {
            (Some(a), Some(p)) => a.busy_pct(p),
            _ => None,
        };
        let level = agg_pct
            .map(|p| Level::from_high(p, ctx.cfg.cpu.warn, ctx.cfg.cpu.critical))
            .unwrap_or(Level::Normal);

        let mut text = match agg_pct {
            Some(p) => format!("{p:>3.0}%"),
            None => "  …".to_string(),
        };

        if self.expanded {
            let cores: Vec<String> = now
                .iter()
                .filter_map(|line| {
                    let core = line.core?;
                    let prev = self.prev.iter().find(|p| p.core == Some(core))?;
                    Some(format!("c{core} {:.0}", line.busy_pct(prev)?))
                })
                .collect();
            if !cores.is_empty() {
                text.push_str(" [");
                text.push_str(&cores.join(" "));
                text.push(']');
            }
        }

        if ctx.advance {
            self.prev = now;
        }
        vec![Segment::new("cpu", text).with_icon(glyph).with_level(level)]
    }

    fn on_click(&mut self, ev: &ClickEvent) -> bool {
        if ev.button == 1 {
            self.expanded = !self.expanded;
            true
        } else {
            false
        }
    }
}
