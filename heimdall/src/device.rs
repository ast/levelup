//! The device model: what Heimdall knows about one host on the LAN.

use std::borrow::Cow;
use std::net::Ipv4Addr;

use crate::mac;

/// One fact about a device, streamed from the discovery engine to the TUI.
/// Fields are optional — each source fills what it knows; the TUI merges by IP.
#[derive(Debug, Clone)]
pub struct Seen {
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
    pub vendor: Option<String>,
    pub hostname: Option<String>,
    pub service: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub ip: Ipv4Addr,
    /// Stable identity when known (from the ARP/neighbour cache).
    pub mac: Option<String>,
    /// Manufacturer from the MAC OUI.
    pub vendor: Option<String>,
    /// Name from mDNS / reverse DNS.
    pub hostname: Option<String>,
    /// Advertised service types (mDNS) / device descriptions (SSDP).
    pub services: Vec<String>,
    /// Open TCP ports from an on-demand port scan.
    pub open_ports: Vec<u16>,
}

impl Device {
    pub fn new(ip: Ipv4Addr) -> Self {
        Self {
            ip,
            mac: None,
            vendor: None,
            hostname: None,
            services: Vec::new(),
            open_ports: Vec::new(),
        }
    }

    /// The VENDOR-column text: the OUI manufacturer when known, otherwise a note
    /// derived from the MAC's address bits. A locally-administered (randomized,
    /// virtual, or hand-set) MAC has no meaningful OUI, so we say `random`
    /// instead of a bare `?`; a group address shows `multicast`. `?` is left
    /// only for a genuinely unknown universal MAC (or no MAC at all).
    pub fn vendor_label(&self) -> Cow<'_, str> {
        if let Some(v) = &self.vendor {
            return Cow::Borrowed(v.as_str());
        }
        Cow::Borrowed(match self.mac.as_deref().and_then(mac::classify) {
            Some(mac::Kind::Local) => "random",
            Some(mac::Kind::Group) => "multicast",
            _ => "?",
        })
    }

    /// One string nucleo matches against — IP, MAC, vendor, hostname, services
    /// fused into one content space (type any of them to filter).
    pub fn haystack(&self) -> String {
        let mut s = self.ip.to_string();
        for part in [self.mac.as_deref(), self.hostname.as_deref()]
            .into_iter()
            .flatten()
        {
            s.push(' ');
            s.push_str(part);
        }
        // Include the vendor label (real name, or `random`/`multicast`) so those
        // are searchable; skip a bare `?`, which carries no signal.
        let vlabel = self.vendor_label();
        if vlabel != "?" {
            s.push(' ');
            s.push_str(&vlabel);
        }
        for svc in &self.services {
            s.push(' ');
            s.push_str(svc);
        }
        s
    }

    /// Add a service label if non-empty and not already present.
    pub fn add_service(&mut self, svc: &str) {
        let svc = svc.trim();
        if !svc.is_empty() && !self.services.iter().any(|s| s == svc) {
            self.services.push(svc.to_string());
        }
    }
}
