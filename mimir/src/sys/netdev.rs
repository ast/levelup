//! `/proc/net/dev` (per-interface byte counters) and `/proc/net/wireless`
//! (link quality / signal level).
//!
//! Throughput, like CPU usage, is a delta: each net block keeps the previous
//! [`IfCounters`] and the instant it sampled them, and divides by the elapsed
//! time (see [`IfCounters::rates`]).

use std::path::Path;

use nom::{
    IResult, Parser,
    bytes::complete::is_not,
    character::complete::{char, space0, space1},
    multi::separated_list1,
};

use super::dec_u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl IfCounters {
    /// (rx, tx) bytes/sec since `prev`, sampled `dt_secs` ago. Counter
    /// resets (a value going backwards) read as 0 rather than a huge spike.
    pub fn rates(&self, prev: &IfCounters, dt_secs: f64) -> (f64, f64) {
        if dt_secs <= 0.0 {
            return (0.0, 0.0);
        }
        let rx = self.rx_bytes.saturating_sub(prev.rx_bytes) as f64 / dt_secs;
        let tx = self.tx_bytes.saturating_sub(prev.tx_bytes) as f64 / dt_secs;
        (rx, tx)
    }
}

/// Parse one `  iface: rx ... tx ...` data line. Per `proc(5)` the receive
/// columns come first (bytes is column 0) and transmit second (bytes is
/// column 8).
fn dev_line(input: &str) -> IResult<&str, (String, IfCounters)> {
    let (input, _) = space0(input)?;
    let (input, iface) = is_not(":")(input)?;
    let (input, _) = char(':')(input)?;
    let (input, _) = space0(input)?;
    let (input, vals) = separated_list1(space1, dec_u64).parse(input)?;
    let counters = IfCounters {
        rx_bytes: vals.first().copied().unwrap_or(0),
        tx_bytes: vals.get(8).copied().unwrap_or(0),
    };
    Ok((input, (iface.trim().to_string(), counters)))
}

/// Parse `/proc/net/dev`. The first two lines are column headers and skipped.
pub fn parse(input: &str) -> Vec<(String, IfCounters)> {
    input
        .lines()
        .skip(2)
        .filter_map(|l| dev_line(l).ok().map(|(_, v)| v))
        .collect()
}

/// Read and parse `/proc/net/dev`.
pub fn read() -> std::io::Result<Vec<(String, IfCounters)>> {
    Ok(parse(&std::fs::read_to_string(Path::new("/proc/net/dev"))?))
}

/// Pick a sensible default interface: skip `lo`, prefer one that has carried
/// traffic, else the first non-loopback present.
pub fn pick_default(ifaces: &[(String, IfCounters)]) -> Option<String> {
    let non_lo = || ifaces.iter().filter(|(n, _)| n != "lo");
    non_lo()
        .find(|(_, c)| c.rx_bytes > 0 || c.tx_bytes > 0)
        .or_else(|| non_lo().next())
        .map(|(n, _)| n.clone())
}

/// One `/proc/net/wireless` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Wireless {
    pub iface: String,
    /// Link quality (typically out of 70).
    pub link: f64,
    /// Signal level in dBm (negative; closer to 0 is stronger).
    pub level_dbm: f64,
}

/// Parse `/proc/net/wireless`. The columns carry trailing dots (`54.`, `-56.`)
/// that confuse a strict float parser, so this uses a tolerant
/// whitespace-split rather than nom.
pub fn parse_wireless(input: &str) -> Vec<Wireless> {
    input
        .lines()
        .skip(2)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let iface = cols.first()?.strip_suffix(':')?;
            let link = parse_dotted(cols.get(2)?)?;
            let level = parse_dotted(cols.get(3)?)?;
            Some(Wireless {
                iface: iface.to_string(),
                link,
                level_dbm: level,
            })
        })
        .collect()
}

/// Parse a `/proc/net/wireless` numeric column, tolerating the trailing dot.
fn parse_dotted(tok: &str) -> Option<f64> {
    tok.trim_end_matches('.').parse::<f64>().ok()
}

/// Read and parse `/proc/net/wireless`.
pub fn read_wireless() -> std::io::Result<Vec<Wireless>> {
    Ok(parse_wireless(&std::fs::read_to_string(Path::new(
        "/proc/net/wireless",
    ))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000      10    0    0    0     0          0         0     1000      10    0    0    0     0       0          0
  eth0: 5000      50    0    0    0     0          0         0     2000      20    0    0    0     0       0          0
";

    #[test]
    fn parses_interfaces_and_columns() {
        let v = super::parse(DEV);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "lo");
        assert_eq!(v[1].0, "eth0");
        assert_eq!(v[1].1.rx_bytes, 5000);
        assert_eq!(v[1].1.tx_bytes, 2000);
    }

    #[test]
    fn picks_non_loopback_with_traffic() {
        let v = super::parse(DEV);
        assert_eq!(pick_default(&v).as_deref(), Some("eth0"));
    }

    #[test]
    fn rates_divide_by_elapsed() {
        let prev = IfCounters {
            rx_bytes: 1000,
            tx_bytes: 500,
        };
        let now = IfCounters {
            rx_bytes: 3000,
            tx_bytes: 1500,
        };
        let (rx, tx) = now.rates(&prev, 2.0);
        assert_eq!(rx, 1000.0); // 2000 bytes / 2s
        assert_eq!(tx, 500.0);
        // Counter reset → no negative spike.
        assert_eq!(prev.rates(&now, 2.0), (0.0, 0.0));
    }

    #[test]
    fn wireless_parses_dotted_columns() {
        let w = "\
Inter-| sta-|   Quality        |   Discarded packets
 face | tus | link level noise |  nwid  crypt   frag
 wlan0: 0000   54.  -56.  -256        0      0      0
";
        let rows = parse_wireless(w);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].iface, "wlan0");
        assert_eq!(rows[0].link, 54.0);
        assert_eq!(rows[0].level_dbm, -56.0);
    }
}
