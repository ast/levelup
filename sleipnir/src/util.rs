//! Small shared helpers, ported from munin's `lib.rs` (copy-now, extract-later
//! — three tools now share this shape). Sleipnir works in unix *seconds*
//! (frecency doesn't need nanosecond precision), where munin uses nanoseconds.

use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;

/// Seconds since the unix epoch. Frecency math (`last_access`, decay buckets)
/// is all in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `$HOME`, or `None` if unset/empty.
pub fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|s| !s.is_empty())
}

/// Abbreviate a leading `$HOME` to `~` for display (`/home/a/src` → `~/src`).
/// Display-only — the action always uses the real absolute path.
pub fn abbrev_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(h) if path == h => "~".to_string(),
        Some(h) => match path.strip_prefix(h) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Install a stderr `tracing` subscriber honouring `SLEIPNIR_LOG` (defaults to
/// `info`). Same EnvFilter shape as munin/hugin.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SLEIPNIR_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Make arbitrary stored text safe to print to a terminal — neutralise control
/// bytes so a stored escape can't drive the terminal. Ported verbatim from
/// munin's `sanitize_display`. Paths rarely contain control bytes, but a
/// maliciously named file shouldn't be able to reprogram the terminal when it
/// scrolls past in the picker.
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
    fn abbrev_home_cases() {
        assert_eq!(abbrev_home("/home/a/src", Some("/home/a")), "~/src");
        assert_eq!(abbrev_home("/home/a", Some("/home/a")), "~");
        // A sibling that merely shares a prefix must NOT be abbreviated.
        assert_eq!(abbrev_home("/home/ab/x", Some("/home/a")), "/home/ab/x");
        assert_eq!(abbrev_home("/etc", Some("/home/a")), "/etc");
        assert_eq!(abbrev_home("/home/a/src", None), "/home/a/src");
    }
}
