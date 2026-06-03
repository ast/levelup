//! CPU temperature from `/sys/class/hwmon`.
//!
//! Walking the hwmon tree is plain file I/O; the only parsing is millidegrees
//! → °C. We prefer a sensor whose `name` matches the config hint (e.g.
//! `coretemp`, `k10temp`), else the first hwmon exposing a `temp*_input`.

use std::path::{Path, PathBuf};

/// Convert a hwmon `temp*_input` reading (millidegrees C) to °C.
pub fn milli_to_c(milli: i64) -> f64 {
    milli as f64 / 1000.0
}

/// Read a CPU/package temperature in °C. `hint` (may be empty) is matched
/// against each hwmon's `name`.
pub fn read(hint: &str) -> Option<f64> {
    let entries: Vec<PathBuf> = std::fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();

    // First pass: honour the hint if given. Second pass: any sensor.
    if !hint.is_empty() {
        for dir in &entries {
            if name_matches(dir, hint)
                && let Some(t) = first_temp(dir)
            {
                return Some(t);
            }
        }
    }
    // Prefer well-known CPU sensors before falling back to anything.
    for known in ["coretemp", "k10temp", "zenpower", "cpu_thermal"] {
        for dir in &entries {
            if name_matches(dir, known)
                && let Some(t) = first_temp(dir)
            {
                return Some(t);
            }
        }
    }
    entries.iter().find_map(|d| first_temp(d))
}

fn name_matches(dir: &Path, want: &str) -> bool {
    std::fs::read_to_string(dir.join("name"))
        .map(|n| n.trim() == want)
        .unwrap_or(false)
}

/// Read the lowest-numbered `tempN_input` in a hwmon dir as °C.
fn first_temp(dir: &Path) -> Option<f64> {
    let mut inputs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("temp") && n.ends_with("_input"))
                .unwrap_or(false)
        })
        .collect();
    inputs.sort();
    for p in inputs {
        if let Ok(v) = std::fs::read_to_string(&p)
            && let Ok(milli) = v.trim().parse::<i64>()
        {
            return Some(milli_to_c(milli));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_millidegrees() {
        assert_eq!(milli_to_c(62000), 62.0);
        assert_eq!(milli_to_c(45500), 45.5);
    }
}
