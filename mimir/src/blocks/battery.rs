//! Battery block: `NN%` with a charge/charging glyph, coloured when low.
//! Emits nothing on machines without a battery. Expanded shows time-to-empty.

use super::{Block, Ctx, Level, Segment};
use crate::config::Icons;
use crate::protocol::ClickEvent;
use crate::sys::power_supply::{self, Battery};

#[derive(Default)]
pub struct BatteryBlock {
    expanded: bool,
}

impl Block for BatteryBlock {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let Some(bat) = power_supply::read() else {
            return vec![]; // no battery → no block
        };
        let glyph = battery_glyph(&bat, ctx.cfg.icons);
        let level = Level::from_low(
            bat.capacity as f64,
            ctx.cfg.battery.warn,
            ctx.cfg.battery.critical,
        );
        let mut text = format!("{glyph} {:>3}%", bat.capacity);
        if self.expanded
            && let Some(mins) = bat.time_to_empty_min
        {
            text.push_str(&format!(" {}h{:02}m", mins / 60, mins % 60));
        }
        vec![Segment::new("battery", text).with_level(level)]
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

/// Material battery glyph: a bolt-fill while charging, else a level by capacity.
fn battery_glyph(bat: &Battery, icons: Icons) -> &'static str {
    if matches!(icons, Icons::Text) {
        return if bat.is_charging() { "BAT+" } else { "BAT" };
    }
    if bat.is_charging() {
        return "\u{f0084}"; // md-battery-charging
    }
    match bat.capacity {
        0..=15 => "\u{f007a}",  // md-battery-10
        16..=45 => "\u{f007c}", // md-battery-30
        46..=70 => "\u{f007e}", // md-battery-50
        71..=90 => "\u{f0080}", // md-battery-70
        _ => "\u{f0079}",       // md-battery (full)
    }
}
