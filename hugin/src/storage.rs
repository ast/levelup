use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, info, warn};

use crate::proto::{EntryMeta, SearchSort};
use crate::{CapturedEntry, CapturedPart, PartStore, is_text_mime};

/// Schema version stored in `PRAGMA user_version`. Bump when the on-disk
/// shape changes incompatibly; `Store::open` refuses to start on mismatch.
/// v1 = entries (+ inline `text` column) + mime_parts. (FTS5 was tried at v2
/// and removed once search moved to in-process nucleo fuzzy matching — same
/// trajectory as munin. Dev-stage, so the rollback ships with no migration:
/// the on-disk v2 DB is refused and the user deletes it.)
const DB_VERSION: i32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id          INTEGER PRIMARY KEY,
    ts_unix_ns  INTEGER NOT NULL,
    selection   TEXT NOT NULL,
    hash        BLOB NOT NULL,
    size_bytes  INTEGER NOT NULL,
    text        TEXT
);
CREATE INDEX IF NOT EXISTS entries_hash_idx ON entries(hash);
CREATE INDEX IF NOT EXISTS entries_ts_idx ON entries(ts_unix_ns);
CREATE INDEX IF NOT EXISTS entries_sel_id_idx ON entries(selection, id);

CREATE TABLE IF NOT EXISTS mime_parts (
    entry_id    INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    mime        TEXT NOT NULL,
    blob        BLOB,
    truncated   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (entry_id, mime)
);
";

/// Upper bound on `limit` for list/search queries. Caps the result set so
/// an IPC peer can't force the daemon to materialise the entire table — and
/// in particular guards against `usize::MAX as i64 == -1`, which SQLite
/// would otherwise interpret as "no limit". Matches the default retention
/// cap so a fully-populated DB is still listable.
pub const MAX_LIMIT: usize = 10_000;

/// How much of an entry's indexable `text` we feed to nucleo as a match
/// haystack (and from which we build the highlighted snippet). Clipboard
/// payloads can be megabytes; fuzzy-scoring the whole thing across the
/// candidate pool on every keystroke would be wasteful, and the
/// distinguishing tokens of a clipping are almost always near the front.
const MATCH_PREFIX: usize = 4096;

// SQL strings are static so list/get/search don't rebuild them per call. The
// `200` is the leading-substr snippet width for list/get; search reads the
// full (prefix-capped in Rust) `text` so nucleo can match deeper than the
// 200-char preview.

const LIST_SQL: &str = "\
    SELECT id, ts_unix_ns, selection, size_bytes, substr(text, 1, 200) \
    FROM entries \
    WHERE (?1 IS NULL OR selection = ?1) \
    ORDER BY id DESC LIMIT ?2";

const GET_SQL: &str = "\
    SELECT id, ts_unix_ns, selection, size_bytes, substr(text, 1, 200) \
    FROM entries WHERE id = ?1";

/// Candidate pool for fuzzy search: recent rows with their full indexable
/// text (capped in Rust to `MATCH_PREFIX`). Filtered by selection at the SQL
/// layer; bounded by `MAX_LIMIT` so a huge DB can't blow up the in-memory
/// pool.
const SEARCH_POOL_SQL: &str = "\
    SELECT id, ts_unix_ns, selection, size_bytes, text \
    FROM entries \
    WHERE (?1 IS NULL OR selection = ?1) \
    ORDER BY id DESC LIMIT ?2";

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
        levelup_core::sqlite::ensure_compatible_schema(&conn, path, DB_VERSION, "entries")?;
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
        // Entries with an omitted part can't be deduped by content — their
        // stored bytes are identical (none), so two distinct oversized copies
        // would hash the same and the second would be dropped. Stubs are tiny,
        // so we just skip the dedup check and always insert them.
        let has_omitted = entry.parts.iter().any(|p| p.store == PartStore::Omitted);
        if !has_omitted {
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
        }
        // Stored size = bytes we actually kept (omitted parts contribute 0).
        let total_size: i64 = entry.parts.iter().map(|p| p.blob.len() as i64).sum();
        let indexable = pick_indexable_text(&entry.parts);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO entries (ts_unix_ns, selection, hash, size_bytes, text)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.ts_unix_ns,
                entry.selection.as_str(),
                hash.as_bytes(),
                total_size,
                indexable,
            ],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO mime_parts (entry_id, mime, blob, truncated) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for part in &entry.parts {
                // Omitted parts store a NULL blob — the row records that the
                // MIME existed without keeping its (oversized) bytes.
                let blob: Option<&[u8]> = match part.store {
                    PartStore::Omitted => None,
                    _ => Some(&part.blob),
                };
                stmt.execute(params![id, part.mime, blob, part.store.as_i64()])?;
            }
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

fn canonical_hash(parts: &[CapturedPart]) -> blake3::Hash {
    let mut sorted: Vec<&CapturedPart> = parts.iter().collect();
    sorted.sort_by(|a, b| a.mime.cmp(&b.mime));
    let mut hasher = blake3::Hasher::new();
    for part in sorted {
        hasher.update(&(part.mime.len() as u32).to_le_bytes());
        hasher.update(part.mime.as_bytes());
        hasher.update(&(part.blob.len() as u64).to_le_bytes());
        hasher.update(&part.blob);
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
fn pick_indexable_text(parts: &[CapturedPart]) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    for part in parts {
        let (mime, blob) = (&part.mime, &part.blob);
        // Omitted parts kept no bytes; a truncated text part keeps its prefix,
        // which is still worth indexing.
        if part.store == PartStore::Omitted || !is_text_mime(mime) {
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
    levelup_core::xdg::data_file("hugin", "hugin.db")
}

pub fn run_storage_thread(
    mut store: Store,
    rx: mpsc::Receiver<CapturedEntry>,
    cfg: RetentionConfig,
) {
    info!("storage ready");
    let _ = store.maybe_retain(&cfg);
    for entry in rx {
        let mimes: Vec<&str> = entry.parts.iter().map(|p| p.mime.as_str()).collect();
        let total_size: usize = entry.parts.iter().map(|p| p.blob.len()).sum();
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

/// Fuzzy search over the recent-entries pool using nucleo-matcher
/// (fzf-style subsequence scoring — the same matcher munin uses).
///
/// Empty / whitespace-only queries fall through to `list` (most-recent N, no
/// scoring, no snippets). Non-empty queries pull up to `MAX_LIMIT` candidates
/// via `SEARCH_POOL_SQL` and score each entry's `text` (capped to
/// `MATCH_PREFIX` chars) with nucleo; non-matches and text-less entries
/// (images / binary) are dropped. The snippet is built from the matched
/// codepoint indices with matched chars wrapped in `‹…›`, the same markers
/// the CLI table and the TUI renderer understand. `limit` is clamped to
/// `MAX_LIMIT`.
pub fn search(
    conn: &Connection,
    query: &str,
    sort: SearchSort,
    limit: usize,
    selection: Option<&str>,
) -> Result<Vec<EntryMeta>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return list(conn, limit, selection);
    }

    // Pull the candidate pool with full text (capped per row below). Filters
    // apply at SQL; the pool is bounded by MAX_LIMIT.
    let mut stmt = conn.prepare(SEARCH_POOL_SQL)?;
    let pool: Vec<(RowTuple, Option<String>)> = stmt
        .query_map(params![selection, MAX_LIMIT as i64], |row| {
            let text: Option<String> = row.get(4)?;
            Ok((
                (
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    // snippet is rebuilt below from match indices; seed None.
                    None,
                ),
                text,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut matcher = levelup_core::fuzzy::matcher();
    let pattern = levelup_core::fuzzy::pattern(trimmed);

    let mut hay_buf = Vec::new();
    let mut idx_buf = Vec::new();
    let mut scored: Vec<(u32, RowTuple)> = Vec::with_capacity(pool.len());
    for (mut row, text) in pool {
        let Some(text) = text else { continue };
        // Cap the haystack: distinguishing tokens of a clipping live up front,
        // and unbounded blobs would make per-keystroke scoring expensive.
        let hay = prefix_chars(&text, MATCH_PREFIX);
        idx_buf.clear();
        let haystack = nucleo_matcher::Utf32Str::new(hay, &mut hay_buf);
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut idx_buf) {
            // nucleo doesn't guarantee sorted indices; sort before walking.
            idx_buf.sort_unstable();
            row.4 = Some(levelup_core::fuzzy::highlight_indices(hay, &idx_buf));
            scored.push((score, row));
        }
    }

    match sort {
        SearchSort::Relevance => {
            // Score desc, tiebreak by id desc (newer first).
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.0.cmp(&a.1.0)));
        }
        SearchSort::Recent => {
            scored.sort_by_key(|s| std::cmp::Reverse(s.1.0));
        }
    }

    let limit = limit.min(MAX_LIMIT);
    let rows: Vec<RowTuple> = scored.into_iter().take(limit).map(|(_, r)| r).collect();
    attach_mimes(conn, rows)
}

/// Borrow the leading `max_chars` codepoints of `s` without allocating,
/// snapping to a char boundary (never splits a multi-byte char).
fn prefix_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Delete an entry (and its `mime_parts`, via the FK cascade). Returns
/// `true` if a row was removed. Foreign keys must be ON for the cascade —
/// callers open the connection with `PRAGMA foreign_keys = ON`.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// Leading text of an entry for the picker's preview pane: up to
/// `max_chars` codepoints of the indexable `text`, or `None` for entries
/// with no text payload (images / binary — the caller renders metadata).
pub fn preview_text(conn: &Connection, id: i64, max_chars: usize) -> Result<Option<String>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT substr(text, 1, ?2) FROM entries WHERE id = ?1",
            params![id, max_chars as i64],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(text)
}

/// Tuple shape returned by row_to_tuple — (id, ts_unix_ns, selection, size_bytes, snippet).
type RowTuple = (i64, i64, String, i64, Option<String>);

/// One clipboard MIME variant: the MIME type and its raw bytes. An entry is a
/// `Vec<MimePart>`.
pub type MimePart = (String, Vec<u8>);

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
        "SELECT entry_id, mime, truncated FROM mime_parts \
         WHERE entry_id IN ({placeholders}) \
         ORDER BY entry_id, mime",
    );
    let mut stmt = conn.prepare(&sql)?;
    let triples = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut mimes_by_id: HashMap<i64, Vec<String>> = HashMap::with_capacity(ids.len());
    let mut oversized_by_id: HashMap<i64, bool> = HashMap::with_capacity(ids.len());
    for triple in triples {
        let (id, mime, truncated) = triple?;
        mimes_by_id.entry(id).or_default().push(mime);
        // truncated != 0 → a part was truncated (text) or omitted (binary).
        let flag = oversized_by_id.entry(id).or_insert(false);
        *flag |= truncated != 0;
    }
    Ok(rows
        .into_iter()
        .map(
            |(id, ts_unix_ns, selection, size_bytes, snippet)| EntryMeta {
                id,
                ts_unix_ns,
                selection,
                mimes: mimes_by_id.remove(&id).unwrap_or_default(),
                size_bytes,
                snippet,
                oversized: oversized_by_id.remove(&id).unwrap_or(false),
            },
        )
        .collect())
}

/// Metadata for a single entry by id, or `None` if not found.
pub fn get(conn: &Connection, id: i64) -> Result<Option<EntryMeta>> {
    let row = conn
        .query_row(GET_SQL, params![id], row_to_tuple)
        .optional()?;
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
    // Only parts with a stored blob are servable — omitted (oversized) parts
    // have a NULL blob and can't be re-copied.
    let chosen = match mime {
        Some(m) => m.to_string(),
        None => {
            let mut stmt = conn.prepare(
                "SELECT mime FROM mime_parts WHERE entry_id = ?1 AND blob IS NOT NULL ORDER BY mime",
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
            "SELECT blob FROM mime_parts WHERE entry_id = ?1 AND mime = ?2 AND blob IS NOT NULL",
            params![id, chosen],
            |row| row.get(0),
        )
        .optional()?;
    Ok(blob.map(|b| (chosen, b)))
}

/// Load every servable (mime, blob) pair for an entry. Omitted (oversized)
/// parts have a NULL blob and are skipped — they can't be re-copied. Returns
/// `None` if the entry id does not exist, or `Some(vec![])` if it exists but
/// every part was omitted (a pure stub). Used by `hugin copy`.
pub fn load_parts(conn: &Connection, id: i64) -> Result<Option<Vec<(String, Vec<u8>)>>> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT mime, blob FROM mime_parts WHERE entry_id = ?1 AND blob IS NOT NULL ORDER BY mime",
    )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Selection;

    /// Apply the live schema to an in-memory DB and insert a couple of
    /// entries directly (mirroring `Store::insert`) so we can exercise the
    /// read/search/delete API without the wayland-bound daemon.
    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        // id 1: a text entry.
        conn.execute(
            "INSERT INTO entries (id, ts_unix_ns, selection, hash, size_bytes, text)
             VALUES (1, 100, 'regular', x'00', 11, 'git commit -m')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mime_parts (entry_id, mime, blob) VALUES (1, 'text/plain', x'00')",
            [],
        )
        .unwrap();
        // id 2: an image entry (no indexable text).
        conn.execute(
            "INSERT INTO entries (id, ts_unix_ns, selection, hash, size_bytes, text)
             VALUES (2, 200, 'regular', x'01', 34000, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mime_parts (entry_id, mime, blob) VALUES (2, 'image/png', x'89504e47')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn list_newest_first_with_snippet() {
        let conn = seeded_conn();
        let rows = list(&conn, 50, None).unwrap();
        assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1]);
        // image entry has no text → no snippet; text entry has its substr.
        assert_eq!(rows[0].snippet, None);
        assert_eq!(rows[0].mimes, vec!["image/png"]);
        assert_eq!(rows[1].snippet.as_deref(), Some("git commit -m"));
    }

    #[test]
    fn search_fuzzy_matches_and_highlights() {
        let conn = seeded_conn();
        // fzf-style subsequence: "gcm" hits "git commit -m".
        let rows = search(&conn, "gcm", SearchSort::Relevance, 50, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 1);
        let snippet = rows[0].snippet.as_deref().unwrap();
        assert!(
            snippet.contains('‹') && snippet.contains('›'),
            "got {snippet:?}"
        );
        // The image entry (no text) never matches a non-empty query.
        assert!(
            search(&conn, "png", SearchSort::Relevance, 50, None)
                .unwrap()
                .iter()
                .all(|e| e.id != 2)
        );
    }

    #[test]
    fn search_empty_query_falls_through_to_list() {
        let conn = seeded_conn();
        let rows = search(&conn, "   ", SearchSort::Relevance, 50, None).unwrap();
        assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn delete_cascades_to_mime_parts() {
        let conn = seeded_conn();
        assert!(delete(&conn, 1).unwrap());
        assert!(!delete(&conn, 1).unwrap()); // already gone
        let parts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mime_parts WHERE entry_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parts, 0, "FK cascade should remove orphaned mime_parts");
    }

    #[test]
    fn preview_text_returns_text_or_none() {
        let conn = seeded_conn();
        assert_eq!(
            preview_text(&conn, 1, 100).unwrap().as_deref(),
            Some("git commit -m")
        );
        assert_eq!(preview_text(&conn, 2, 100).unwrap(), None); // image: NULL text
    }

    /// Read-path behaviour for an entry whose oversized binary part was
    /// omitted (NULL blob) and whose text part was truncated to a prefix.
    #[test]
    fn oversized_entry_reads_partial() {
        let conn = seeded_conn();
        // id 3: text/plain truncated (blob present, truncated=1) +
        // image/png omitted (NULL blob, truncated=2).
        conn.execute(
            "INSERT INTO entries (id, ts_unix_ns, selection, hash, size_bytes, text)
             VALUES (3, 300, 'regular', x'02', 5, 'hello')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mime_parts (entry_id, mime, blob, truncated)
             VALUES (3, 'text/plain', x'68656c6c6f', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mime_parts (entry_id, mime, blob, truncated)
             VALUES (3, 'image/png', NULL, 2)",
            [],
        )
        .unwrap();

        // list/get flag it oversized and still list both MIMEs.
        let e = get(&conn, 3).unwrap().unwrap();
        assert!(
            e.oversized,
            "entry with truncated/omitted part is oversized"
        );
        assert_eq!(e.mimes, vec!["image/png", "text/plain"]);

        // The omitted image isn't servable; the truncated text is.
        let parts = load_parts(&conn, 3).unwrap().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "text/plain");
        assert!(read_blob(&conn, 3, Some("image/png")).unwrap().is_none());
        assert_eq!(
            read_blob(&conn, 3, None)
                .unwrap()
                .map(|(m, _)| m)
                .as_deref(),
            Some("text/plain")
        );
    }

    fn temp_store() -> (Store, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "hugin-test-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).unwrap(), path)
    }

    /// Insert-path behaviour: a truncated text part keeps a searchable blob, an
    /// omitted part writes a NULL blob, and an entry containing an omitted part
    /// skips dedup (two identical stubs both persist).
    #[test]
    fn insert_truncated_and_omitted_parts() {
        let (mut store, path) = temp_store();
        let make = || {
            CapturedEntry::now(
                Selection::Regular,
                vec![
                    CapturedPart {
                        mime: "text/plain".into(),
                        blob: b"prefix".to_vec(),
                        store: PartStore::Truncated,
                    },
                    CapturedPart {
                        mime: "image/png".into(),
                        blob: Vec::new(),
                        store: PartStore::Omitted,
                    },
                ],
            )
        };
        let id1 = store.insert(&make()).unwrap().expect("first insert");
        // Same content again: an omitted part forces insert (no dedup).
        let id2 = store.insert(&make()).unwrap().expect("stub is not deduped");
        assert_ne!(id1, id2);

        let conn = Connection::open(&path).unwrap();
        // The text prefix is stored and indexed; the image blob is NULL.
        assert_eq!(
            preview_text(&conn, id1, 100).unwrap().as_deref(),
            Some("prefix")
        );
        let img_blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT blob FROM mime_parts WHERE entry_id = ?1 AND mime = 'image/png'",
                params![id1],
                |r| r.get(0),
            )
            .unwrap();
        assert!(img_blob.is_none(), "omitted part should store NULL blob");
        // Searchable on the kept prefix.
        let hits = search(&conn, "prefix", SearchSort::Relevance, 50, None).unwrap();
        assert!(hits.iter().any(|e| e.id == id1 && e.oversized));

        let _ = std::fs::remove_file(&path);
    }
}
