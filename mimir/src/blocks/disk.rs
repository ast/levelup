//! Disk block: one segment per configured mount, `label pct%`. Expanded
//! appends free space.

use std::path::PathBuf;

use super::{Block, Ctx, Level, Segment, icon};
use crate::config::{DiskConfig, MountConfig};
use crate::human_bytes;
use crate::protocol::ClickEvent;
use crate::sys::fs;

struct Mount {
    path: PathBuf,
    label: String,
    warn: f64,
    critical: f64,
}

pub struct Disk {
    mounts: Vec<Mount>,
    expanded: bool,
}

impl Disk {
    pub fn new(cfg: &DiskConfig) -> Self {
        let mounts = cfg
            .mounts
            .iter()
            .map(|m: &MountConfig| Mount {
                path: PathBuf::from(&m.path),
                label: if m.label.is_empty() {
                    m.path.clone()
                } else {
                    m.label.clone()
                },
                warn: m.warn,
                critical: m.critical,
            })
            .collect();
        Self {
            mounts,
            expanded: false,
        }
    }
}

impl Block for Disk {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn render(&mut self, ctx: &Ctx) -> Vec<Segment> {
        let glyph = icon("\u{f01bc}", "DISK", ctx.cfg.icons); // md-database
        self.mounts
            .iter()
            .map(|m| match fs::usage(&m.path) {
                Ok(u) => {
                    let level = Level::from_high(u.used_pct, m.warn, m.critical);
                    let mut text = format!("{} {:>3.0}%", m.label, u.used_pct);
                    if self.expanded {
                        text.push_str(&format!(" ({} free)", human_bytes(u.avail)));
                    }
                    Segment::new("disk", text)
                        .with_icon(glyph)
                        .with_level(level)
                        .with_instance(m.label.clone())
                }
                Err(_) => Segment::new("disk", format!("{} ?", m.label))
                    .with_icon(glyph)
                    .with_instance(m.label.clone()),
            })
            .collect()
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
