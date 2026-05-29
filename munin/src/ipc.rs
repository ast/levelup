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

use crate::now_unix_ns;
use crate::proto::{Request, Response};
use crate::storage::StoreCmd;

pub type StoreTx = mpsc::Sender<StoreCmd>;

/// Bind to `socket_path` (cleaning a stale file if no live daemon owns it)
/// and serve connections until the listener errors.
pub async fn serve(socket_path: PathBuf, store_tx: StoreTx) -> Result<()> {
    bind_clean(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), "ipc listening");

    // std::sync::mpsc::Sender is Clone + Send but not Sync; wrap in a Mutex so
    // multiple tokio tasks can grab it without contention beyond the brief
    // moment of the send.
    let store_tx = std::sync::Arc::new(Mutex::new(store_tx));

    loop {
        let (stream, _addr) = listener.accept().await.context("accept")?;
        let store_tx = store_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, store_tx).await {
                warn!(error = %e, "ipc connection error");
            }
        });
    }
}

/// If `path` exists, probe it; if no daemon answers, unlink it. If a daemon
/// does answer, refuse to start.
fn bind_clean(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "another munind appears to be running at {}",
            path.display()
        ),
        Err(_) => std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display())),
    }
}

async fn handle_connection(
    stream: UnixStream,
    store_tx: std::sync::Arc<Mutex<StoreTx>>,
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
        dispatch(req, &store_tx, &mut wr).await?;
    }
}

async fn dispatch<W: AsyncWriteExt + Unpin>(
    req: Request,
    store_tx: &Mutex<StoreTx>,
    wr: &mut W,
) -> Result<()> {
    match req {
        Request::Ping => write_response(wr, &Response::Ok).await,
        Request::AddStart {
            cmd,
            session,
            ts_unix_ns,
            cwd,
            hostname,
            shell,
        } => {
            let ts_unix_ns = ts_unix_ns.unwrap_or_else(now_unix_ns);
            let send_result = {
                let guard = store_tx.lock().expect("store_tx poisoned");
                guard.send(StoreCmd::AddStart {
                    cmd,
                    session,
                    ts_unix_ns,
                    cwd,
                    hostname,
                    shell,
                })
            };
            if let Err(e) = send_result {
                warn!(error = %e, "storage thread unavailable for add-start");
            }
            Ok(())
        }
        Request::AddEnd {
            session,
            exit_code,
            ts_unix_ns,
        } => {
            let ts_unix_ns = ts_unix_ns.unwrap_or_else(now_unix_ns);
            let send_result = {
                let guard = store_tx.lock().expect("store_tx poisoned");
                guard.send(StoreCmd::AddEnd {
                    session,
                    exit_code,
                    ts_unix_ns,
                })
            };
            if let Err(e) = send_result {
                warn!(error = %e, "storage thread unavailable for add-end");
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
                Ok(Ok(inserted)) => {
                    write_response(wr, &Response::Imported { inserted }).await
                }
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

