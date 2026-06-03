//! Battery status from `/sys/class/power_supply/BAT*/uevent`.
//!
//! The `uevent` file is a flat list of `KEY=VALUE` lines — parsed here with
//! nom — from which we derive charge %, charging state, and a rough
//! time-to-empty when discharging.

use nom::{
    IResult, Parser,
    bytes::complete::is_not,
    character::complete::{char, not_line_ending},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Battery {
    /// Charge percentage, 0–100.
    pub capacity: u8,
    /// e.g. `"Charging"`, `"Discharging"`, `"Full"`, `"Not charging"`.
    pub status: String,
    /// Estimated minutes until empty while discharging, if derivable.
    pub time_to_empty_min: Option<u64>,
}

impl Battery {
    pub fn is_charging(&self) -> bool {
        self.status == "Charging" || self.status == "Full"
    }
}

/// Parse one `KEY=VALUE` line.
fn kv(input: &str) -> IResult<&str, (&str, &str)> {
    let (input, key) = is_not("=").parse(input)?;
    let (input, _) = char('=').parse(input)?;
    let (input, val) = not_line_ending.parse(input)?;
    Ok((input, (key, val)))
}

/// Parse a `uevent` file into its key/value pairs.
pub fn parse_uevent(input: &str) -> Vec<(String, String)> {
    input
        .lines()
        .filter_map(|l| kv(l).ok().map(|(_, (k, v))| (k.to_string(), v.to_string())))
        .collect()
}

/// Build a [`Battery`] from parsed uevent pairs. Time-to-empty uses
/// energy/power if present (Wh / W → h), else charge/current (Ah / A → h).
pub fn battery_from_uevent(pairs: &[(String, String)]) -> Option<Battery> {
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let num = |key: &str| get(key).and_then(|v| v.parse::<f64>().ok());

    let capacity = num("POWER_SUPPLY_CAPACITY")?.clamp(0.0, 100.0) as u8;
    let status = get("POWER_SUPPLY_STATUS").unwrap_or("Unknown").to_string();

    let time_to_empty_min = if status == "Discharging" {
        let hours = match (
            num("POWER_SUPPLY_ENERGY_NOW"),
            num("POWER_SUPPLY_POWER_NOW"),
        ) {
            (Some(e), Some(p)) if p > 0.0 => Some(e / p),
            _ => match (
                num("POWER_SUPPLY_CHARGE_NOW"),
                num("POWER_SUPPLY_CURRENT_NOW"),
            ) {
                (Some(c), Some(i)) if i > 0.0 => Some(c / i),
                _ => None,
            },
        };
        hours.map(|h| (h * 60.0) as u64)
    } else {
        None
    };

    Some(Battery {
        capacity,
        status,
        time_to_empty_min,
    })
}

/// Find the first `BAT*` power supply and read its battery state.
pub fn read() -> Option<Battery> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("BAT")
            && let Ok(raw) = std::fs::read_to_string(entry.path().join("uevent"))
            && let Some(b) = battery_from_uevent(&parse_uevent(&raw))
        {
            return Some(b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const UEVENT: &str = "\
POWER_SUPPLY_NAME=BAT0
POWER_SUPPLY_STATUS=Discharging
POWER_SUPPLY_CAPACITY=85
POWER_SUPPLY_ENERGY_NOW=30000000
POWER_SUPPLY_POWER_NOW=10000000
";

    #[test]
    fn parses_and_estimates_runtime() {
        let b = battery_from_uevent(&parse_uevent(UEVENT)).unwrap();
        assert_eq!(b.capacity, 85);
        assert_eq!(b.status, "Discharging");
        assert!(!b.is_charging());
        // 30/10 = 3h = 180 min.
        assert_eq!(b.time_to_empty_min, Some(180));
    }

    #[test]
    fn charging_has_no_time_to_empty() {
        let raw = "POWER_SUPPLY_STATUS=Charging\nPOWER_SUPPLY_CAPACITY=42\n";
        let b = battery_from_uevent(&parse_uevent(raw)).unwrap();
        assert_eq!(b.capacity, 42);
        assert!(b.is_charging());
        assert_eq!(b.time_to_empty_min, None);
    }

    #[test]
    fn missing_capacity_is_none() {
        assert!(battery_from_uevent(&parse_uevent("POWER_SUPPLY_STATUS=Full\n")).is_none());
    }
}
