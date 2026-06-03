pub mod cli;
pub mod client;
pub mod config;
pub mod ipc;
pub mod proto;
pub mod storage;
pub mod tui;
pub mod wayland;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use tracing_subscriber::EnvFilter;

/// Default unix-socket path for daemon ↔ CLI IPC: `$XDG_RUNTIME_DIR/hugin.sock`
/// (falls back to `/tmp/hugin.sock`).
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hugin.sock")
}

/// Install a `tracing_subscriber` writing to stderr, honouring the `HUGIN_LOG`
/// env var (falls back to `info`). Idempotent guard left to the caller —
/// `tracing_subscriber::fmt().init()` will panic on a second call.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("HUGIN_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Regular,
    Primary,
}

impl Selection {
    pub fn as_str(self) -> &'static str {
        match self {
            Selection::Regular => "regular",
            Selection::Primary => "primary",
        }
    }
}

/// Default per-MIME-part size cap. Generous enough to keep 4K-screenshot PNGs
/// and large text, tight enough to stub video-scale blobs. Override with
/// `hugind --max-part-bytes`.
pub const DEFAULT_MAX_PART_BYTES: usize = 16 * 1024 * 1024;

/// How a captured MIME part was stored relative to the size cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartStore {
    /// Stored in full (the part fit under the cap).
    Full,
    /// A text part that exceeded the cap: the leading `blob` prefix is stored
    /// (UTF-8-boundary-snapped) and stays searchable; the rest is gone.
    Truncated,
    /// A binary part that exceeded the cap: no bytes kept (a truncated image
    /// is useless), only the record that this MIME existed.
    Omitted,
}

impl PartStore {
    /// On-disk encoding for `mime_parts.truncated`.
    pub fn as_i64(self) -> i64 {
        match self {
            PartStore::Full => 0,
            PartStore::Truncated => 1,
            PartStore::Omitted => 2,
        }
    }

    pub fn from_i64(n: i64) -> Self {
        match n {
            1 => PartStore::Truncated,
            2 => PartStore::Omitted,
            _ => PartStore::Full,
        }
    }
}

/// One MIME part of a capture. `blob` holds the stored bytes — the full
/// content, a truncated text prefix, or empty for an omitted oversized part.
#[derive(Debug, Clone)]
pub struct CapturedPart {
    pub mime: String,
    pub blob: Vec<u8>,
    pub store: PartStore,
}

impl CapturedPart {
    /// A fully-stored part (the common case, and the only kind produced when
    /// re-serving a stored entry for `hugin copy`).
    pub fn full(mime: String, blob: Vec<u8>) -> Self {
        Self {
            mime,
            blob,
            store: PartStore::Full,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CapturedEntry {
    pub ts_unix_ns: i64,
    pub selection: Selection,
    pub parts: Vec<CapturedPart>,
}

impl CapturedEntry {
    pub fn now(selection: Selection, parts: Vec<CapturedPart>) -> Self {
        let ts_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self {
            ts_unix_ns,
            selection,
            parts,
        }
    }
}

pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "UTF8_STRING" || mime == "STRING" || mime == "TEXT"
}

/// Format a nanosecond unix timestamp as local `YYYY-MM-DD HH:MM:SS`.
/// Shared by the CLI table and the interactive picker.
pub fn fmt_ts(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    match Local.timestamp_opt(secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("@{secs}"),
    }
}

/// Human-readable byte size (`512B` / `1.5K` / `3.2M`).
pub fn human_size(n: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = KB * 1024;
    if n >= MB {
        format!("{:.1}M", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}K", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

/// Make arbitrary clipboard text safe to print to a terminal. Clipboard content
/// is fully untrusted — copying a crafted string can embed control bytes that
/// *drive* the terminal (e.g. `ESC[?1003h` enables mouse reporting, spewing
/// motion reports over the prompt). We sanitize at the display boundary
/// (storage stays byte-faithful): C0 → caret notation (`^[`), DEL → `^?`,
/// newline → `↵`, tab → space, other control (C1) → U+FFFD.
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

/// Like [`sanitize_display`] but preserves newlines and tabs, for multi-line
/// views (the preview pane) where real line breaks drive the layout. Still
/// neutralizes ESC and the other control bytes that could drive the terminal.
pub fn sanitize_multiline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
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
    use super::{sanitize_display, sanitize_multiline};

    #[test]
    fn sanitize_neutralizes_control_bytes() {
        assert_eq!(sanitize_display("a\x1b[?1003hb"), "a^[[?1003hb");
        assert_eq!(sanitize_display("x\ty\nz"), "x y\u{21B5}z");
        assert_eq!(sanitize_display("a\x7fb"), "a^?b");
        assert_eq!(sanitize_display("plain ‹m›"), "plain ‹m›");
    }

    #[test]
    fn sanitize_multiline_keeps_layout_whitespace() {
        // Newlines and tabs survive (the preview pane needs them); ESC doesn't.
        assert_eq!(sanitize_multiline("a\nb\tc\x1b[0m"), "a\nb\tc^[[0m");
    }
}
