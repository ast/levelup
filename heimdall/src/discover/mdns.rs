//! mDNS / DNS-SD discovery (rootless, UDP 5353) — hands us hostnames and
//! service types an ARP sweep never sees (printers, Chromecasts, HomeKit, …).
//!
//! We browse a curated set of common service types and collect resolved
//! records for a time-box. (The `_services._dns-sd._udp` meta-query for *full*
//! enumeration is a later refinement.)

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

/// The service types most devices actually advertise.
const TYPES: &[&str] = &[
    "_http._tcp.local.",
    "_https._tcp.local.",
    "_ssh._tcp.local.",
    "_sftp-ssh._tcp.local.",
    "_ipp._tcp.local.",
    "_ipps._tcp.local.",
    "_printer._tcp.local.",
    "_pdl-datastream._tcp.local.",
    "_airplay._tcp.local.",
    "_raop._tcp.local.",
    "_googlecast._tcp.local.",
    "_spotify-connect._tcp.local.",
    "_smb._tcp.local.",
    "_afpovertcp._tcp.local.",
    "_workstation._tcp.local.",
    "_device-info._tcp.local.",
    "_companion-link._tcp.local.",
    "_homekit._tcp.local.",
    "_hap._tcp.local.",
];

pub struct MdnsHit {
    pub ip: Ipv4Addr,
    pub hostname: Option<String>,
    pub service: String,
}

/// Browse the curated types for `dur`, returning every resolved IPv4 record.
pub fn collect(dur: Duration) -> Vec<MdnsHit> {
    let mut out = Vec::new();
    let Ok(daemon) = ServiceDaemon::new() else {
        return out;
    };
    let receivers: Vec<(&str, _)> = TYPES
        .iter()
        .filter_map(|t| daemon.browse(t).ok().map(|rx| (*t, rx)))
        .collect();

    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        for (ty, rx) in &receivers {
            while let Ok(ev) = rx.try_recv() {
                if let ServiceEvent::ServiceResolved(info) = ev {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let host = (!host.is_empty()).then_some(host);
                    for addr in info.get_addresses() {
                        if let IpAddr::V4(v4) = addr {
                            out.push(MdnsHit {
                                ip: *v4,
                                hostname: host.clone(),
                                service: service_label(ty),
                            });
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let _ = daemon.shutdown();
    out
}

/// Browse continuously, streaming each resolved record to the engine. Runs
/// until the receiver is dropped. Used by the live TUI engine.
pub fn run(tx: std::sync::mpsc::Sender<crate::device::Seen>) {
    let Ok(daemon) = ServiceDaemon::new() else {
        return;
    };
    let receivers: Vec<(&str, _)> = TYPES
        .iter()
        .filter_map(|t| daemon.browse(t).ok().map(|rx| (*t, rx)))
        .collect();
    loop {
        for (ty, rx) in &receivers {
            while let Ok(ev) = rx.try_recv() {
                if let ServiceEvent::ServiceResolved(info) = ev {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let host = (!host.is_empty()).then_some(host);
                    for addr in info.get_addresses() {
                        if let IpAddr::V4(v4) = addr {
                            let seen = crate::device::Seen {
                                ip: *v4,
                                mac: None,
                                vendor: None,
                                hostname: host.clone(),
                                service: Some(service_label(ty)),
                            };
                            if tx.send(seen).is_err() {
                                let _ = daemon.shutdown();
                                return;
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `_googlecast._tcp.local.` → `googlecast`.
fn service_label(ty: &str) -> String {
    ty.trim_start_matches('_')
        .split("._")
        .next()
        .unwrap_or(ty)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_strips_dnssd_decoration() {
        assert_eq!(service_label("_googlecast._tcp.local."), "googlecast");
        assert_eq!(service_label("_ipp._tcp.local."), "ipp");
    }
}
