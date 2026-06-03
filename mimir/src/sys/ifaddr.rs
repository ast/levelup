//! Interface IP addresses via `getifaddrs(3)` (through `nix`). No shell-out.

use nix::ifaddrs::getifaddrs;

/// Return `(ipv4, ipv6)` for `iface` as display strings. IPv6 prefers a
/// non-link-local (global / ULA) address, falling back to link-local
/// (`fe80::…`) only if that's all the interface has.
pub fn addrs(iface: &str) -> (Option<String>, Option<String>) {
    let Ok(addrs) = getifaddrs() else {
        return (None, None);
    };
    let mut v4 = None;
    let mut v6_global = None;
    let mut v6_link_local = None;
    for ifa in addrs {
        if ifa.interface_name != iface {
            continue;
        }
        let Some(storage) = ifa.address else { continue };
        if let Some(sin) = storage.as_sockaddr_in() {
            v4.get_or_insert_with(|| sin.ip().to_string());
        } else if let Some(sin6) = storage.as_sockaddr_in6() {
            let ip = sin6.ip();
            if (ip.segments()[0] & 0xffc0) == 0xfe80 {
                v6_link_local.get_or_insert_with(|| ip.to_string());
            } else {
                v6_global.get_or_insert_with(|| ip.to_string());
            }
        }
    }
    (v4, v6_global.or(v6_link_local))
}
