//! Storage layer: SQLite Store + the storage worker thread.
//!
//! `munind`'s `Connection` is owned by exactly one OS thread
//! (`munin-storage`) and reached via `mpsc<StoreCmd>` for the daemon's
//! writes (captures + imports). Reads (`list`/`search`/`get`) are NOT
//! routed through the daemon at all — the CLI opens its own
//! `Connection` directly against the SQLite file (`bin/munin.rs::run_read`)
//! and calls the standalone functions at the bottom of this module. WAL
//! mode makes the concurrent read safe alongside the daemon's writer.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::{NoContext, Timestamp, Uuid};

use crate::proto::{EntryMeta, Filters, SearchSort};

/// Schema version stored in `PRAGMA user_version`. Bump when the on-disk
/// shape changes incompatibly; `Store::open` refuses to start on mismatch.
/// v1 = entries + config. (FTS5 was tried briefly at v2 and removed once
/// search moved to in-process nucleo fuzzy matching.)
const DB_VERSION: i32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id           INTEGER PRIMARY KEY,
    uuid         TEXT NOT NULL UNIQUE,
    client_id    TEXT NOT NULL,
    cmd          TEXT NOT NULL,
    ts_unix_ns   INTEGER NOT NULL,
    cwd          TEXT,
    hostname     TEXT,
    session      TEXT,
    shell        TEXT,
    exit_code    INTEGER,
    duration_ms  INTEGER,
    synced_at    INTEGER
);
CREATE INDEX IF NOT EXISTS entries_ts_idx       ON entries(ts_unix_ns);
CREATE INDEX IF NOT EXISTS entries_session_idx  ON entries(session, ts_unix_ns);
CREATE INDEX IF NOT EXISTS entries_cwd_idx      ON entries(cwd);
CREATE INDEX IF NOT EXISTS entries_unsynced_idx ON entries(synced_at) WHERE synced_at IS NULL;

CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Upper bound on `limit` for list/search queries. Caps the result set so a
/// caller can't force materialising the entire table — and in particular
/// guards against `usize::MAX as i64 == -1`, which SQLite would otherwise
/// interpret as "no limit".
pub const MAX_LIMIT: usize = 10_000;

/// Static SQL for `list` — newest first with the standard filter pattern
/// (`?N IS NULL OR col = ?N`) so we don't have to build SQL dynamically.
const LIST_SQL: &str = "\
    SELECT id, uuid, cmd, ts_unix_ns, cwd, hostname, session, shell, exit_code, duration_ms \
    FROM entries \
    WHERE (?1 IS NULL OR cwd = ?1) \
      AND (?2 IS NULL OR session = ?2) \
      AND (?3 IS NULL OR shell = ?3) \
      AND (?4 IS NULL OR ts_unix_ns >= ?4) \
      AND (?5 IS NULL OR ts_unix_ns <= ?5) \
    ORDER BY id DESC LIMIT ?6";

const GET_SQL: &str = "\
    SELECT id, uuid, cmd, ts_unix_ns, cwd, hostname, session, shell, exit_code, duration_ms \
    FROM entries WHERE id = ?1";

/// Write commands routed through the storage thread.
pub enum StoreCmd {
    AddStart {
        cmd: String,
        session: String,
        ts_unix_ns: i64,
        cwd: Option<String>,
        hostname: Option<String>,
        shell: Option<String>,
    },
    AddEnd {
        session: String,
        exit_code: i32,
        ts_unix_ns: i64,
    },
    Import {
        path: PathBuf,
        /// `"zsh"` / `"bash"` (text history files) or `"atuin"` (SQLite DB).
        source: String,
        reply: oneshot::Sender<Result<usize>>,
    },
}

pub struct Store {
    conn: Connection,
    client_id: String,
    /// `session → (row_id, start_ts_unix_ns)` for open commands awaiting their
    /// matching `AddEnd`. Bounded by the number of live shell sessions on the
    /// machine, so memory is not a concern.
    open: HashMap<String, (i64, i64)>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create data dir {parent:?}"))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
        // Version check runs before pragmas: WAL leaves -wal/-shm sidecars on
        // disk, and we don't want to scatter them around a DB we're about to
        // refuse.
        ensure_compatible_schema(&conn, path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .context("set pragmas")?;
        // Schema apply and the version stamp must be atomic. A crash between
        // CREATE TABLE entries and the user_version write would leave a v0 DB
        // with an entries table, which ensure_compatible_schema then rejects.
        let tx = conn.transaction().context("begin schema tx")?;
        tx.execute_batch(SCHEMA_SQL).context("apply schema")?;
        tx.pragma_update(None, "user_version", DB_VERSION)
            .context("set schema version")?;
        tx.commit().context("commit schema tx")?;

        let client_id = ensure_client_id(&conn)?;

        Ok(Self {
            conn,
            client_id,
            open: HashMap::new(),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Insert a row for the just-started command and remember its row id so
    /// the matching `AddEnd` can close it out. `cmd` is dropped silently if it
    /// begins with whitespace (atuin / `HIST_IGNORE_SPACE` convention).
    pub fn add_start(
        &mut self,
        cmd: &str,
        session: &str,
        ts_unix_ns: i64,
        cwd: Option<&str>,
        hostname: Option<&str>,
        shell: Option<&str>,
    ) -> Result<Option<i64>> {
        if cmd.starts_with(|c: char| c.is_whitespace()) {
            debug!(session, "skip whitespace-prefixed cmd");
            return Ok(None);
        }
        let uuid = Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO entries (uuid, client_id, cmd, ts_unix_ns, cwd, hostname, session, shell)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                uuid,
                self.client_id,
                cmd,
                ts_unix_ns,
                cwd,
                hostname,
                session,
                shell,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.open.insert(session.to_owned(), (id, ts_unix_ns));
        Ok(Some(id))
    }

    /// Close out the most-recent open command for `session`. Returns the row
    /// id that was updated, or `None` if there was no matching open row.
    pub fn add_end(
        &mut self,
        session: &str,
        exit_code: i32,
        ts_unix_ns: i64,
    ) -> Result<Option<i64>> {
        let Some((id, started_at)) = self.open.remove(session) else {
            debug!(session, exit_code, "add-end with no matching open row");
            return Ok(None);
        };
        let duration_ms = ((ts_unix_ns.saturating_sub(started_at)) / 1_000_000).max(0);
        self.conn.execute(
            "UPDATE entries SET exit_code = ?1, duration_ms = ?2 WHERE id = ?3",
            params![exit_code, duration_ms, id],
        )?;
        Ok(Some(id))
    }

    /// Bulk-import from an atuin SQLite database (`history.db`).
    ///
    /// Atuin's id is itself a UUIDv7, so we preserve it as munin's `uuid` —
    /// that makes re-imports idempotent (the UNIQUE constraint on `uuid`
    /// drops dupes via `INSERT OR IGNORE`), and gives us the same identity
    /// space we'd want for sync later. `shell` is set to `"atuin"` so
    /// imported rows can be filtered with `--shell atuin`.
    pub fn import_atuin_db(&mut self, db_path: &Path) -> Result<usize> {
        use rusqlite::OpenFlags;
        let src = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open atuin db {}", db_path.display()))?;

        // Skip soft-deleted rows. ORDER BY timestamp gives us a stable, useful
        // shape but isn't load-bearing — UUIDv7 preserves the time order.
        let mut stmt = src.prepare(
            "SELECT id, command, timestamp, duration, exit, cwd, session, hostname \
             FROM history WHERE deleted_at IS NULL ORDER BY timestamp",
        )?;
        let rows: Vec<AtuinRow> = stmt
            .query_map([], |r| {
                Ok(AtuinRow {
                    uuid: r.get(0)?,
                    cmd: r.get(1)?,
                    ts_unix_ns: r.get(2)?,
                    duration_ns: r.get(3)?,
                    exit_code: r.get(4)?,
                    cwd: r.get(5)?,
                    session: r.get(6)?,
                    hostname: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(src);

        let total = rows.len();
        let tx = self.conn.transaction().context("begin atuin import tx")?;
        let mut inserted = 0usize;
        {
            // INSERT OR IGNORE: a second import of the same atuin DB silently
            // drops dupes (uuid UNIQUE), so the operation is idempotent.
            let mut insert_entry = tx.prepare(
                "INSERT OR IGNORE INTO entries \
                 (uuid, client_id, cmd, ts_unix_ns, cwd, hostname, session, shell, exit_code, duration_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for row in rows {
                let duration_ms = if row.duration_ns > 0 {
                    Some(row.duration_ns / 1_000_000)
                } else {
                    None
                };
                let affected = insert_entry.execute(params![
                    row.uuid,
                    self.client_id,
                    row.cmd,
                    row.ts_unix_ns,
                    row.cwd,
                    row.hostname,
                    row.session,
                    "atuin",
                    row.exit_code,
                    duration_ms,
                ])?;
                if affected > 0 {
                    inserted += 1;
                }
            }
        }
        tx.commit().context("commit atuin import tx")?;
        info!(scanned = total, inserted, "atuin import done");
        Ok(inserted)
    }

    /// Bulk-import a shell-history file. `shell` is recorded on every row
    /// (so imported entries are filterable later) and selects which parser
    /// to use. All inserts happen inside one transaction.
    pub fn import_file(&mut self, path: &Path, shell: &str) -> Result<usize> {
        let entries = match shell {
            "zsh" => parse_zsh_history(path)?,
            "bash" => parse_bash_history(path)?,
            other => bail!("unsupported shell for import: {other}"),
        };
        let hostname = crate::current_hostname();
        let tx = self.conn.transaction().context("begin import tx")?;
        let mut inserted = 0usize;
        {
            let mut insert_entry = tx.prepare(
                "INSERT INTO entries \
                 (uuid, client_id, cmd, ts_unix_ns, hostname, shell, duration_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for e in entries {
                // UUIDv7 carries the entry's own timestamp (not the import
                // time) so imported rows sort next to live captures from
                // the same era. We don't pass cwd/session/exit_code — shell
                // history files don't carry them.
                let uuid = uuid_for_ts(e.ts_unix_ns).to_string();
                insert_entry.execute(params![
                    uuid,
                    self.client_id,
                    e.cmd,
                    e.ts_unix_ns,
                    hostname,
                    shell,
                    e.duration_ms,
                ])?;
                inserted += 1;
            }
        }
        tx.commit().context("commit import tx")?;
        Ok(inserted)
    }
}

/// Read the persisted client id from the `config` table, or generate one
/// (UUIDv4) on first start. The id is stable per machine and gets stamped
/// onto every captured row — sync uses it to attribute history to a host.
fn ensure_client_id(conn: &Connection) -> Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM config WHERE key = 'client_id'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO config (key, value) VALUES ('client_id', ?1)",
        params![id],
    )?;
    info!(client_id = %id, "generated new client_id");
    Ok(id)
}

/// Refuse to open a database produced by a different schema generation.
/// Fresh DBs (no `entries` table yet) pass through unconditionally.
fn ensure_compatible_schema(conn: &Connection, path: &Path) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == DB_VERSION {
        return Ok(());
    }
    if version == 0 && !table_exists(conn, "entries")? {
        return Ok(());
    }
    bail!(
        "incompatible munin database at {}: schema v{version}, daemon expects v{DB_VERSION}. \
         No automatic migration; delete the file and restart to recreate.",
        path.display(),
    );
}

/// Build a UUIDv7 whose embedded timestamp is `ts_unix_ns` (instead of the
/// wall-clock-now used by `Uuid::now_v7()`). Used by `import_file` so the
/// imported row's uuid sorts next to live captures from the same era.
/// Negative `ts_unix_ns` (shouldn't happen — captures and synthesised
/// timestamps are non-negative) clamps to the epoch.
fn uuid_for_ts(ts_unix_ns: i64) -> Uuid {
    let ns = ts_unix_ns.max(0);
    let secs = (ns / 1_000_000_000) as u64;
    let subsec_nanos = (ns % 1_000_000_000) as u32;
    Uuid::new_v7(Timestamp::from_unix(NoContext, secs, subsec_nanos))
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

pub fn default_db_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("munin").join("munin.db"))
}

pub fn run_storage_thread(mut store: Store, rx: mpsc::Receiver<StoreCmd>) {
    info!(client_id = %store.client_id(), "storage ready");
    for cmd in rx {
        match cmd {
            StoreCmd::AddStart {
                cmd,
                session,
                ts_unix_ns,
                cwd,
                hostname,
                shell,
            } => match store.add_start(
                &cmd,
                &session,
                ts_unix_ns,
                cwd.as_deref(),
                hostname.as_deref(),
                shell.as_deref(),
            ) {
                Ok(Some(id)) => info!(id, session, cmd, "add-start"),
                Ok(None) => {}
                Err(e) => warn!(error = %e, session, "add-start failed"),
            },
            StoreCmd::AddEnd {
                session,
                exit_code,
                ts_unix_ns,
            } => match store.add_end(&session, exit_code, ts_unix_ns) {
                Ok(Some(id)) => info!(id, session, exit_code, "add-end"),
                Ok(None) => {}
                Err(e) => warn!(error = %e, session, "add-end failed"),
            },
            StoreCmd::Import {
                path,
                source,
                reply,
            } => {
                let result = match source.as_str() {
                    "zsh" | "bash" => store.import_file(&path, &source),
                    "atuin" => store.import_atuin_db(&path),
                    other => Err(anyhow::anyhow!("unsupported import source: {other}")),
                };
                match &result {
                    Ok(n) => info!(inserted = n, path = %path.display(), source, "import"),
                    Err(e) => warn!(error = %e, path = %path.display(), source, "import failed"),
                }
                // If the receiver was dropped (peer disconnected), we just
                // discard the result — the work is done either way.
                let _ = reply.send(result);
            }
        }
    }
    info!("storage thread exiting (channel closed)");
}

/// Spawn the storage worker on a named thread. The store must already be
/// opened by the caller so any schema-version error fails fast on the main
/// thread before the daemon advertises itself as ready.
pub fn spawn_storage_thread(
    store: Store,
    rx: mpsc::Receiver<StoreCmd>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("munin-storage".into())
        .spawn(move || run_storage_thread(store, rx))
        .context("spawn storage thread")
}

// ---- read API (called directly by the CLI / TUI) -------------------------

pub fn list(conn: &Connection, limit: usize, filters: &Filters) -> Result<Vec<EntryMeta>> {
    let limit = limit.min(MAX_LIMIT) as i64;
    let mut stmt = conn.prepare(LIST_SQL)?;
    let rows = stmt.query_map(
        params![
            filters.cwd,
            filters.session,
            filters.shell,
            filters.since,
            filters.until,
            limit,
        ],
        row_to_entry,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Fuzzy search over the recent-entries pool using nucleo-matcher.
///
/// Empty / whitespace-only queries fall through to `list` (most-recent N,
/// no scoring, no snippets). Non-empty queries pull up to `MAX_LIMIT`
/// candidates via `list_with_filters` and score each `cmd` with nucleo's
/// fzf-style algorithm; non-matches are dropped. `EntryMeta.snippet` is
/// built from the matched codepoint indices and wraps matched chars in
/// `‹…›`, matching the markers the TUI's `highlight_snippet` already
/// understands.
pub fn search(
    conn: &Connection,
    query: &str,
    sort: SearchSort,
    limit: usize,
    filters: &Filters,
) -> Result<Vec<EntryMeta>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return list(conn, limit, filters);
    }

    // Filters still apply at the SQL layer; the in-memory pool is bounded
    // by MAX_LIMIT so we don't blow up on huge DBs.
    let pool = list(conn, MAX_LIMIT, filters)?;

    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let pattern = nucleo_matcher::pattern::Pattern::parse(
        trimmed,
        nucleo_matcher::pattern::CaseMatching::Smart,
        nucleo_matcher::pattern::Normalization::Smart,
    );

    let mut hay_buf = Vec::new();
    let mut idx_buf = Vec::new();
    let mut scored: Vec<(u32, EntryMeta)> = Vec::with_capacity(pool.len());
    for mut entry in pool {
        idx_buf.clear();
        let haystack = nucleo_matcher::Utf32Str::new(&entry.cmd, &mut hay_buf);
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut idx_buf) {
            // nucleo doesn't guarantee sorted indices; sort once before we
            // walk them in highlight_indices.
            idx_buf.sort_unstable();
            entry.snippet = Some(highlight_indices(&entry.cmd, &idx_buf));
            scored.push((score, entry));
        }
    }

    match sort {
        SearchSort::Relevance => {
            // Score desc, tiebreak by id desc (newer first).
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
        }
        SearchSort::Recent => {
            scored.sort_by_key(|s| std::cmp::Reverse(s.1.id));
        }
    }

    let limit = limit.min(MAX_LIMIT);
    Ok(scored.into_iter().take(limit).map(|(_, e)| e).collect())
}

/// Walk `s` codepoint by codepoint; wrap runs of matched positions
/// (`indices`, sorted ascending) in `‹…›`. The TUI's `highlight_snippet`
/// parses these markers to colour the matched runs.
fn highlight_indices(s: &str, indices: &[u32]) -> String {
    let mut out = String::with_capacity(s.len() + indices.len() * 4);
    let mut idx_iter = indices.iter().copied().peekable();
    let mut in_match = false;
    for (i, c) in s.chars().enumerate() {
        let is_match = idx_iter.peek() == Some(&(i as u32));
        if is_match {
            if !in_match {
                out.push('‹');
                in_match = true;
            }
            out.push(c);
            idx_iter.next();
        } else {
            if in_match {
                out.push('›');
                in_match = false;
            }
            out.push(c);
        }
    }
    if in_match {
        out.push('›');
    }
    out
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<EntryMeta>> {
    Ok(conn
        .query_row(GET_SQL, params![id], row_to_entry)
        .optional()?)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryMeta> {
    Ok(EntryMeta {
        id: row.get(0)?,
        uuid: row.get(1)?,
        cmd: row.get(2)?,
        ts_unix_ns: row.get(3)?,
        cwd: row.get(4)?,
        hostname: row.get(5)?,
        session: row.get(6)?,
        shell: row.get(7)?,
        exit_code: row.get(8)?,
        duration_ms: row.get(9)?,
        snippet: None,
    })
}

// ---- history file parsers -------------------------------------------------

/// One parsed history line. Only the bits the file format actually carries.
struct ParsedEntry {
    cmd: String,
    ts_unix_ns: i64,
    duration_ms: Option<i64>,
}

/// One row read out of atuin's `history` table.
struct AtuinRow {
    uuid: String,
    cmd: String,
    ts_unix_ns: i64,
    duration_ns: i64,
    exit_code: i32,
    cwd: Option<String>,
    session: Option<String>,
    hostname: Option<String>,
}

/// Parse `.zsh_history`. Supports both extended (`: <ts>:<dur>;<cmd>`) and
/// plain formats; lines ending in `\` continue onto the next. Plain-format
/// entries get sequential synthesized timestamps (file order is preserved
/// because UUIDv7 carries the timestamp into the auto-derived sort key).
fn parse_zsh_history(path: &Path) -> Result<Vec<ParsedEntry>> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    let mut synth_ts = 0i64; // increments per row for plain-format files
    let mut buffer: Option<(i64, Option<i64>, String)> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // skip non-UTF-8 lines silently
        };
        if let Some((_, _, ref mut cmd)) = buffer {
            cmd.push('\n');
            cmd.push_str(line.trim_end_matches('\\'));
            if !line.ends_with('\\') {
                let (ts, dur, cmd) = buffer.take().unwrap();
                out.push(ParsedEntry {
                    cmd,
                    ts_unix_ns: ts,
                    duration_ms: dur,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(": ") {
            // ": <ts>:<dur>;<cmd>"
            if let Some((meta, cmd)) = rest.split_once(';') {
                let mut parts = meta.split(':');
                let ts_secs = parts.next().and_then(|s| s.trim().parse::<i64>().ok());
                let dur_secs = parts.next().and_then(|s| s.trim().parse::<i64>().ok());
                if let Some(ts_secs) = ts_secs {
                    let ts_unix_ns = ts_secs.saturating_mul(1_000_000_000);
                    let duration_ms = dur_secs.map(|s| s.saturating_mul(1_000));
                    push_or_continue(&mut out, &mut buffer, ts_unix_ns, duration_ms, cmd);
                    continue;
                }
            }
        }
        // Plain line.
        if line.is_empty() {
            continue;
        }
        synth_ts += 1;
        push_or_continue(&mut out, &mut buffer, synth_ts, None, &line);
    }
    if let Some((ts, dur, cmd)) = buffer {
        out.push(ParsedEntry {
            cmd,
            ts_unix_ns: ts,
            duration_ms: dur,
        });
    }
    Ok(out)
}

/// Parse `.bash_history`. Recognises `HISTTIMEFORMAT`-prefixed timestamps
/// (`#<unix-ts>` on a line of its own, followed by the command). Plain
/// lines without a leading timestamp get sequential synthesized timestamps.
fn parse_bash_history(path: &Path) -> Result<Vec<ParsedEntry>> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    let mut synth_ts = 0i64;
    let mut pending_ts: Option<i64> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        if let Some(ts) = line.strip_prefix('#').and_then(|s| s.trim().parse::<i64>().ok()) {
            pending_ts = Some(ts.saturating_mul(1_000_000_000));
            continue;
        }
        let ts_unix_ns = pending_ts.take().unwrap_or_else(|| {
            synth_ts += 1;
            synth_ts
        });
        out.push(ParsedEntry {
            cmd: line,
            ts_unix_ns,
            duration_ms: None,
        });
    }
    Ok(out)
}

fn push_or_continue(
    out: &mut Vec<ParsedEntry>,
    buffer: &mut Option<(i64, Option<i64>, String)>,
    ts_unix_ns: i64,
    duration_ms: Option<i64>,
    cmd_line: &str,
) {
    if let Some(stripped) = cmd_line.strip_suffix('\\') {
        *buffer = Some((ts_unix_ns, duration_ms, stripped.to_owned()));
    } else {
        out.push(ParsedEntry {
            cmd: cmd_line.to_owned(),
            ts_unix_ns,
            duration_ms,
        });
    }
}

