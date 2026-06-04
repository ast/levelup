//! Sleipnir-specific small helpers. (`sanitize_display` / `init_tracing` now
//! live in `levelup_core`.)

use std::time::{SystemTime, UNIX_EPOCH};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbrev_home_cases() {
        assert_eq!(abbrev_home("/home/a/src", Some("/home/a")), "~/src");
        assert_eq!(abbrev_home("/home/a", Some("/home/a")), "~");
        assert_eq!(abbrev_home("/home/ab/x", Some("/home/a")), "/home/ab/x");
        assert_eq!(abbrev_home("/etc", Some("/home/a")), "/etc");
        assert_eq!(abbrev_home("/home/a/src", None), "/home/a/src");
    }
}
