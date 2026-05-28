use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::storage::{default_db_path, RetentionConfig};

#[derive(Debug, Parser)]
#[command(name = "hugind", version, about = "Wayland clipboard manager daemon")]
pub struct DaemonArgs {
    /// SQLite database path. Defaults to $XDG_DATA_HOME/hugin/hugin.db.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Maximum number of entries kept; older ones are pruned hourly.
    #[arg(long, default_value_t = 10_000)]
    pub max_entries: usize,

    /// Maximum age in days; older entries are pruned hourly.
    #[arg(long, default_value_t = 90)]
    pub max_age_days: u32,
}

impl DaemonArgs {
    pub fn db_path(&self) -> Result<PathBuf> {
        match &self.db {
            Some(p) => Ok(p.clone()),
            None => default_db_path(),
        }
    }

    pub fn retention(&self) -> RetentionConfig {
        RetentionConfig {
            max_entries: self.max_entries,
            max_age_days: self.max_age_days,
        }
    }
}
