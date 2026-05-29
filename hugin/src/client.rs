//! Minimal synchronous client for the daemon's unix socket.
//!
//! The interactive picker reads history straight from SQLite, but the two
//! *write* actions it offers — putting an entry back on the clipboard
//! (`Copy`) and removing one (`Delete`) — can only be done by the daemon,
//! which owns the wayland connection and the storage thread. Those go through
//! this client. If the daemon is down, `connect` fails and the picker reports
//! it in the status line instead of crashing.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::proto::{Request, Response};

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connect to hugin daemon at {}", path.display()))?;
        let reader = BufReader::new(stream.try_clone().context("clone socket")?);
        Ok(Self {
            reader,
            writer: stream,
        })
    }

    /// Send a request and read one JSON-line response. Not for `ReadBlob`,
    /// which has a raw-bytes trailer — this client only speaks the
    /// control-message ops (`Copy`, `Delete`, `Ping`).
    pub fn request(&mut self, req: &Request) -> Result<Response> {
        let mut json = serde_json::to_string(req).context("serialize request")?;
        json.push('\n');
        self.writer
            .write_all(json.as_bytes())
            .context("send request")?;
        self.writer.flush().context("flush socket")?;

        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .context("read response line")?;
        if n == 0 {
            bail!("daemon closed connection without responding");
        }
        serde_json::from_str(line.trim()).context("parse response")
    }

    /// Send a request that should answer with a plain `Ok`. Maps an
    /// `Error` response to an `Err` and any other variant to a protocol error.
    pub fn request_ok(&mut self, req: &Request) -> Result<()> {
        match self.request(req)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(anyhow!("{message}")),
            other => Err(anyhow!("unexpected response: {other:?}")),
        }
    }
}
