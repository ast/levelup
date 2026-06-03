//! `/proc/stat` — aggregate and per-core CPU jiffy counters.
//!
//! CPU usage isn't a value you can read instantaneously: it's the change in
//! busy jiffies between two samples. Each block keeps the previous [`CpuLine`]s
//! and computes a percentage from the delta (see [`CpuLine::busy_pct`]).

use std::path::Path;

use nom::{
    IResult, Parser,
    bytes::complete::tag,
    character::complete::{space1, u32 as u32_p},
    combinator::opt,
    multi::separated_list1,
    sequence::preceded,
};

use super::dec_u64;

/// One `cpuN` (or aggregate `cpu`) line, reduced to the two numbers that
/// matter for a usage percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuLine {
    /// `None` for the aggregate `cpu` line, `Some(n)` for `cpuN`.
    pub core: Option<u32>,
    /// idle + iowait jiffies.
    pub idle: u64,
    /// sum of all jiffies (busy + idle).
    pub total: u64,
}

impl CpuLine {
    /// Busy percentage between an earlier sample (`prev`) and `self`. Returns
    /// `None` if the counters didn't advance or appear to have wrapped/reset.
    pub fn busy_pct(&self, prev: &CpuLine) -> Option<f64> {
        let dt = self.total.checked_sub(prev.total)?;
        let di = self.idle.checked_sub(prev.idle)?;
        if dt == 0 {
            return None;
        }
        let busy = dt.saturating_sub(di) as f64;
        Some((busy / dt as f64) * 100.0)
    }
}

/// Parse one `cpu`/`cpuN` line's content (the caller has confirmed it starts
/// with `cpu`). Field order per `proc(5)`: user nice system idle iowait irq
/// softirq steal guest guest_nice — idle-all = idle + iowait.
fn cpu_line(input: &str) -> IResult<&str, CpuLine> {
    let (input, core) = preceded(tag("cpu"), opt(u32_p)).parse(input)?;
    let (input, vals) = preceded(space1, separated_list1(space1, dec_u64)).parse(input)?;
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    let total: u64 = vals.iter().sum();
    Ok((input, CpuLine { core, idle, total }))
}

/// Parse `/proc/stat`, returning every `cpu*` line. Index 0 is the aggregate
/// (`core: None`); the rest are per-core in file order. Non-`cpu` lines (intr,
/// ctxt, btime, …) are ignored.
pub fn parse(input: &str) -> Vec<CpuLine> {
    input
        .lines()
        .filter(|l| l.starts_with("cpu"))
        .filter_map(|l| cpu_line(l).ok().map(|(_, c)| c))
        .collect()
}

/// Read and parse `/proc/stat`.
pub fn read() -> std::io::Result<Vec<CpuLine>> {
    Ok(parse(&std::fs::read_to_string(Path::new("/proc/stat"))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  100 0 50 800 50 0 0 0 0 0
cpu0 60 0 30 380 30 0 0 0 0 0
cpu1 40 0 20 420 20 0 0 0 0 0
intr 12345 0 0
ctxt 99999
";

    #[test]
    fn parses_aggregate_and_cores() {
        let lines = super::parse(SAMPLE);
        assert_eq!(lines.len(), 3);
        let agg = lines[0];
        assert_eq!(agg.core, None);
        assert_eq!(agg.idle, 800 + 50); // idle + iowait
        assert_eq!(agg.total, 1000);
        assert_eq!(lines[1].core, Some(0));
        assert_eq!(lines[2].core, Some(1));
    }

    #[test]
    fn busy_pct_from_delta() {
        let prev = CpuLine {
            core: None,
            idle: 850,
            total: 1000,
        };
        // +200 total, +50 idle → 150 busy of 200 = 75%.
        let now = CpuLine {
            core: None,
            idle: 900,
            total: 1200,
        };
        assert_eq!(now.busy_pct(&prev), Some(75.0));
    }

    #[test]
    fn busy_pct_guards_no_progress_and_wrap() {
        let a = CpuLine {
            core: None,
            idle: 10,
            total: 100,
        };
        assert_eq!(a.busy_pct(&a), None); // no progress
        let earlier = CpuLine {
            core: None,
            idle: 10,
            total: 200,
        };
        assert_eq!(a.busy_pct(&earlier), None); // total went backwards
    }
}
