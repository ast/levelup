//! Low-level readers for `/proc` & `/sys`.
//!
//! Each module splits cleanly into a **pure parser** over the raw file
//! contents (`&str`) — written with `nom` for the multi-field records — and a
//! thin reader that does the file I/O. Tests drive the parsers with fixture
//! strings, so they need no special privileges or hardware.
//!
//! Trivial single-value sysfs files (a governor name, a battery capacity) skip
//! nom entirely: `read_to_string().trim().parse()` is clearer than a combinator.

pub mod cpufreq;
pub mod fs;
pub mod hwmon;
pub mod ifaddr;
pub mod loadavg;
pub mod meminfo;
pub mod netdev;
pub mod power_supply;
pub mod procstat;

use nom::{IResult, Parser, character::complete::digit1, combinator::map_res};

/// Shared nom atom: a base-10 `u64`. Used across the `/proc` parsers.
pub(crate) fn dec_u64(input: &str) -> IResult<&str, u64> {
    map_res(digit1, |s: &str| s.parse::<u64>()).parse(input)
}
