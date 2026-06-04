//! Discovery. For now a one-shot rootless scan fusing three concurrent sources
//! (ARP sweep + mDNS + SSDP); the live background engine wraps these on periodic
//! loops in a later build step.

pub mod arp;
pub mod engine;
pub mod mdns;
pub mod oui;
pub mod ssdp;
pub mod subnet;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::device::Device;

/// How long the passive discoverers (mDNS/SSDP) listen during a one-shot scan.
const LISTEN: Duration = Duration::from_secs(3);

/// Sweep + listen concurrently, then fuse everything by IP. ARP gives MAC +
/// vendor; mDNS gives hostname + service types; SSDP gives device types.
pub async fn scan_once() -> Vec<Device> {
    let nets = subnet::detect();
    let vendors = oui::Vendors::load();

    // mDNS and SSDP listen on blocking threads while the ARP sweep runs.
    let mdns_h = tokio::task::spawn_blocking(|| mdns::collect(LISTEN));
    let ssdp_h = tokio::task::spawn_blocking(|| ssdp::collect(LISTEN));
    for net in &nets {
        arp::sweep(&net.hosts).await;
    }
    let arp = arp::read_arp_cache();
    let mdns = mdns_h.await.unwrap_or_default();
    let ssdp = ssdp_h.await.unwrap_or_default();

    let mut map: BTreeMap<Ipv4Addr, Device> = BTreeMap::new();
    for (ip, mac) in arp {
        let d = map.entry(ip).or_insert_with(|| Device::new(ip));
        d.vendor = vendors.as_ref().and_then(|v| v.lookup(&mac));
        d.mac = Some(mac);
    }
    for hit in mdns {
        let d = map.entry(hit.ip).or_insert_with(|| Device::new(hit.ip));
        if d.hostname.is_none() {
            d.hostname = hit.hostname;
        }
        d.add_service(&hit.service);
    }
    for hit in ssdp {
        let d = map.entry(hit.ip).or_insert_with(|| Device::new(hit.ip));
        d.add_service(&hit.service);
    }
    map.into_values().collect()
}

/// The CIDRs being scanned, for display.
pub fn local_cidrs() -> Vec<String> {
    subnet::detect().into_iter().map(|n| n.cidr).collect()
}
