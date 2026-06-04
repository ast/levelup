//! Rootless host discovery: provoke kernel ARP by attempting TCP connects to a
//! few common ports across the subnet, then read the populated neighbour cache
//! (`/proc/net/arp`) for IP↔MAC. No raw sockets, no root.
//!
//! Even a *refused* or *filtered* connect works: to send the SYN the kernel
//! must first resolve the destination MAC (ARP), which it then caches — so any
//! host that answers at layer 2 lands in the table regardless of open ports.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

/// One connect is enough to make the kernel resolve (and cache) the MAC; a
/// second common port just improves the odds of reaching a quiet host.
const PROBE_PORTS: &[u16] = &[80, 443];
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_INFLIGHT: usize = 512;

/// Fire connect probes at every host (concurrently, bounded) to populate ARP.
/// Results are intentionally ignored — the side effect (ARP resolution) is the
/// point.
pub async fn sweep(hosts: &[Ipv4Addr]) {
    let sem = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let mut set = JoinSet::new();
    for &ip in hosts {
        for &port in PROBE_PORTS {
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let _ = timeout(
                    CONNECT_TIMEOUT,
                    TcpStream::connect(SocketAddr::from((ip, port))),
                )
                .await;
            });
        }
    }
    while set.join_next().await.is_some() {}
}

/// Read `(ip, mac)` for every complete IPv4 entry in the kernel's ARP cache.
pub fn read_arp_cache() -> Vec<(Ipv4Addr, String)> {
    let Ok(s) = std::fs::read_to_string("/proc/net/arp") else {
        return Vec::new();
    };
    s.lines().skip(1).filter_map(parse_arp_line).collect()
}

/// One `/proc/net/arp` row → `(ip, mac)` if it's a complete entry.
/// Columns: `IP  HWtype  Flags  HWaddr  Mask  Device`. Flags `0x2` = complete.
fn parse_arp_line(line: &str) -> Option<(Ipv4Addr, String)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 4 {
        return None;
    }
    let ip: Ipv4Addr = f[0].parse().ok()?;
    let flags = u32::from_str_radix(f[2].trim_start_matches("0x"), 16).ok()?;
    let mac = f[3].to_ascii_lowercase();
    // 0x2 = ATF_COM (complete); skip incomplete / all-zero entries.
    if flags & 0x2 == 0 || mac == "00:00:00:00:00:00" {
        return None;
    }
    Some((ip, mac))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_entry_skips_incomplete() {
        let complete =
            "192.168.1.1     0x1         0x2         aa:bb:cc:dd:ee:ff     *        wlan0";
        assert_eq!(
            parse_arp_line(complete),
            Some((Ipv4Addr::new(192, 168, 1, 1), "aa:bb:cc:dd:ee:ff".into()))
        );
        // Flags 0x0 = incomplete → skipped.
        let incomplete =
            "192.168.1.9     0x1         0x0         00:00:00:00:00:00     *        wlan0";
        assert_eq!(parse_arp_line(incomplete), None);
    }
}
