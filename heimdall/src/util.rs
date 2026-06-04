//! Small shared helpers, ported from the sibling crates (copy-now, extract-later).

use tracing_subscriber::EnvFilter;

/// Install a stderr `tracing` subscriber honouring `HEIMDALL_LOG` (default `info`).
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("HEIMDALL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Neutralise control bytes so a device-advertised name (mDNS/SSDP strings can
/// contain anything) can't drive the terminal when it scrolls past. Ported
/// verbatim from munin.
pub fn sanitize_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\u{21B5}'),
            '\t' => out.push(' '),
            '\u{7f}' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push((b'@' + c as u8) as char);
            }
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}
