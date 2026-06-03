//! Network block: per-interface `iface ↓rx ↑tx`, with wifi signal appended for
//! wireless interfaces. Throughput is a delta over the global tick. The icon
//! reflects the medium (wifi vs wired). Left-clicking an interface toggles its
//! ↓/↑ rates for the interface's IP address(es) (sticky, per interface).

use std::collections::{HashMap, HashSet};

use super::{Block, Ctx, Segment, icon};
use crate::config::NetConfig;
use crate::human_rate;
use crate::protocol::ClickEvent;
use crate::sys::ifaddr;
use crate::sys::netdev::{self, IfCounters};

pub struct Net {
    /// Configured interfaces; empty = auto-detect each tick.
    configured: Vec<String>,
    /// Previous byte counters per interface, for rate calculation.
    prev: HashMap<String, IfCounters>,
    /// Interfaces currently showing their IP instead of throughput.
    show_ip: HashSet<String>,
}

impl Net {
    pub fn new(cfg: &NetConfig) -> Self {
        Self {
            configured: cfg.interfaces.clone(),
            prev: HashMap::new(),
            show_ip: HashSet::new(),
        }
    }
}

impl Block for Net {
    fn name(&self) -> &'static str {
        "net"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let counters = netdev::read().unwrap_or_default();
        let wireless = netdev::read_wireless().unwrap_or_default();

        // Which interfaces to show.
        let ifaces: Vec<String> = if !self.configured.is_empty() {
            self.configured.clone()
        } else {
            netdev::pick_default(&counters).into_iter().collect()
        };

        let segments = ifaces
            .iter()
            .map(|iface| {
                let now = counters
                    .iter()
                    .find(|(n, _)| n == iface)
                    .map(|(_, c)| *c)
                    .unwrap_or_default();
                // Pick the icon from the interface's medium: wifi if it shows
                // up in /proc/net/wireless, otherwise wired.
                let wifi = wireless.iter().find(|w| &w.iface == iface);
                let glyph = if wifi.is_some() {
                    icon("\u{f05a9}", "wifi", ctx.cfg.icons) // md-wifi
                } else {
                    icon("\u{f0e8b}", "lan", ctx.cfg.icons) // md-lan
                };

                // Body: the IP address(es) if toggled, else the ↓/↑ rates.
                let body = if self.show_ip.contains(iface) {
                    ip_body(iface)
                } else {
                    let (rx_s, tx_s) = match self.prev.get(iface) {
                        Some(prev) if ctx.dt_secs > 0.0 => {
                            let (rx, tx) = now.rates(prev, ctx.dt_secs);
                            (human_rate(rx), human_rate(tx))
                        }
                        _ => ("…".to_string(), "…".to_string()),
                    };
                    format!("↓{rx_s:>5} ↑{tx_s:>5}")
                };

                let mut text = format!("{iface} {body}");
                if let Some(w) = wifi {
                    // Link quality is reported out of ~70.
                    let pct = (w.link / 70.0 * 100.0).clamp(0.0, 100.0);
                    text.push_str(&format!(" {pct:>3.0}%"));
                }
                Segment::new("net", text)
                    .with_icon(glyph)
                    .with_instance(iface.clone())
            })
            .collect();

        // Refresh the previous-counter baseline only on a real tick.
        if ctx.advance {
            self.prev = counters.into_iter().collect();
        }
        segments
    }

    fn on_click(&mut self, ev: &ClickEvent) -> bool {
        // Left-click toggles the IP view for the clicked interface.
        if ev.button == 1
            && let Some(iface) = &ev.instance
        {
            if !self.show_ip.remove(iface) {
                self.show_ip.insert(iface.clone());
            }
            true
        } else {
            false
        }
    }
}

/// Build the IP portion: IPv4 and a (preferably global) IPv6, space-separated.
/// Falls back to a hint when the interface has no address (e.g. it's down).
fn ip_body(iface: &str) -> String {
    let (v4, v6) = ifaddr::addrs(iface);
    let parts: Vec<String> = [v4, v6].into_iter().flatten().collect();
    if parts.is_empty() {
        "no ip".to_string()
    } else {
        parts.join("  ")
    }
}
