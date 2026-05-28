use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{debug, info, warn};

use crate::{is_text_mime, CapturedEntry};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id          INTEGER PRIMARY KEY,
    ts_unix_ns  INTEGER NOT NULL,
    selection   TEXT NOT NULL,
    hash        BLOB NOT NULL,
    size_bytes  INTEGER NOT NULL,
    preview     TEXT
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
";

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
        let conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .context("set pragmas")?;
        conn.execute_batch(SCHEMA_SQL).context("apply schema")?;
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
        let preview = make_preview(&entry.parts);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO entries (ts_unix_ns, selection, hash, size_bytes, preview)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.ts_unix_ns,
                entry.selection.as_str(),
                hash.as_bytes(),
                total_size,
                preview,
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

fn make_preview(parts: &[(String, Vec<u8>)]) -> Option<String> {
    parts.iter().find_map(|(mime, blob)| {
        if !is_text_mime(mime) {
            return None;
        }
        let s = std::str::from_utf8(blob).ok()?;
        Some(s.chars().take(200).collect::<String>())
    })
}

pub fn default_db_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("hugin").join("hugin.db"))
}

pub fn run_storage_thread(
    path: PathBuf,
    rx: mpsc::Receiver<CapturedEntry>,
    cfg: RetentionConfig,
) -> Result<()> {
    let mut store = Store::open(&path)?;
    info!(db = %path.display(), "storage ready");
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
    Ok(())
}

/// Spawn the storage worker on a named thread. Errors inside the worker are
/// logged; the returned handle resolves once the channel is closed.
pub fn spawn_storage_thread(
    path: PathBuf,
    rx: mpsc::Receiver<CapturedEntry>,
    cfg: RetentionConfig,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("hugin-storage".into())
        .spawn(move || {
            if let Err(e) = run_storage_thread(path, rx, cfg) {
                tracing::warn!(error = %e, "storage thread terminated with error");
            }
        })
        .context("spawn storage thread")
}
