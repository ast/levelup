//! The live discovery engine: three background threads (ARP sweep loop, mDNS
//! continuous browse, SSDP poll loop) streaming `Seen` facts to the TUI over an
//! `mpsc` channel. The TUI drains it each tick and merges by IP, so devices and
//! their names fill in live.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::device::Seen;
use crate::discover::{arp, mdns, oui, ssdp, subnet};

/// Re-sweep ARP this often (also the liveness heartbeat — a device re-seen here
/// refreshes its last-seen).
const ARP_EVERY: Duration = Duration::from_secs(12);
/// Re-issue the SSDP M-SEARCH this often.
const SSDP_EVERY: Duration = Duration::from_secs(20);

pub struct Engine {
    pub rx: Receiver<Seen>,
}

/// Start the three discovery threads. They exit when the receiver is dropped.
pub fn spawn() -> Engine {
    let (tx, rx) = mpsc::channel();

    // ARP sweep loop — needs async connects, so its own tokio runtime.
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let vendors = oui::Vendors::load();
            rt.block_on(async {
                loop {
                    for net in &subnet::detect() {
                        arp::sweep(&net.hosts).await;
                    }
                    for (ip, mac) in arp::read_arp_cache() {
                        let vendor = vendors.as_ref().and_then(|v| v.lookup(&mac));
                        let seen = Seen {
                            ip,
                            mac: Some(mac),
                            vendor,
                            hostname: None,
                            service: None,
                        };
                        if tx.send(seen).is_err() {
                            return;
                        }
                    }
                    tokio::time::sleep(ARP_EVERY).await;
                }
            });
        });
    }

    // mDNS continuous browse.
    {
        let tx = tx.clone();
        thread::spawn(move || mdns::run(tx));
    }

    // SSDP poll loop.
    {
        let tx = tx.clone();
        thread::spawn(move || {
            loop {
                for hit in ssdp::collect(Duration::from_secs(2)) {
                    let seen = Seen {
                        ip: hit.ip,
                        mac: None,
                        vendor: None,
                        hostname: None,
                        service: Some(hit.service),
                    };
                    if tx.send(seen).is_err() {
                        return;
                    }
                }
                thread::sleep(SSDP_EVERY);
            }
        });
    }

    Engine { rx }
}
