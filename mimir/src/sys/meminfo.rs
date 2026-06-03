//! `/proc/meminfo` — RAM and swap totals.

use std::path::Path;

use nom::{
    IResult, Parser,
    bytes::complete::is_not,
    character::complete::{char, space0},
};

use super::dec_u64;

/// Memory figures, all in **bytes** (the file reports kB; we scale on parse).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
    pub fn used_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64 * 100.0
        }
    }
    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

/// Parse one `Key:   <num> kB` line into `(key, value_in_kB)`.
fn line(input: &str) -> IResult<&str, (&str, u64)> {
    let (input, key) = is_not(":").parse(input)?;
    let (input, _) = char(':').parse(input)?;
    let (input, _) = space0.parse(input)?;
    let (input, val) = dec_u64(input)?;
    Ok((input, (key, val)))
}

/// Parse `/proc/meminfo`, picking the four fields the memory block needs.
pub fn parse(input: &str) -> MemInfo {
    let mut m = MemInfo::default();
    for l in input.lines() {
        if let Ok((_, (key, kb))) = line(l) {
            let bytes = kb.saturating_mul(1024);
            match key {
                "MemTotal" => m.total = bytes,
                "MemAvailable" => m.available = bytes,
                "SwapTotal" => m.swap_total = bytes,
                "SwapFree" => m.swap_free = bytes,
                _ => {}
            }
        }
    }
    m
}

/// Read and parse `/proc/meminfo`.
pub fn read() -> std::io::Result<MemInfo> {
    Ok(parse(&std::fs::read_to_string(Path::new("/proc/meminfo"))?))
}

#[cfg(test)]
mod tests {
    const SAMPLE: &str = "\
MemTotal:       16000 kB
MemFree:         2000 kB
MemAvailable:    8000 kB
Buffers:          500 kB
SwapTotal:       4000 kB
SwapFree:        3000 kB
";

    #[test]
    fn parses_and_computes() {
        let m = super::parse(SAMPLE);
        assert_eq!(m.total, 16000 * 1024);
        assert_eq!(m.available, 8000 * 1024);
        assert_eq!(m.used(), 8000 * 1024); // 16000 - 8000
        assert_eq!(m.used_pct(), 50.0);
        assert_eq!(m.swap_used(), 1000 * 1024);
    }

    #[test]
    fn missing_fields_default_to_zero() {
        let m = super::parse("MemTotal: 100 kB\n");
        assert_eq!(m.total, 100 * 1024);
        assert_eq!(m.available, 0);
        assert_eq!(m.used_pct(), 100.0);
    }
}
