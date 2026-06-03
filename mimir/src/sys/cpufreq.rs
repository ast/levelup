//! CPU scaling governor and current frequency from
//! `/sys/devices/system/cpu/cpu0/cpufreq/`. These are trivial single-value
//! files, so no nom — just `trim().parse()`.

use std::path::Path;

const BASE: &str = "/sys/devices/system/cpu/cpu0/cpufreq";

#[derive(Debug, Clone, PartialEq)]
pub struct CpuFreq {
    pub governor: String,
    /// Current frequency in kHz.
    pub cur_khz: u64,
}

/// Format a kHz frequency compactly: `2.4GHz` / `850MHz`.
pub fn fmt_khz(khz: u64) -> String {
    if khz >= 1_000_000 {
        format!("{:.1}GHz", khz as f64 / 1_000_000.0)
    } else {
        format!("{}MHz", khz / 1000)
    }
}

pub fn read() -> Option<CpuFreq> {
    let governor = std::fs::read_to_string(Path::new(BASE).join("scaling_governor"))
        .ok()?
        .trim()
        .to_string();
    let cur_khz = std::fs::read_to_string(Path::new(BASE).join("scaling_cur_freq"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Some(CpuFreq { governor, cur_khz })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_frequency() {
        assert_eq!(fmt_khz(2_400_000), "2.4GHz");
        assert_eq!(fmt_khz(850_000), "850MHz");
    }
}
