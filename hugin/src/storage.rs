use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, info, warn};

use crate::proto::{EntryMeta, SearchSort};
use crate::{CapturedEntry, is_text_mime};

/// Schema version stored in `PRAGMA user_version`. Bump when the on-disk
/// shape changes incompatibly; `Store::open` refuses to start on mismatch.
const DB_VERSION: i32 = 2;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id          INTEGER PRIMARY KEY,
    ts_unix_ns  INTEGER NOT NULL,
    selection   TEXT NOT NULL,
    hash        BLOB NOT NULL,
    size_bytes  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_hash_idx ON entries(hash);
CREATE INDEX IF NOT EXISTS entries_ts_idx ON entries(ts_unix_ns);
CREATE INDEX IF NOT EXISTS entries_sel_id_idx ON entries(selection, id);

CREATE TABLE IF NOT EXISTS mime_parts (
    entry_id    INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    mime        TEXT NOT NULL,
    blob        BLOB NOT NULL,
    PRIMARY KEY (entry_id, mime)
);

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(content);

CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    DELETE FROM entries_fts WHERE rowid = OLD.id;
END;
";

/// Upper bound on `limit` for list/search queries. Caps the result set so
/// an IPC peer can't force the daemon to materialise the entire table — and
/// in particular guards against `usize::MAX as i64 == -1`, which SQLite
/// would otherwise interpret as "no limit". Matches the default retention
/// cap so a fully-populated DB is still listable.
const MAX_LIMIT: usize = 10_000;

// SQL strings are static so list/get/search don't rebuild them per call.
// The `200` is the leading-substr snippet width for list/get; the
// `‹›/…/16` arguments to `snippet()` are the FTS5 highlight markers and
// token-count budget. Keep both in step if you tune one.

const LIST_SQL: &str = "\
    SELECT e.id, e.ts_unix_ns, e.selection, e.size_bytes, \
           substr(f.content, 1, 200) \
    FROM entries e LEFT JOIN entries_fts f ON f.rowid = e.id \
    WHERE (?1 IS NULL OR e.selection = ?1) \
    ORDER BY e.id DESC LIMIT ?2";

const GET_SQL: &str = "\
    SELECT e.id, e.ts_unix_ns, e.selection, e.size_bytes, \
           substr(f.content, 1, 200) \
    FROM entries e LEFT JOIN entries_fts f ON f.rowid = e.id \
    WHERE e.id = ?1";

const SEARCH_SQL_RELEVANCE: &str = "\
    SELECT e.id, e.ts_unix_ns, e.selection, e.size_bytes, \
           snippet(entries_fts, 0, '‹', '›', '…', 16) \
    FROM entries_fts \
    JOIN entries e ON e.id = entries_fts.rowid \
    WHERE entries_fts MATCH ?1 AND (?2 IS NULL OR e.selection = ?2) \
    ORDER BY bm25(entries_fts) ASC, e.id DESC LIMIT ?3";

const SEARCH_SQL_RECENT: &str = "\
    SELECT e.id, e.ts_unix_ns, e.selection, e.size_bytes, \
           snippet(entries_fts, 0, '‹', '›', '…', 16) \
    FROM entries_fts \
    JOIN entries e ON e.id = entries_fts.rowid \
    WHERE entries_fts MATCH ?1 AND (?2 IS NULL OR e.selection = ?2) \
    ORDER BY e.id DESC LIMIT ?3";

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub max_entries: usize,
    pub max_age_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_age_days: 90,
        }
    }
}

pub struct Store {
    conn: Connection,
    last_vacuum: Instant,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create data dir {parent:?}"))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
        // Version check runs before any pragmas: WAL leaves -wal/-shm sidecars
        // on disk, and we don't want to scatter them around a DB we're about
        // to refuse.
        ensure_compatible_schema(&conn, path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .context("set pragmas")?;
        // Schema apply and the version stamp must be atomic. Otherwise a
        // crash between CREATE TABLE entries and the user_version write leaves
        // a v0 DB with an entries table — which ensure_compatible_schema
        // (correctly, given no other signal) rejects as pre-FTS, asking the
        // user to delete a valid-but-half-stamped file.
        let tx = conn.transaction().context("begin schema tx")?;
        tx.execute_batch(SCHEMA_SQL).context("apply schema")?;
        tx.pragma_update(None, "user_version", DB_VERSION)
            .context("set schema version")?;
        tx.commit().context("commit schema tx")?;
        // Force retention to run on the first capture by setting last_vacuum into the past.
        Ok(Self {
            conn,
            last_vacuum: Instant::now()
                .checked_sub(Duration::from_secs(7200))
                .unwrap_or_else(Instant::now),
        })
    }

    /// Insert a capture, deduping against the most recent entry for the same selection.
    /// Returns the new row id, or `None` if the capture was a duplicate.
    pub fn insert(&mut self, entry: &CapturedEntry) -> Result<Option<i64>> {
        let hash = canonical_hash(&entry.parts);
        let prev_hash: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT hash FROM entries WHERE selection = ?1 ORDER BY id DESC LIMIT 1",
                params![entry.selection.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if prev_hash.as_deref() == Some(hash.as_bytes()) {
            return Ok(None);
        }
        let total_size: i64 = entry.parts.iter().map(|(_, b)| b.len() as i64).sum();
        let indexable = pick_indexable_text(&entry.parts);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO entries (ts_unix_ns, selection, hash, size_bytes)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.ts_unix_ns,
                entry.selection.as_str(),
                hash.as_bytes(),
                total_size,
            ],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut stmt =
                tx.prepare("INSERT INTO mime_parts (entry_id, mime, blob) VALUES (?1, ?2, ?3)")?;
            for (mime, blob) in &entry.parts {
                stmt.execute(params![id, mime, blob])?;
            }
        }
        if let Some(text) = indexable {
            tx.execute(
                "INSERT INTO entries_fts (rowid, content) VALUES (?1, ?2)",
                params![id, text],
            )?;
        }
        tx.commit()?;
        Ok(Some(id))
    }

    pub fn maybe_retain(&mut self, cfg: &RetentionConfig) -> Result<usize> {
        if self.last_vacuum.elapsed() < Duration::from_secs(3600) {
            return Ok(0);
        }
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(i64::MAX);
        let cutoff = now_ns - (cfg.max_age_days as i64) * 86_400 * 1_000_000_000;
        let by_age = self
            .conn
            .execute("DELETE FROM entries WHERE ts_unix_ns < ?1", params![cutoff])?;
        let by_count = self.conn.execute(
            "DELETE FROM entries WHERE id IN (
                SELECT id FROM entries ORDER BY id DESC LIMIT -1 OFFSET ?1
             )",
            params![cfg.max_entries as i64],
        )?;
        self.last_vacuum = Instant::now();
        Ok(by_age + by_count)
    }
}

/// Refuse to open a database produced by a different schema generation.
/// Fresh DBs (no `entries` table yet) pass through; otherwise the
/// `user_version` pragma must match `DB_VERSION` *and* the FTS virtual table
/// must be present. Pre-FTS DBs predate the versioning scheme and sit at
/// `user_version = 0` even though their schema is incompatible — we detect
/// them by the presence of `entries`.
fn ensure_compatible_schema(conn: &Connection, path: &Path) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == DB_VERSION {
        if !table_exists(conn, "entries_fts")? {
            bail!(
                "hugin database at {} is at schema v{version} but entries_fts is missing; \
                 the search index has been dropped out-of-band. Delete the file and restart \
                 to recreate (or rebuild entries_fts manually if you need to keep the data).",
                path.display(),
            );
        }
        return Ok(());
    }
    if version == 0 && !table_exists(conn, "entries")? {
        return Ok(());
    }
    bail!(
        "incompatible hugin database at {}: schema v{version}, daemon expects v{DB_VERSION}. \
         No automatic migration; delete the file and restart to recreate.",
        path.display(),
    );
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            params![name],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

fn canonical_hash(parts: &[(String, Vec<u8>)]) -> blake3::Hash {
    let mut sorted: Vec<&(String, Vec<u8>)> = parts.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    for (mime, blob) in sorted {
        hasher.update(&(mime.len() as u32).to_le_bytes());
        hasher.update(mime.as_bytes());
        hasher.update(&(blob.len() as u64).to_le_bytes());
        hasher.update(blob);
    }
    hasher.finalize()
}

/// Choose the text payload that goes into the FTS index. Returns `None`
/// for image-only / binary entries — those are stored intact but aren't
/// searchable. Walks `parts` once, keeping the best candidate by tier:
///   3. `text/plain` (any charset) that decodes as valid UTF-8
///   2. any other text-MIME that decodes as valid UTF-8
///   1. any text-MIME via lossy decode (e.g. X11 STRING atoms shipping
///      Latin-1) — non-UTF-8 bytes become U+FFFD so the text is still
///      searchable rather than silently absent from the index.
fn pick_indexable_text(parts: &[(String, Vec<u8>)]) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    for (mime, blob) in parts {
        if !is_text_mime(mime) {
            continue;
        }
        let base = mime.split(';').next().unwrap_or(mime).trim();
        let is_plain = base.eq_ignore_ascii_case("text/plain");
        let (tier, text) = match std::str::from_utf8(blob) {
            Ok(s) => (if is_plain { 3u8 } else { 2 }, s.to_owned()),
            Err(_) => (1, String::from_utf8_lossy(blob).into_owned()),
        };
        if best.as_ref().is_none_or(|(b, _)| *b < tier) {
            best = Some((tier, text));
            if tier == 3 {
                break;
            }
        }
    }
    best.map(|(_, s)| s)
}

pub fn default_db_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("hugin").join("hugin.db"))
}

pub fn run_storage_thread(
    mut store: Store,
    rx: mpsc::Receiver<CapturedEntry>,
    cfg: RetentionConfig,
) {
    info!("storage ready");
    let _ = store.maybe_retain(&cfg);
    for entry in rx {
        let mimes: Vec<&str> = entry.parts.iter().map(|(m, _)| m.as_str()).collect();
        let total_size: usize = entry.parts.iter().map(|(_, b)| b.len()).sum();
        match store.insert(&entry) {
            Ok(Some(id)) => info!(
                id,
                sel = entry.selection.as_str(),
                parts = entry.parts.len(),
                size = total_size,
                mimes = ?mimes,
                "stored"
            ),
            Ok(None) => debug!(
                sel = entry.selection.as_str(),
                "dedup: identical to previous entry"
            ),
            Err(e) => warn!(error = %e, sel = entry.selection.as_str(), "insert failed"),
        }
        if let Err(e) = store.maybe_retain(&cfg) {
            warn!(error = %e, "retention sweep failed");
        }
    }
    info!("storage thread exiting (channel closed)");
}

/// Return up to `limit` most-recent entries (newest first), optionally
/// filtered to a single selection (`"regular"` or `"primary"`). The snippet
/// is the leading 200 chars of the FTS-indexed text, or `None` for entries
/// with no indexable text payload. `limit` is clamped to `MAX_LIMIT`.
pub fn list(conn: &Connection, limit: usize, selection: Option<&str>) -> Result<Vec<EntryMeta>> {
    let limit = limit.min(MAX_LIMIT) as i64;
    let mut stmt = conn.prepare(LIST_SQL)?;
    let rows: Vec<RowTuple> = stmt
        .query_map(params![selection, limit], row_to_tuple)?
        .collect::<rusqlite::Result<_>>()?;
    attach_mimes(conn, rows)
}

/// Full-text search of indexed clipboard entries. `query` is treated as a
/// single phrase unless `raw` is set, in which case it's passed through to
/// FTS5 verbatim. `selection` filters by `"regular"` / `"primary"`. The
/// snippet field contains `snippet(...)` excerpts with match terms wrapped
/// in `‹›` and elided context shown as `…`. Empty/whitespace queries
/// short-circuit to an empty result; `limit` is clamped to `MAX_LIMIT`.
pub fn search(
    conn: &Connection,
    query: &str,
    raw: bool,
    sort: SearchSort,
    limit: usize,
    selection: Option<&str>,
) -> Result<Vec<EntryMeta>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = if raw {
        query.to_owned()
    } else {
        // FTS5 phrase literal: wrap in double quotes, doubling embedded ones.
        format!("\"{}\"", query.replace('"', "\"\""))
    };
    let limit = limit.min(MAX_LIMIT) as i64;
    let sql = match sort {
        SearchSort::Relevance => SEARCH_SQL_RELEVANCE,
        SearchSort::Recent => SEARCH_SQL_RECENT,
    };
    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<RowTuple> = stmt
        .query_map(params![&match_expr, selection, limit], row_to_tuple)?
        .collect::<rusqlite::Result<_>>()?;
    attach_mimes(conn, rows)
}

/// Tuple shape returned by row_to_tuple — (id, ts_unix_ns, selection, size_bytes, snippet).
type RowTuple = (i64, i64, String, i64, Option<String>);

/// Populate the mimes field on each entry with a single batched
/// `WHERE entry_id IN (...)` query, then fan out via a HashMap. The
/// previous per-row mime SELECT was an N+1 pattern that ran one
/// statement per result row.
fn attach_mimes(conn: &Connection, rows: Vec<RowTuple>) -> Result<Vec<EntryMeta>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT entry_id, mime FROM mime_parts \
         WHERE entry_id IN ({placeholders}) \
         ORDER BY entry_id, mime",
    );
    let mut stmt = conn.prepare(&sql)?;
    let pairs = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut mimes_by_id: HashMap<i64, Vec<String>> = HashMap::with_capacity(ids.len());
    for pair in pairs {
        let (id, mime) = pair?;
        mimes_by_id.entry(id).or_default().push(mime);
    }
    Ok(rows
        .into_iter()
        .map(|(id, ts_unix_ns, selection, size_bytes, snippet)| EntryMeta {
            id,
            ts_unix_ns,
            selection,
            mimes: mimes_by_id.remove(&id).unwrap_or_default(),
            size_bytes,
            snippet,
        })
        .collect())
}

/// Metadata for a single entry by id, or `None` if not found.
pub fn get(conn: &Connection, id: i64) -> Result<Option<EntryMeta>> {
    let row = conn.query_row(GET_SQL, params![id], row_to_tuple).optional()?;
    let Some(tuple) = row else {
        return Ok(None);
    };
    Ok(attach_mimes(conn, vec![tuple])?.into_iter().next())
}

/// Read the raw blob for `(id, mime)`. If `mime` is `None`, picks the first
/// `text/*` MIME, falling back to the first available. Returns the actual
/// MIME alongside the bytes so the caller can report what they got.
pub fn read_blob(
    conn: &Connection,
    id: i64,
    mime: Option<&str>,
) -> Result<Option<(String, Vec<u8>)>> {
    let chosen = match mime {
        Some(m) => m.to_string(),
        None => {
            let mut stmt = conn.prepare(
                "SELECT mime FROM mime_parts WHERE entry_id = ?1 ORDER BY mime",
            )?;
            let mimes: Vec<String> = stmt
                .query_map(params![id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let Some(picked) = mimes
                .iter()
                .find(|m| is_text_mime(m))
                .cloned()
                .or_else(|| mimes.into_iter().next())
            else {
                return Ok(None);
            };
            picked
        }
    };

    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT blob FROM mime_parts WHERE entry_id = ?1 AND mime = ?2",
            params![id, chosen],
            |row| row.get(0),
        )
        .optional()?;
    Ok(blob.map(|b| (chosen, b)))
}

/// Load every (mime, blob) pair for an entry. Returns `None` if the entry
/// id does not exist. Used by `hugin copy` to repopulate the clipboard.
pub fn load_parts(conn: &Connection, id: i64) -> Result<Option<Vec<(String, Vec<u8>)>>> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let mut stmt =
        conn.prepare("SELECT mime, blob FROM mime_parts WHERE entry_id = ?1 ORDER BY mime")?;
    let parts: Vec<(String, Vec<u8>)> = stmt
        .query_map(params![id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(parts))
}

fn row_to_tuple(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(i64, i64, String, i64, Option<String>)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

/// Spawn the storage worker on a named thread. The store must already be
/// opened by the caller so any schema-version error fails fast on the main
/// thread before the daemon advertises itself as ready.
pub fn spawn_storage_thread(
    store: Store,
    rx: mpsc::Receiver<CapturedEntry>,
    cfg: RetentionConfig,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("hugin-storage".into())
        .spawn(move || run_storage_thread(store, rx, cfg))
        .context("spawn storage thread")
}
