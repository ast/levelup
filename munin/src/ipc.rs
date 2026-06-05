//! IPC server: a unix-socket listener that accepts JSON-lines requests
//! from the `munin` CLI.
//!
//! Each accepted connection runs as its own tokio task and serves a
//! request stream until EOF.
//!
//! The surface is deliberately narrow: only ops that touch shared mutable
//! state live here.
//!
//! - `ping` writes `{"kind":"ok"}` — a liveness probe used by `bind_clean`.
//! - Capture commands (`add-start`, `add-end`) are forwarded to the storage
//!   thread and produce **no response** — the client writes the request
//!   and exits without reading.
//! - `import` writes a `StoreCmd::Import` with a oneshot reply channel and
//!   awaits it; the work itself runs on the storage thread.
//!
//! Read commands (`list` / `search` / `get`) are NOT served over IPC — the
//! CLI opens the SQLite file directly. See `bin/munin.rs::run_read`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::proto::{Request, Response};
use crate::storage::StoreCmd;

pub type StoreTx = mpsc::Sender<StoreCmd>;

/// Static runtime facts about this daemon, surfaced over the `Status` op so
/// `munin daemon status` can print pid / uptime / resolved paths.
#[derive(Debug, Clone)]
pub struct DaemonInfo {
    pub started_unix_ns: i64,
    pub db_path: String,
    pub socket_path: String,
}

/// Bind to `socket_path` (cleaning a stale file if no live daemon owns it)
/// and serve connections until the listener errors.
pub async fn serve(socket_path: PathBuf, store_tx: StoreTx, info: DaemonInfo) -> Result<()> {
    bind_clean(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    restrict_socket_permissions(&socket_path)?;
    info!(socket = %socket_path.display(), "ipc listening");

    // std::sync::mpsc::Sender is Clone + Send but not Sync; wrap in a Mutex so
    // multiple tokio tasks can grab it without contention beyond the brief
    // moment of the send.
    let store_tx = std::sync::Arc::new(Mutex::new(store_tx));
    let info = std::sync::Arc::new(info);

    loop {
        let (stream, _addr) = listener.accept().await.context("accept")?;
        let store_tx = store_tx.clone();
        let info = info.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, store_tx, info).await {
                warn!(error = %e, "ipc connection error");
            }
        });
    }
}

/// Clamp the socket to owner-only (0600). A peer that can connect may inject
/// `add-start` / `import` (history poisoning). Inside `$XDG_RUNTIME_DIR`
/// (mode 0700) the directory gates access, but the `/tmp` fallback path is
/// world-traversable — so we harden the socket itself.
fn restrict_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
}

/// If `path` exists, probe it; if no daemon answers, unlink it. If a daemon
/// does answer, refuse to start.
fn bind_clean(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => anyhow::bail!("another munind appears to be running at {}", path.display()),
        Err(_) => std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display())),
    }
}

async fn handle_connection(
    stream: UnixStream,
    store_tx: std::sync::Arc<Mutex<StoreTx>>,
    info: std::sync::Arc<DaemonInfo>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                write_response(
                    &mut wr,
                    &Response::Error {
                        message: format!("bad request: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };
        dispatch(req, &store_tx, &info, &mut wr).await?;
    }
}

async fn dispatch<W: AsyncWriteExt + Unpin>(
    req: Request,
    store_tx: &Mutex<StoreTx>,
    info: &DaemonInfo,
    wr: &mut W,
) -> Result<()> {
    match req {
        Request::Ping => write_response(wr, &Response::Ok).await,
        Request::Status => {
            write_response(
                wr,
                &Response::Status {
                    pid: std::process::id(),
                    version: crate::VERSION.to_string(),
                    started_unix_ns: info.started_unix_ns,
                    db_path: info.db_path.clone(),
                    socket_path: info.socket_path.clone(),
                },
            )
            .await
        }
        Request::Shutdown => {
            // Reply first so the client sees a clean Ok, then drive the same
            // graceful path SIGTERM/SIGINT use (wait_for_shutdown_signal in
            // bin/munind.rs). raise() targets our own process.
            write_response(wr, &Response::Ok).await?;
            if let Err(e) = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM) {
                warn!(error = %e, "failed to raise SIGTERM for shutdown");
            }
            Ok(())
        }
        Request::Import { path, source } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            let send_result = {
                let guard = store_tx.lock().expect("store_tx poisoned");
                guard.send(StoreCmd::Import {
                    path: PathBuf::from(path),
                    source,
                    reply: reply_tx,
                })
            };
            if send_result.is_err() {
                return write_error(wr, "storage thread unavailable".into()).await;
            }
            match reply_rx.await {
                Ok(Ok(inserted)) => write_response(wr, &Response::Imported { inserted }).await,
                Ok(Err(e)) => write_error(wr, e.to_string()).await,
                Err(_) => write_error(wr, "no reply from storage thread".into()).await,
            }
        }
    }
}

async fn write_response<W: AsyncWriteExt + Unpin>(wr: &mut W, resp: &Response) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    wr.write_all(json.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await?;
    Ok(())
}

async fn write_error<W: AsyncWriteExt + Unpin>(wr: &mut W, message: String) -> Result<()> {
    write_response(wr, &Response::Error { message }).await
}
