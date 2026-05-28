pub mod cli;
pub mod config;
pub mod ipc;
pub mod proto;
pub mod shells;
pub mod storage;
pub mod tui;

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
