//! mimir — a compact, interactive status bar for sway (an i3status replacement).
//!
//! A single foreground process that swaybar spawns: it prints the
//! swaybar/i3bar JSON protocol on stdout and reads click events on stdin.
//! System state is read straight from `/proc` & `/sys` (parsed with `nom`),
//! in the same minimal-dependency spirit as the rest of the `levelup` suite.
//! No daemon, no async runtime.

pub mod blocks;
pub mod config;
pub mod protocol;
pub mod render;
pub mod sys;

use tracing_subscriber::EnvFilter;

/// Install a `tracing_subscriber` writing to **stderr** (stdout is the
/// swaybar protocol and must stay clean JSON), honouring the `MIMIR_LOG`
/// env var (falls back to `info`). Mirrors `munin::init_tracing`.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MIMIR_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Compact human-readable byte size: `512B` / `4K` / `34M` / `15.4G` / `2.1T`.
/// Sub-gigabyte values drop the decimal (a status bar wants them short);
/// gigabytes and up keep one. Used by the memory and disk blocks.
pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K * K {
        format!("{:.1}T", f / (K * K * K * K))
    } else if f >= K * K * K {
        format!("{:.1}G", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.0}M", f / (K * K))
    } else if f >= K {
        format!("{:.0}K", f / K)
    } else {
        format!("{n}B")
    }
}

/// Compact bit/byte *rate* for the network block: `0B` / `1.2K` / `256K` per
/// second (the caller prefixes a glyph). One decimal below 10, none above, so
/// the result is at most 5 chars wide — callers right-pad to a fixed field so
/// a steadily-changing rate doesn't shift the bar.
pub fn human_rate(bytes_per_sec: f64) -> String {
    const K: f64 = 1024.0;
    let f = bytes_per_sec.max(0.0);
    let (val, unit) = if f >= K * K * K {
        (f / (K * K * K), 'G')
    } else if f >= K * K {
        (f / (K * K), 'M')
    } else if f >= K {
        (f / K, 'K')
    } else {
        return format!("{f:.0}B");
    };
    if val < 10.0 {
        format!("{val:.1}{unit}")
    } else {
        format!("{val:.0}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_buckets() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(4096), "4K");
        assert_eq!(human_bytes(34 * 1024 * 1024), "34M");
        assert_eq!(human_bytes(16 * 1024 * 1024 * 1024), "16.0G");
    }

    #[test]
    fn human_rate_buckets() {
        assert_eq!(human_rate(0.0), "0B");
        assert_eq!(human_rate(1536.0), "1.5K"); // <10 → one decimal
        assert_eq!(human_rate(200.0 * 1024.0), "200K"); // ≥10 → no decimal
        assert_eq!(human_rate(-5.0), "0B"); // clamps negatives (counter wrap)
        // Never wider than 5 chars, so a width-5 field never overflows.
        for bps in [0.0, 999.0, 1536.0, 200.0 * 1024.0, 5.0e8, 9.9e9] {
            assert!(human_rate(bps).chars().count() <= 5, "{}", human_rate(bps));
        }
    }
}
