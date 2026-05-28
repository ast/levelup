//! Storage layer: SQLite Store + the storage worker thread.
//!
//! The `Connection` is owned by exactly one OS thread (`munin-storage`).
//! `munind` sends write commands via `mpsc<StoreCmd>`. Reads
//! (`list`/`search`/`get`) open ephemeral `Connection`s from inside
//! `spawn_blocking` tokio tasks — WAL mode makes that safe.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::proto::{EntryMeta, Filters, SearchSort};

/// Schema version stored in `PRAGMA user_version`. Bump when the on-disk
/// shape changes incompatibly; `Store::open` refuses to start on mismatch.
/// v1 = entries + config; v2 = adds entries_fts (FTS5) + entries_ad trigger.
const DB_VERSION: i32 = 2;

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

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(content);

-- The FK CASCADE on related tables doesn't reach virtual tables, so this
-- trigger is load-bearing: without it, retention or manual DELETEs would
-- leave orphaned FTS rows that still match searches.
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    DELETE FROM entries_fts WHERE rowid = OLD.id;
END;
";

/// Upper bound on `limit` for list/search queries. Caps the result set so an
/// IPC peer can't force the daemon to materialise the entire table — and
/// in particular guards against `usize::MAX as i64 == -1`, which SQLite
/// would otherwise interpret as "no limit".
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

// FTS5 search. Two variants only differ in ORDER BY; column list and snippet
// markers (`‹›/…/16`) stay in sync. The `snippet()` arguments are the FTS
// column index (0 = our `content` column), open marker, close marker,
// ellipsis, and token-count budget — keep these aligned with the CLI's table
// width if you tune one.
const SEARCH_SQL_RELEVANCE: &str = "\
    SELECT e.id, e.uuid, e.cmd, e.ts_unix_ns, e.cwd, e.hostname, e.session, e.shell, \
           e.exit_code, e.duration_ms, snippet(entries_fts, 0, '‹', '›', '…', 16) \
    FROM entries_fts \
    JOIN entries e ON e.id = entries_fts.rowid \
    WHERE entries_fts MATCH ?1 \
      AND (?2 IS NULL OR e.cwd = ?2) \
      AND (?3 IS NULL OR e.session = ?3) \
      AND (?4 IS NULL OR e.shell = ?4) \
      AND (?5 IS NULL OR e.ts_unix_ns >= ?5) \
      AND (?6 IS NULL OR e.ts_unix_ns <= ?6) \
    ORDER BY bm25(entries_fts) ASC, e.id DESC LIMIT ?7";

const SEARCH_SQL_RECENT: &str = "\
    SELECT e.id, e.uuid, e.cmd, e.ts_unix_ns, e.cwd, e.hostname, e.session, e.shell, \
           e.exit_code, e.duration_ms, snippet(entries_fts, 0, '‹', '›', '…', 16) \
    FROM entries_fts \
    JOIN entries e ON e.id = entries_fts.rowid \
    WHERE entries_fts MATCH ?1 \
      AND (?2 IS NULL OR e.cwd = ?2) \
      AND (?3 IS NULL OR e.session = ?3) \
      AND (?4 IS NULL OR e.shell = ?4) \
      AND (?5 IS NULL OR e.ts_unix_ns >= ?5) \
      AND (?6 IS NULL OR e.ts_unix_ns <= ?6) \
    ORDER BY e.id DESC LIMIT ?7";

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
        shell: String,
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
    /// The `entries` row and the matching `entries_fts` row are inserted in
    /// the same transaction so a partial commit can't leave a searchable row
    /// without metadata or vice versa.
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
        let tx = self.conn.transaction()?;
        tx.execute(
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
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO entries_fts (rowid, content) VALUES (?1, ?2)",
            params![id, cmd],
        )?;
        tx.commit()?;
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
            let mut insert_fts =
                tx.prepare("INSERT INTO entries_fts (rowid, content) VALUES (?1, ?2)")?;
            for e in entries {
                // UUIDv7 with the entry's timestamp so imports sort right next
                // to live captures. We don't pass cwd/session/exit_code —
                // shell history files don't carry them.
                let uuid = Uuid::now_v7().to_string();
                insert_entry.execute(params![
                    uuid,
                    self.client_id,
                    e.cmd,
                    e.ts_unix_ns,
                    hostname,
                    shell,
                    e.duration_ms,
                ])?;
                let id = tx.last_insert_rowid();
                insert_fts.execute(params![id, e.cmd])?;
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
/// Fresh DBs (no `entries` table yet) pass through; v2 DBs must additionally
/// have the FTS5 virtual table present (rebuilding it silently would hide
/// historical rows from search).
fn ensure_compatible_schema(conn: &Connection, path: &Path) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == DB_VERSION {
        if !table_exists(conn, "entries_fts")? {
            bail!(
                "munin database at {} is at schema v{version} but entries_fts is missing; \
                 the search index has been dropped out-of-band. Delete the file and restart \
                 to recreate.",
                path.display(),
            );
        }
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
                shell,
                reply,
            } => {
                let result = store.import_file(&path, &shell);
                match &result {
                    Ok(n) => info!(inserted = n, path = %path.display(), shell, "import"),
                    Err(e) => warn!(error = %e, path = %path.display(), "import failed"),
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

// ---- read API used by IPC's spawn_blocking tasks --------------------------

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

/// FTS5 search. `query` is treated as a single phrase (wrapped in double
/// quotes, embedded `"` doubled) unless `raw` is set — then it's passed
/// through verbatim so the user can use FTS5 operators like `AND`, `OR`,
/// `NEAR`, prefix wildcards `foo*`, etc. Empty/whitespace queries
/// short-circuit so we don't ship `""` to FTS5 (which would raise a syntax
/// error).
pub fn search(
    conn: &Connection,
    query: &str,
    raw: bool,
    sort: SearchSort,
    limit: usize,
    filters: &Filters,
) -> Result<Vec<EntryMeta>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = if raw {
        query.to_owned()
    } else {
        // FTS5 phrase literal: wrap in double quotes, doubling embedded ones.
        // Trim leading/trailing whitespace so "git " behaves like "git" (a
        // trailing space inside the phrase makes FTS5 match nothing).
        format!("\"{}\"", trimmed.replace('"', "\"\""))
    };
    let limit = limit.min(MAX_LIMIT) as i64;
    let sql = match sort {
        SearchSort::Relevance => SEARCH_SQL_RELEVANCE,
        SearchSort::Recent => SEARCH_SQL_RECENT,
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![
            match_expr,
            filters.cwd,
            filters.session,
            filters.shell,
            filters.since,
            filters.until,
            limit,
        ],
        row_to_entry_with_snippet,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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

fn row_to_entry_with_snippet(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryMeta> {
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
        snippet: row.get(10)?,
    })
}

// ---- history file parsers -------------------------------------------------

/// One parsed history line. Only the bits the file format actually carries.
struct ParsedEntry {
    cmd: String,
    ts_unix_ns: i64,
    duration_ms: Option<i64>,
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

