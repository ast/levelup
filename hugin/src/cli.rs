use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use clap_complete::Shell;

use crate::default_socket_path;
use crate::storage::{default_db_path, RetentionConfig};

#[derive(Debug, Parser)]
#[command(name = "hugind", version, about = "Wayland clipboard manager daemon")]
pub struct DaemonArgs {
    /// SQLite database path. Defaults to $XDG_DATA_HOME/hugin/hugin.db.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Unix socket path. Defaults to $XDG_RUNTIME_DIR/hugin.sock.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Maximum number of entries kept; older ones are pruned hourly.
    #[arg(long, default_value_t = 10_000)]
    pub max_entries: usize,

    /// Maximum age in days; older entries are pruned hourly.
    #[arg(long, default_value_t = 90)]
    pub max_age_days: u32,

    /// Also watch the primary selection (text auto-selected by mouse,
    /// pasted via middle-click). Off by default because text-drag selections
    /// in many apps emit a steady stream of new MIMEs that crowd the history.
    #[arg(long)]
    pub primary: bool,

    /// Print a shell-completion script for SHELL to stdout and exit.
    #[arg(long, value_name = "SHELL", value_enum)]
    pub generate_completions: Option<Shell>,
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

    pub fn retention(&self) -> RetentionConfig {
        RetentionConfig {
            max_entries: self.max_entries,
            max_age_days: self.max_age_days,
        }
    }
}
