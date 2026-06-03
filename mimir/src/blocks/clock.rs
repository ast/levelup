//! The clock block: a leading local date plus Local + N labelled zones, e.g.
//! `Sun 02 Jun · LOC 14:32 · UTC 17:32 · NAT 14:32`. Left-click toggles
//! seconds.

use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use tracing::warn;

use super::{Block, Ctx, Role, Segment};
use crate::config::ClockConfig;
use crate::protocol::ClickEvent;

pub struct Clock {
    date_format: String,
    time_format: String,
    local_label: String,
    /// Parsed extra zones (label, tz). Unparseable tz names are dropped at
    /// construction with a warning.
    zones: Vec<(String, Tz)>,
    /// Expanded view: show seconds.
    seconds: bool,
}

impl Clock {
    pub fn new(cfg: &ClockConfig) -> Self {
        let zones = cfg
            .zones
            .iter()
            .filter_map(|z| match z.tz.parse::<Tz>() {
                Ok(tz) => Some((z.label.clone(), tz)),
                Err(_) => {
                    warn!(tz = %z.tz, "unknown timezone in clock config; skipping");
                    None
                }
            })
            .collect();
        Self {
            date_format: cfg.date_format.clone(),
            time_format: cfg.time_format.clone(),
            local_label: cfg.local_label.clone(),
            zones,
            seconds: false,
        }
    }

    fn time_format(&self) -> String {
        if self.seconds {
            format!("{}:%S", self.time_format)
        } else {
            self.time_format.clone()
        }
    }
}

impl Block for Clock {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn render(&mut self, _ctx: &Ctx) -> Vec<Segment> {
        let now = Local::now();
        let tf = self.time_format();
        let mut parts: Vec<String> = Vec::new();
        if !self.date_format.is_empty() {
            parts.push(now.format(&self.date_format).to_string());
        }
        parts.push(format!("{} {}", self.local_label, now.format(&tf)));
        parts.extend(zone_times(now.with_timezone(&Utc), &self.zones, &tf));
        vec![Segment::new("clock", parts.join(" · ")).with_role(Role::Time)]
    }

    fn on_click(&mut self, ev: &ClickEvent) -> bool {
        if ev.button == 1 {
            self.seconds = !self.seconds;
            true
        } else {
            false
        }
    }
}

/// Format `now` (in UTC) for each labelled zone as `LABEL HH:MM`. Pulled out
/// as a free function so the timezone conversion is unit-testable without
/// depending on the machine's local zone.
fn zone_times(now: DateTime<Utc>, zones: &[(String, Tz)], time_format: &str) -> Vec<String> {
    zones
        .iter()
        .map(|(label, tz)| {
            let t = now.with_timezone(tz);
            format!("{} {}", label, t.format(time_format))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn zones_convert_from_utc() {
        // 17:32 UTC → 14:32 in America/Fortaleza (UTC-3, no DST).
        let now = Utc.with_ymd_and_hms(2026, 6, 2, 17, 32, 0).unwrap();
        let zones = vec![
            ("UTC".to_string(), "UTC".parse::<Tz>().unwrap()),
            (
                "NAT".to_string(),
                "America/Fortaleza".parse::<Tz>().unwrap(),
            ),
        ];
        let out = zone_times(now, &zones, "%H:%M");
        assert_eq!(out, vec!["UTC 17:32", "NAT 14:32"]);
    }

    #[test]
    fn seconds_toggle_extends_format() {
        let cfg = ClockConfig::default();
        let mut clock = Clock::new(&cfg);
        assert_eq!(clock.time_format(), "%H:%M");
        assert!(clock.on_click(&ClickEvent {
            name: Some("clock".into()),
            instance: None,
            button: 1,
        }));
        assert_eq!(clock.time_format(), "%H:%M:%S");
    }
}
