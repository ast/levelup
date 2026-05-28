use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::default_socket_path;
use crate::storage::default_db_path;

#[derive(Debug, Parser)]
#[command(name = "munind", version, about = "Shell-history capture daemon")]
pub struct DaemonArgs {
    /// SQLite database path. Defaults to $XDG_DATA_HOME/munin/munin.db.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Unix socket path. Defaults to $XDG_RUNTIME_DIR/munin.sock.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl DaemonArgs {
    pub fn db_path(&self) -> Result<PathBuf> {
        match &self.db {
            Some(p) => Ok(p.clone()),
            None => default_db_path(),
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(default_socket_path)
    }
}
