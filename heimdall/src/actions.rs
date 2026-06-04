//! Acting on a selected device: copy to clipboard, open in a browser, and an
//! on-demand TCP port scan. (ssh suspends the TUI and is driven from the run
//! loop, which owns the terminal.) Everything here is rootless and only fires
//! on an explicit keypress.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Common service ports probed by the on-demand scan.
const COMMON_PORTS: &[u16] = &[
    21, 22, 23, 25, 53, 80, 110, 139, 143, 443, 445, 554, 587, 631, 993, 995, 1883, 3000, 3306,
    3389, 5000, 5432, 5900, 6379, 8000, 8080, 8081, 8443, 8883, 9000, 9090, 32400,
];
const PORT_TIMEOUT: Duration = Duration::from_millis(500);

/// Copy `text` to the Wayland clipboard (`wl-copy`), falling back to `xclip`.
/// Returns whether a clipboard helper was found.
pub fn copy(text: &str) -> bool {
    for (cmd, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
        if let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return true;
        }
    }
    false
}

/// Open a URL in the default browser, detached.
pub fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Scan `ip`'s common ports concurrently on a background thread; send the open
/// ones back over `tx` (the TUI drains it and updates the device).
pub fn scan_ports(ip: Ipv4Addr, tx: Sender<(Ipv4Addr, Vec<u16>)>) {
    thread::spawn(move || {
        let found = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for &port in COMMON_PORTS {
            let found = found.clone();
            handles.push(thread::spawn(move || {
                if TcpStream::connect_timeout(&SocketAddr::from((ip, port)), PORT_TIMEOUT).is_ok() {
                    found.lock().unwrap().push(port);
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let mut ports = Arc::try_unwrap(found)
            .map(|m| m.into_inner().unwrap_or_default())
            .unwrap_or_default();
        ports.sort_unstable();
        let _ = tx.send((ip, ports));
    });
}
