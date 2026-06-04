//! Small shared helpers, ported from sleipnir/munin (copy-now, extract-later).

use tracing_subscriber::EnvFilter;

/// Install a stderr `tracing` subscriber honouring `VALKYRIE_LOG` (defaults to
/// `info`). Same EnvFilter shape as the sibling crates.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("VALKYRIE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Make arbitrary text safe to print to a terminal — neutralise control bytes
/// so a process's argv (which can contain anything) can't drive the terminal
/// when it scrolls past in the picker. Ported verbatim from munin.
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
