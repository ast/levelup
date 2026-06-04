//! Detect the local IPv4 subnet(s) to sweep, rootless via `if-addrs`.

use std::net::Ipv4Addr;

/// Don't sweep more than this many hosts from one interface (a /16 is 65k —
/// cap it so a misconfigured prefix can't launch a giant scan).
const MAX_HOSTS: usize = 4096;

pub struct LocalNet {
    /// e.g. `192.168.1.0/24` — for display.
    pub cidr: String,
    /// The host addresses to probe (network+1 .. broadcast-1, capped).
    pub hosts: Vec<Ipv4Addr>,
}

/// Every non-loopback IPv4 interface with a usable prefix.
pub fn detect() -> Vec<LocalNet> {
    let mut nets = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return nets;
    };
    for iface in ifaces {
        if iface.is_loopback() || is_virtual(&iface.name) {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue; // IPv6 neighbours are a follow-up
        };
        let prefix = mask_to_prefix(v4.netmask);
        // Skip absurdly large or point-to-point (/31, /32) ranges.
        if !(16..=30).contains(&prefix) {
            continue;
        }
        let net = u32::from(v4.ip) & u32::from(v4.netmask);
        nets.push(LocalNet {
            cidr: format!("{}/{prefix}", Ipv4Addr::from(net)),
            hosts: host_list(net, u32::from(v4.netmask)),
        });
    }
    nets
}

fn host_list(net: u32, mask: u32) -> Vec<Ipv4Addr> {
    let broadcast = net | !mask;
    let mut hosts = Vec::new();
    let mut a = net.wrapping_add(1);
    while a < broadcast && hosts.len() < MAX_HOSTS {
        hosts.push(Ipv4Addr::from(a));
        a += 1;
    }
    hosts
}

fn mask_to_prefix(mask: Ipv4Addr) -> u8 {
    u32::from(mask).count_ones() as u8
}

/// Skip virtual/container/VPN interfaces (docker bridges, veth, wireguard,
/// tailscale…) — their subnets are huge and irrelevant to "devices on my LAN".
fn is_virtual(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker",
        "br-",
        "veth",
        "virbr",
        "vmnet",
        "tailscale",
        "tun",
        "tap",
        "wg",
        "zt",
        "podman",
        "cni",
        "flannel",
        "kube",
        "lxc",
        "lxd",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_24_yields_254_hosts() {
        let net = u32::from(Ipv4Addr::new(192, 168, 1, 0));
        let mask = u32::from(Ipv4Addr::new(255, 255, 255, 0));
        let hosts = host_list(net, mask);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));
    }

    #[test]
    fn prefix_from_mask() {
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 0, 0)), 16);
    }
}
