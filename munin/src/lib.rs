pub mod cli;
pub mod config;
pub mod ipc;
pub mod proto;
pub mod shells;
pub mod storage;
pub mod tui;

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;

/// Default unix-socket path for daemon ↔ CLI IPC: `$XDG_RUNTIME_DIR/munin.sock`
/// (falls back to `/tmp/munin.sock`).
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("munin.sock")
}

/// Install a `tracing_subscriber` writing to stderr, honouring the `MUNIN_LOG`
/// env var (falls back to `info`).
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MUNIN_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

pub fn now_unix_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Linux-only: read the kernel-exposed hostname. Trims trailing whitespace and
/// returns `None` for empty/unreadable hostnames.
pub fn current_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Human-friendly duration formatter: `Some(ms)` → `"345ms"` / `"3.4s"` /
/// `"1m23s"`; `None` → `"-"`. Shared by the CLI's `print_table` and the
/// TUI's row renderer so they stay in sync.
pub fn fmt_dur(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        let s = ms / 1_000;
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Make arbitrary stored text safe to print to a terminal. Stored commands can
/// contain raw control bytes (bracketed-paste markers from imports, escape
/// sequences from pasted input) that would otherwise *drive* the terminal —
/// e.g. a stored `ESC[?1003h` flips it into mouse-reporting mode. We sanitize
/// at the display boundary (storage stays faithful for sync): C0 → caret
/// notation (`^[`), DEL → `^?`, newline → `↵`, tab → space, other control
/// (C1) → U+FFFD. Shared by the CLI's printers and the TUI's row renderer.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_neutralizes_control_bytes() {
        // The exact attack: a stored escape that enables mouse reporting.
        assert_eq!(sanitize_display("a\x1b[?1003hb"), "a^[[?1003hb");
        // Bracketed-paste prefix from atuin imports.
        assert_eq!(sanitize_display("\x1b[200~ls"), "^[[200~ls");
        // Whitespace handling: tab → space, newline → ↵.
        assert_eq!(sanitize_display("x\ty\nz"), "x y\u{21B5}z");
        // DEL and a C1 byte.
        assert_eq!(sanitize_display("a\x7fb\u{0085}c"), "a^?b\u{FFFD}c");
        // Plain text (incl. the ‹› highlight markers) is untouched.
        assert_eq!(sanitize_display("git ‹commit›"), "git ‹commit›");
    }

    #[test]
    fn fmt_dur_buckets() {
        assert_eq!(fmt_dur(None), "-");
        assert_eq!(fmt_dur(Some(0)), "0ms");
        assert_eq!(fmt_dur(Some(345)), "345ms");
        assert_eq!(fmt_dur(Some(999)), "999ms");
        assert_eq!(fmt_dur(Some(3_400)), "3.4s"); // sub-minute → seconds
        assert_eq!(fmt_dur(Some(83_000)), "1m23s"); // ≥1min → m+s, zero-padded
        assert_eq!(fmt_dur(Some(3_600_000)), "60m00s");
    }
}
