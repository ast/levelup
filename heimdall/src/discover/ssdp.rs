//! SSDP / UPnP discovery (rootless, UDP multicast 1900) — finds routers, smart
//! TVs, media servers and the like that advertise UPnP. Rolled in-tree (hugin's
//! keep-protocol-in-tree ethos): send an `M-SEARCH`, parse the HTTP-over-UDP
//! responses for the device type.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const MSEARCH: &str = "M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 1\r\n\
ST: ssdp:all\r\n\r\n";

pub struct SsdpHit {
    pub ip: Ipv4Addr,
    pub service: String,
}

/// Send the M-SEARCH and gather responders for `dur`.
pub fn collect(dur: Duration) -> Vec<SsdpHit> {
    let mut out = Vec::new();
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else {
        return out;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = sock.send_to(MSEARCH.as_bytes(), "239.255.255.250:1900");

    let deadline = Instant::now() + dur;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        if let Ok((n, SocketAddr::V4(src))) = sock.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            out.push(SsdpHit {
                ip: *src.ip(),
                service: parse_label(&text),
            });
        }
    }
    out
}

/// Case-insensitive header value lookup.
fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A friendly-ish label: prefer the `ST` device/service type, else `SERVER`.
fn parse_label(text: &str) -> String {
    if let Some(st) = header(text, "ST") {
        let s = simplify_st(st);
        if !s.is_empty() {
            return s;
        }
    }
    header(text, "SERVER")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "upnp".to_string())
}

/// `urn:schemas-upnp-org:device:MediaServer:1` → `MediaServer`;
/// `upnp:rootdevice` → `rootdevice`; a bare `uuid:…` → `upnp`.
fn simplify_st(st: &str) -> String {
    let parts: Vec<&str> = st.split(':').collect();
    if let Some(i) = parts.iter().position(|&p| p == "device" || p == "service")
        && let Some(name) = parts.get(i + 1)
    {
        return name.to_string();
    }
    let last = parts.last().copied().unwrap_or("upnp");
    // A raw UUID isn't a useful label.
    if last.len() > 20 && last.contains('-') {
        "upnp".to_string()
    } else {
        last.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn st_simplification() {
        assert_eq!(
            simplify_st("urn:schemas-upnp-org:device:MediaServer:1"),
            "MediaServer"
        );
        assert_eq!(simplify_st("upnp:rootdevice"), "rootdevice");
        assert_eq!(
            simplify_st("uuid:2f402f80-da50-11e1-9b23-0017881234ab"),
            "upnp"
        );
    }

    #[test]
    fn header_is_case_insensitive() {
        let resp = "HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\nSERVER: Linux UPnP/1.0\r\n\r\n";
        assert_eq!(header(resp, "st"), Some("upnp:rootdevice"));
        assert_eq!(header(resp, "server"), Some("Linux UPnP/1.0"));
    }
}
