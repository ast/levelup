//! IPC server: a unix-socket listener that accepts JSON-lines requests
//! from the `munin` CLI.
//!
//! Each accepted connection runs as its own tokio task and serves a
//! request stream until EOF.
//!
//! - Capture commands (`add-start`, `add-end`) are forwarded to the storage
//!   thread and produce **no response** — the client is expected to write
//!   the request and exit without reading.
//! - Read commands (`ping`, `list`, `search`, `get`) write a single JSON
//!   response. Reads open an ephemeral `Connection` inside `spawn_blocking`;
//!   WAL mode makes that safe alongside the storage thread's writer.
//! - `import` writes a `StoreCmd::Import` with a oneshot reply channel and
//!   blocks the IPC task on it; the work itself runs on the storage thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::now_unix_ns;
use crate::proto::{Request, Response};
use crate::storage::{self, StoreCmd};

pub type StoreTx = mpsc::Sender<StoreCmd>;

/// Bind to `socket_path` (cleaning a stale file if no live daemon owns it)
/// and serve connections until the listener errors.
pub async fn serve(socket_path: PathBuf, db_path: PathBuf, store_tx: StoreTx) -> Result<()> {
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
        let db_path = db_path.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, db_path, store_tx).await {
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
    db_path: PathBuf,
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
        dispatch(req, &db_path, &store_tx, &mut wr).await?;
    }
}

async fn dispatch<W: AsyncWriteExt + Unpin>(
    req: Request,
    db_path: &Path,
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
        Request::List { limit, filters } => {
            let db = db_path.to_owned();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let conn = Connection::open(&db)?;
                storage::list(&conn, limit.unwrap_or(50), &filters)
            })
            .await?;
            match result {
                Ok(entries) => write_response(wr, &Response::Entries { entries }).await,
                Err(e) => write_error(wr, e.to_string()).await,
            }
        }
        Request::Search {
            query,
            raw,
            sort,
            limit,
            filters,
        } => {
            let db = db_path.to_owned();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let conn = Connection::open(&db)?;
                storage::search(&conn, &query, raw, sort, limit.unwrap_or(50), &filters)
            })
            .await?;
            match result {
                Ok(entries) => write_response(wr, &Response::Entries { entries }).await,
                Err(e) => write_error(wr, e.to_string()).await,
            }
        }
        Request::Get { id } => {
            let db = db_path.to_owned();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let conn = Connection::open(&db)?;
                storage::get(&conn, id)
            })
            .await?;
            match result {
                Ok(Some(entry)) => write_response(wr, &Response::Entry { entry }).await,
                Ok(None) => write_error(wr, format!("no entry with id {id}")).await,
                Err(e) => write_error(wr, e.to_string()).await,
            }
        }
        Request::Import { path, shell } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            let send_result = {
                let guard = store_tx.lock().expect("store_tx poisoned");
                guard.send(StoreCmd::Import {
                    path: PathBuf::from(path),
                    shell,
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

