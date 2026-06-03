//! Memory block: `used/total`. Expanded appends swap usage.

use super::{Block, Ctx, Level, Segment, icon};
use crate::human_bytes;
use crate::protocol::ClickEvent;
use crate::sys::meminfo;

#[derive(Default)]
pub struct Memory {
    expanded: bool,
}

impl Block for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let glyph = icon("\u{f035b}", "MEM", ctx.cfg.icons); // md-memory
        let m = match meminfo::read() {
            Ok(m) => m,
            Err(_) => return vec![Segment::new("memory", "…").with_icon(glyph)],
        };
        let level = Level::from_high(m.used_pct(), ctx.cfg.memory.warn, ctx.cfg.memory.critical);
        // Pad `used` (total is fixed per machine) so the block width is stable.
        let mut text = format!("{:>5}/{}", human_bytes(m.used()), human_bytes(m.total));
        if self.expanded && m.swap_total > 0 {
            text.push_str(&format!(" swap {}", human_bytes(m.swap_used())));
        }
        vec![
            Segment::new("memory", text)
                .with_icon(glyph)
                .with_level(level),
        ]
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
