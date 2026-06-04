//! Storage: Sleipnir's own SQLite frecency DB + the fused pool/scoring.
//!
//! Two content tables, `dirs` and `files`, each holding `(path, rank,
//! last_access)`. `dirs` is fed live by the chpwd hook (`add_dir`); `files`
//! is a *materialised* view derived from munin's command history (see
//! `munin_import`), refreshed behind a TTL so the `pick` hot path only ever
//! reads local tables. The schema-version gate / WAL / busy_timeout discipline
//! is copied verbatim from `munin/src/storage.rs`.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::util::{abbrev_home, home_dir};

/// Schema version stored in `PRAGMA user_version`. Bump on incompatible
/// on-disk changes; `open` refuses to start on mismatch (dev-stage: no
/// migrations, the user deletes the file).
const DB_VERSION: i32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS dirs (
    path        TEXT PRIMARY KEY,
    rank        REAL NOT NULL,
    last_access INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
    path        TEXT PRIMARY KEY,
    rank        REAL NOT NULL,
    last_access INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// When the summed `dirs` rank crosses this, age everything down (zoxide's
/// approach) so a long-lived DB can't grow without bound.
const RANK_CAP: f64 = 9000.0;

/// Whether a pooled row is a directory (Enter → `cd`) or a file (Enter →
/// insert path). The picker shows the two distinctly (Norman signifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

/// One candidate in the picker's fused list.
#[derive(Debug, Clone)]
pub struct Row {
    /// Absolute, real path — what the action (`cd` / insert) uses.
    pub path: String,
    /// Home-abbreviated path — what we match against and show.
    pub display: String,
    pub kind: Kind,
    /// Frecency score (higher = more relevant), used for ordering with no
    /// query and as a tiebreak under a query.
    pub frecency: f64,
    /// `‹…›`-marked version of `display` when a query matched; `None` otherwise.
    pub snippet: Option<String>,
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create data dir {parent:?}"))?;
    }
    let mut conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
    // Version check runs before pragmas so a rejected DB doesn't leave
    // -wal/-shm sidecars scattered around.
    levelup_core::sqlite::ensure_compatible_schema(&conn, path, DB_VERSION, "dirs")?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
        .context("set pragmas")?;
    // Schema apply + version stamp must be atomic (a crash between them would
    // leave a v0 DB with a dirs table, which the gate then rejects).
    let tx = conn.transaction().context("begin schema tx")?;
    tx.execute_batch(SCHEMA_SQL).context("apply schema")?;
    tx.pragma_update(None, "user_version", DB_VERSION)
        .context("set schema version")?;
    tx.commit().context("commit schema tx")?;
    Ok(conn)
}

/// Record a directory visit. First visit seeds rank 1; later visits bump it
/// and refresh recency — the same `(rank, last_access)` shape the file pool
/// uses, so both decay through one `frecency` function.
pub fn add_dir(conn: &Connection, path: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO dirs(path, rank, last_access) VALUES(?1, 1.0, ?2)
         ON CONFLICT(path) DO UPDATE SET rank = rank + 1.0, last_access = ?2",
        params![path, now],
    )?;
    Ok(())
}

/// Replace the materialised file pool wholesale (it's purely derived from
/// munin, so a clean rebuild is correct and simplest). Done in one transaction.
pub fn replace_files(conn: &mut Connection, files: &[(String, f64, i64)]) -> Result<()> {
    let tx = conn.transaction().context("begin files tx")?;
    tx.execute("DELETE FROM files", [])?;
    {
        let mut stmt =
            tx.prepare("INSERT INTO files(path, rank, last_access) VALUES(?1, ?2, ?3)")?;
        for (path, rank, last_access) in files {
            stmt.execute(params![path, rank, last_access])?;
        }
    }
    tx.commit().context("commit files tx")?;
    Ok(())
}

/// zoxide-style frecency: a fixed rank scaled by a recency multiplier that
/// steps down with age. Within the hour counts ×4, within the day ×2, within
/// the week ×0.5, older ×0.25.
pub fn frecency(rank: f64, last_access: i64, now: i64) -> f64 {
    let dx = now.saturating_sub(last_access);
    let mult = if dx < 3_600 {
        4.0
    } else if dx < 86_400 {
        2.0
    } else if dx < 604_800 {
        0.5
    } else {
        0.25
    };
    rank * mult
}

/// Build the fused candidate pool: every live dir and file with its current
/// frecency. Dead paths (deleted since last recorded) are filtered out here
/// via a `stat` — done once at pool build, NOT per keystroke, so scoring stays
/// cheap. `display` is the home-abbreviated path we match and render.
pub fn load_pool(conn: &Connection, now: i64) -> Result<Vec<Row>> {
    let home = home_dir();
    let home = home.as_deref();
    let mut rows = Vec::new();

    let mut dirs = conn.prepare("SELECT path, rank, last_access FROM dirs")?;
    let dir_iter = dirs.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for d in dir_iter {
        let (path, rank, la) = d?;
        if Path::new(&path).is_dir() {
            let display = abbrev_home(&path, home);
            rows.push(Row {
                path,
                display,
                kind: Kind::Dir,
                frecency: frecency(rank, la, now),
                snippet: None,
            });
        }
    }
    drop(dirs);

    let mut files = conn.prepare("SELECT path, rank, last_access FROM files")?;
    let file_iter = files.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for f in file_iter {
        let (path, rank, la) = f?;
        if Path::new(&path).is_file() {
            let display = abbrev_home(&path, home);
            rows.push(Row {
                path,
                display,
                kind: Kind::File,
                frecency: frecency(rank, la, now),
                snippet: None,
            });
        }
    }
    Ok(rows)
}

/// Rank the pool against `query`. Empty query → frecency order (the modeless
/// "just show me where I go" view). Non-empty → nucleo fuzzy match over the
/// (home-abbreviated) path, match score primary, frecency as tiebreak so your
/// most-used match floats up among equally-good fuzzy hits. Non-matches drop.
pub fn rank_pool(mut pool: Vec<Row>, query: &str) -> Vec<Row> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        pool.sort_by(|a, b| {
            b.frecency
                .partial_cmp(&a.frecency)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.display.cmp(&b.display))
        });
        return pool;
    }

    let mut matcher = levelup_core::fuzzy::matcher();
    let pattern = levelup_core::fuzzy::pattern(trimmed);

    let mut hay_buf = Vec::new();
    let mut idx_buf = Vec::new();
    let mut scored: Vec<(u32, Row)> = Vec::with_capacity(pool.len());
    for mut row in pool {
        idx_buf.clear();
        let haystack = nucleo_matcher::Utf32Str::new(&row.display, &mut hay_buf);
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut idx_buf) {
            idx_buf.sort_unstable();
            row.snippet = Some(levelup_core::fuzzy::highlight_indices(
                &row.display,
                &idx_buf,
            ));
            scored.push((score, row));
        }
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            b.1.frecency
                .partial_cmp(&a.1.frecency)
                .unwrap_or(Ordering::Equal)
        })
    });
    scored.into_iter().map(|(_, r)| r).collect()
}

/// Housekeeping run from the (TTL-gated) file sync, off the `pick` hot path:
/// drop dirs whose path no longer exists, then age ranks down if the table has
/// grown past `RANK_CAP` (zoxide's bounded-growth trick).
pub fn maintain_dirs(conn: &Connection) -> Result<()> {
    // Collect dead paths first (can't mutate while iterating the prepared stmt).
    let dead: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM dirs")?;
        let iter = stmt.query_map([], |r| r.get::<_, String>(0))?;
        iter.filter_map(|p| p.ok())
            .filter(|p| !Path::new(p).is_dir())
            .collect()
    };
    for p in &dead {
        conn.execute("DELETE FROM dirs WHERE path = ?1", params![p])?;
    }

    let total: f64 = conn.query_row("SELECT COALESCE(SUM(rank), 0) FROM dirs", [], |r| r.get(0))?;
    if total > RANK_CAP {
        let factor = RANK_CAP / total;
        conn.execute("UPDATE dirs SET rank = rank * ?1", params![factor])?;
        // After scaling, drop entries that have decayed below relevance.
        conn.execute("DELETE FROM dirs WHERE rank < 1.0", [])?;
    }
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value],
    )?;
    Ok(())
}

/// Default DB path: `$XDG_DATA_HOME/sleipnir/sleipnir.db`.
pub fn default_db_path() -> Result<PathBuf> {
    levelup_core::xdg::data_file("sleipnir", "sleipnir.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            TempDb(
                std::env::temp_dir().join(format!("sleipnir-test-{}-{n}.db", std::process::id())),
            )
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    #[test]
    fn add_dir_seeds_then_bumps_rank() {
        let db = TempDb::new();
        let conn = open(db.path()).unwrap();
        add_dir(&conn, "/tmp/x", 100).unwrap();
        add_dir(&conn, "/tmp/x", 200).unwrap();
        let (rank, la): (f64, i64) = conn
            .query_row(
                "SELECT rank, last_access FROM dirs WHERE path = '/tmp/x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rank, 2.0, "two visits → rank 2");
        assert_eq!(la, 200, "last_access refreshed to the newer visit");
    }

    #[test]
    fn frecency_decays_with_age() {
        let now = 1_000_000;
        // Same rank, different recency: the recent one must score higher.
        let recent = frecency(1.0, now - 60, now); // within the hour ×4
        let old = frecency(1.0, now - 10 * 86_400, now); // >week ×0.25
        assert!(recent > old);
        assert_eq!(recent, 4.0);
        assert_eq!(old, 0.25);
    }

    #[test]
    fn empty_query_orders_by_frecency() {
        let pool = vec![
            Row {
                path: "/a".into(),
                display: "/a".into(),
                kind: Kind::Dir,
                frecency: 1.0,
                snippet: None,
            },
            Row {
                path: "/b".into(),
                display: "/b".into(),
                kind: Kind::File,
                frecency: 9.0,
                snippet: None,
            },
        ];
        let ranked = rank_pool(pool, "  ");
        assert_eq!(ranked[0].path, "/b", "highest frecency first");
    }

    #[test]
    fn query_filters_and_highlights() {
        let pool = vec![
            Row {
                path: "/home/a/src/levelup".into(),
                display: "~/src/levelup".into(),
                kind: Kind::Dir,
                frecency: 1.0,
                snippet: None,
            },
            Row {
                path: "/etc/hosts".into(),
                display: "/etc/hosts".into(),
                kind: Kind::File,
                frecency: 1.0,
                snippet: None,
            },
        ];
        let ranked = rank_pool(pool, "levelup");
        assert_eq!(ranked.len(), 1, "non-matching row dropped");
        assert!(ranked[0].snippet.as_deref().unwrap().contains('‹'));
    }
}
