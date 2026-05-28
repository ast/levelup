pub mod cli;
pub mod ipc;
pub mod proto;
pub mod storage;
pub mod wayland;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone)]
pub struct CapturedEntry {
    pub ts_unix_ns: i64,
    pub selection: Selection,
    pub parts: Vec<(String, Vec<u8>)>,
}

impl CapturedEntry {
    pub fn now(selection: Selection, parts: Vec<(String, Vec<u8>)>) -> Self {
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
