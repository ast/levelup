//! Mine file frecency from munin's command history.
//!
//! Sleipnir owns directory frecency (the chpwd hook). For *files* it leans on
//! its sibling: munin already records every command you run, with the `cwd` it
//! ran in. We scan recent commands, pull out the arguments that resolve to
//! real files, and rank them by how often / how recently they appeared. The
//! result is materialised into Sleipnir's `files` table so the picker reads
//! only local rows.
//!
//! munin's DB is opened read-only (WAL makes the concurrent read safe). If it
//! isn't there — munin not installed, or never run — we degrade silently to
//! "dirs only".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use tracing::debug;

use crate::storage;
use crate::util::home_dir;

/// Re-mine files at most this often. `add` runs on every chpwd, so without a
/// gate we'd stat thousands of paths on every `cd`; behind the TTL the cost
/// lands on roughly one `cd` per interval and the rest stay ~5 ms.
const SYNC_TTL_SECS: i64 = 300;

/// How many recent commands to scan. Bounds the stat work and keeps the file
/// pool to things you've touched lately.
const SCAN_LIMIT: usize = 5_000;

const LAST_SYNC_KEY: &str = "last_file_sync";

/// Default munin DB path: `$XDG_DATA_HOME/munin/munin.db` (falling back to
/// `$HOME/.local/share/munin/munin.db`). Matches `munin::storage::default_db_path`.
pub fn default_munin_db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("munin").join("munin.db"))
}

/// Refresh the file pool if it's never been synced or the TTL has lapsed. Runs
/// from `add` (chpwd). Best-effort: any failure reading munin is swallowed (and
/// the TTL clock is reset so we back off) so a missing/locked sibling DB never
/// breaks `cd`.
pub fn sync_if_stale(conn: &mut Connection, now: i64) -> Result<()> {
    let last: Option<i64> = storage::get_meta(conn, LAST_SYNC_KEY)?.and_then(|s| s.parse().ok());
    let due = match last {
        None => true,
        Some(t) => now.saturating_sub(t) >= SYNC_TTL_SECS,
    };
    if !due {
        return Ok(());
    }
    match mine_and_store(conn, now) {
        Ok(count) => debug!(files = count, "file pool synced from munin"),
        Err(e) => {
            // Degrade to "keep the existing pool" rather than failing the cd,
            // and stamp the clock so we don't retry on every subsequent cd.
            debug!(error = %e, "munin file mining failed; backing off");
            let _ = storage::set_meta(conn, LAST_SYNC_KEY, &now.to_string());
        }
    }
    Ok(())
}

/// Force a refresh now, ignoring the TTL. Propagates errors (used by the `sync`
/// debug subcommand, where the user wants to see what went wrong).
pub fn sync_now(conn: &mut Connection, now: i64) -> Result<usize> {
    mine_and_store(conn, now)
}

/// Mine → materialise → housekeep → stamp the sync clock. The shared body of
/// both the gated and forced paths.
fn mine_and_store(conn: &mut Connection, now: i64) -> Result<usize> {
    let files = match default_munin_db_path() {
        Some(db) => mine_files(&db)?,
        None => Vec::new(),
    };
    let n = files.len();
    storage::replace_files(conn, &files)?;
    storage::maintain_dirs(conn)?;
    storage::set_meta(conn, LAST_SYNC_KEY, &now.to_string())?;
    Ok(n)
}

/// Scan munin's recent commands and return `(abs_path, rank, last_access)` for
/// every argument that resolves to an existing file. `rank` is the appearance
/// count; `last_access` is the most recent appearance (seconds).
fn mine_files(munin_db: &Path) -> Result<Vec<(String, f64, i64)>> {
    if !munin_db.exists() {
        return Ok(Vec::new());
    }
    let src = Connection::open_with_flags(
        munin_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open munin db {}", munin_db.display()))?;

    let mut stmt =
        src.prepare("SELECT cmd, cwd, ts_unix_ns FROM entries ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([SCAN_LIMIT as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;

    let home = home_dir();
    let home = home.as_deref();
    // path → (count, max_ts_secs)
    let mut acc: HashMap<String, (f64, i64)> = HashMap::new();

    for row in rows {
        let (cmd, cwd, ts_ns) = row?;
        let ts_secs = ts_ns / 1_000_000_000;
        let Some(tokens) = shlex::split(&cmd) else {
            continue; // malformed quoting — skip the whole command
        };
        // Skip argv0 (the command name itself is not a file argument).
        for tok in tokens.iter().skip(1) {
            if !looks_like_path_arg(tok) {
                continue;
            }
            if let Some(abs) = resolve_file(tok, cwd.as_deref(), home) {
                let e = acc.entry(abs).or_insert((0.0, 0));
                e.0 += 1.0;
                e.1 = e.1.max(ts_secs);
            }
        }
    }

    Ok(acc.into_iter().map(|(p, (c, t))| (p, c, t)).collect())
}

/// Cheap pre-filter before the (relatively expensive) `canonicalize`. Drops
/// flags and tokens we know can't be a usable file path.
fn looks_like_path_arg(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    if tok.starts_with('-') {
        return false; // flag
    }
    if tok.contains('$') || tok.contains('*') || tok.contains('?') {
        return false; // unexpanded var / glob — can't resolve reliably
    }
    if tok.contains("://") {
        return false; // URL
    }
    true
}

/// Resolve a token to an absolute, existing **file** path. Expands a leading
/// `~`, joins relatives against the command's recorded `cwd`, and canonicalises
/// (which also proves existence). Returns `None` for non-files (dirs come from
/// the chpwd hook) and anything that doesn't resolve.
fn resolve_file(tok: &str, cwd: Option<&str>, home: Option<&str>) -> Option<String> {
    let expanded: PathBuf = if tok == "~" {
        PathBuf::from(home?)
    } else if let Some(rest) = tok.strip_prefix("~/") {
        Path::new(home?).join(rest)
    } else {
        PathBuf::from(tok)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        Path::new(cwd?).join(expanded)
    };
    let canon = std::fs::canonicalize(abs).ok()?;
    if canon.is_file() {
        Some(canon.to_string_lossy().into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_arg_prefilter() {
        assert!(looks_like_path_arg("README.md"));
        assert!(looks_like_path_arg("src/main.rs"));
        assert!(looks_like_path_arg("~/notes.txt"));
        assert!(!looks_like_path_arg("-v"));
        assert!(!looks_like_path_arg("--long"));
        assert!(!looks_like_path_arg("$HOME/x"));
        assert!(!looks_like_path_arg("*.rs"));
        assert!(!looks_like_path_arg("https://example.com"));
        assert!(!looks_like_path_arg(""));
    }

    #[test]
    fn resolve_real_file_relative_to_cwd() {
        // Use this very source file as a known-existing target.
        let dir = env!("CARGO_MANIFEST_DIR");
        let got = resolve_file("src/munin_import.rs", Some(dir), None)
            .expect("should resolve relative to cwd");
        assert!(got.ends_with("munin_import.rs"));
        // A directory must NOT resolve as a file (dirs come from chpwd).
        assert_eq!(resolve_file("src", Some(dir), None), None);
        // Nonexistent file → None.
        assert_eq!(resolve_file("nope_xyz.txt", Some(dir), None), None);
    }
}
