//! `sleipnir` — a modeless frecency navigator.
//!
//! One binary, no daemon. Subcommands:
//! - `add -- PATH` — record a directory visit (the chpwd hook calls this); also
//!   refreshes the munin-mined file pool behind a TTL.
//! - `pick [QUERY]` — open the picker; print `<action>\t<path>` for the shell
//!   widget, or exit 1 on cancel.
//! - `init <shell>` — print the shell hook.
//! - `query [QUERY]` — non-interactive ranked dump (debugging / tests).
//! - `sync` — force a file-pool refresh from munin (debugging).

mod config;
mod highlight;
mod munin_import;
mod shells;
mod storage;
mod tui;
mod util;
mod walk;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::shells::Shell;
use crate::storage::{Kind, Row, default_db_path};
use crate::tui::Outcome;
use crate::util::{init_tracing, now_unix_secs};

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(
    name = "sleipnir",
    version = VERSION,
    about = "Modeless frecency navigator — jump to the dirs and files you actually use"
)]
struct Cli {
    /// Override the SQLite database path. Default: $XDG_DATA_HOME/sleipnir/sleipnir.db.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record a directory visit (used by the chpwd hook). Writes straight to
    /// SQLite; also refreshes the file pool mined from munin behind a TTL.
    Add {
        /// The directory just entered (typically `$PWD`).
        path: String,
    },
    /// Open the interactive picker seeded with QUERY. On Enter/Tab, prints a
    /// `<action>\t<path>` line to stdout; exits 1 on cancel.
    Pick {
        /// Initial query (the shell widget passes the current line buffer).
        query: Vec<String>,
    },
    /// Print the shell-hook script for SHELL. Wire up with
    /// `eval "$(sleipnir init zsh)"` in your rc file.
    Init {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Non-interactive ranked dump of the pool (for debugging / tests).
    Query {
        query: Vec<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Force a refresh of the file pool mined from munin's history.
    Sync,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { shell } => {
            print!("{}", shells::init_script(shell));
            Ok(())
        }
        Cmd::Add { path } => {
            init_tracing();
            run_add(&path, cli.db.as_deref())
        }
        Cmd::Pick { query } => run_pick(query.join(" "), cli.db.as_deref()),
        Cmd::Query { query, limit } => run_query(&query.join(" "), limit, cli.db.as_deref()),
        Cmd::Sync => {
            init_tracing();
            run_sync(cli.db.as_deref())
        }
    }
}

fn resolve_db_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => default_db_path(),
    }
}

/// Canonicalise (resolve symlinks → a stable identity for dedup), falling back
/// to the raw path if it can't be resolved (e.g. just deleted).
fn canonicalize_or(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

fn run_add(path: &str, db_override: Option<&Path>) -> Result<()> {
    let db_path = resolve_db_path(db_override)?;
    let mut conn = storage::open(&db_path)?;
    let now = now_unix_secs();
    storage::add_dir(&conn, &canonicalize_or(path), now)?;
    // Opportunistically refresh the file pool (TTL-gated inside). Best-effort:
    // a slow/missing munin DB must never make `cd` fail.
    if let Err(e) = munin_import::sync_if_stale(&mut conn, now) {
        tracing::debug!(error = %e, "file sync skipped");
    }
    Ok(())
}

/// Build the picker's pool: frecency rows (chpwd dirs + munin-mined files,
/// anywhere) fused with the live walk under the current locus (completeness,
/// mtime-ranked). Both `pick` and `query` use this so they show the same thing.
fn build_pool(db_override: Option<&Path>, now: i64) -> Result<Vec<Row>> {
    let db_path = resolve_db_path(db_override)?;
    let conn = storage::open(&db_path)?;
    let frecency_rows = storage::load_pool(&conn, now)?;
    drop(conn);
    let walk_rows = std::env::current_dir()
        .ok()
        .map(|cwd| walk::walk_pool(&cwd, now))
        .unwrap_or_default();
    Ok(walk::fuse(frecency_rows, walk_rows))
}

fn run_pick(query: String, db_override: Option<&Path>) -> Result<()> {
    let cfg = config::load_or_default();
    let pool = build_pool(db_override, now_unix_secs())?;
    match tui::run(pool, query, &cfg)? {
        Outcome::Cd(p) => {
            println!("cd\t{p}");
            Ok(())
        }
        Outcome::Insert(p) => {
            println!("insert\t{p}");
            Ok(())
        }
        Outcome::Cancel => std::process::exit(1),
    }
}

fn run_query(query: &str, limit: usize, db_override: Option<&Path>) -> Result<()> {
    let pool = build_pool(db_override, now_unix_secs())?;
    let ranked = storage::rank_pool(pool, query);
    for row in ranked.iter().take(limit) {
        let kind = match row.kind {
            Kind::Dir => "dir",
            Kind::File => "file",
        };
        println!("{kind}\t{:.2}\t{}", row.frecency, row.display);
    }
    Ok(())
}

fn run_sync(db_override: Option<&Path>) -> Result<()> {
    let db_path = resolve_db_path(db_override)?;
    let mut conn = storage::open(&db_path)?;
    let n =
        munin_import::sync_now(&mut conn, now_unix_secs()).context("sync file pool from munin")?;
    println!("synced {n} files from munin");
    Ok(())
}
