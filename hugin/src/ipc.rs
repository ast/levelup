//! IPC server: a unix-socket listener that answers `hugin` CLI requests.
//!
//! Each accepted connection runs as its own tokio task and serves a
//! request/response stream until EOF. SQLite reads happen inside
//! `spawn_blocking` so they don't tie up tokio worker threads.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

use crate::proto::{Request, Response};
use crate::storage;

/// Bind to `socket_path` (cleaning a stale file if no live daemon owns it)
/// and serve connections until the listener errors.
pub async fn serve(socket_path: PathBuf, db_path: PathBuf) -> Result<()> {
    bind_clean(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), "ipc listening");

    loop {
        let (stream, _addr) = listener.accept().await.context("accept")?;
        let db_path = db_path.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, db_path).await {
                warn!(error = %e, "ipc connection error");
            }
        });
    }
}

/// If `path` exists, probe it; if no daemon answers, unlink. If a daemon does
/// answer, refuse to start (the caller should not race with another hugind).
fn bind_clean(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "another hugind appears to be running at {}",
            path.display()
        ),
        Err(_) => std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display())),
    }
}

async fn handle_connection(stream: UnixStream, db_path: PathBuf) -> Result<()> {
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
        dispatch(req, &db_path, &mut wr).await?;
    }
}

async fn dispatch<W: AsyncWriteExt + Unpin>(
    req: Request,
    db_path: &Path,
    wr: &mut W,
) -> Result<()> {
    match req {
        Request::Ping => write_response(wr, &Response::Ok).await,
        Request::List { limit, selection } => {
            let db = db_path.to_owned();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let conn = Connection::open(&db)?;
                storage::list(&conn, limit.unwrap_or(50), selection.as_deref())
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
        Request::ReadBlob { id, mime } => {
            let db = db_path.to_owned();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let conn = Connection::open(&db)?;
                storage::read_blob(&conn, id, mime.as_deref())
            })
            .await?;
            match result {
                Ok(Some((mime, blob))) => {
                    write_response(
                        wr,
                        &Response::BlobHeader {
                            mime,
                            len: blob.len(),
                        },
                    )
                    .await?;
                    wr.write_all(&blob).await?;
                    wr.flush().await?;
                    Ok(())
                }
                Ok(None) => write_error(wr, format!("no blob for id {id}")).await,
                Err(e) => write_error(wr, e.to_string()).await,
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
