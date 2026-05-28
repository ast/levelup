//! Wire types shared between the daemon and the `munin` CLI.
//!
//! All control messages are one JSON object per line. Reads (`ping`,
//! `list`, `search`, `get`, `import`) get a one-line JSON response.
//! Captures (`add-start` / `add-end`) are fire-and-forget — the daemon
//! does not write a response, so the client can write the line and exit
//! without waiting for a round trip.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub id: i64,
    pub uuid: String,
    pub cmd: String,
    pub ts_unix_ns: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// `search`-only: FTS5 `snippet()` excerpt with match terms wrapped in
    /// `‹›` and elided context shown as `…`. Empty/None for list/get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchSort {
    #[default]
    Relevance,
    Recent,
}

/// Optional filters shared by `list` and `search`. All `None` means "no
/// filter on this column". `since` / `until` are unix nanoseconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Record the start of a shell command. Fire-and-forget — no response.
    AddStart {
        cmd: String,
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts_unix_ns: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
    },
    /// Close out the most-recent open command for this session.
    /// Fire-and-forget — no response.
    AddEnd {
        session: String,
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts_unix_ns: Option<i64>,
    },
    /// Recent entries, newest first.
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default)]
        filters: Filters,
    },
    /// Full-text search across `cmd` via FTS5. `query` is treated as a
    /// single phrase unless `raw` is set, in which case it's passed to FTS5
    /// verbatim (so operators like `AND`, `OR`, `NEAR`, `prefix*` work).
    Search {
        query: String,
        #[serde(default)]
        raw: bool,
        #[serde(default)]
        sort: SearchSort,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default)]
        filters: Filters,
    },
    /// Metadata for one entry.
    Get { id: i64 },
    /// Import an existing shell-history file (`.zsh_history` or
    /// `.bash_history`). Long-running; daemon runs it on a blocking task.
    Import { path: String, shell: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error { message: String },
    Entries { entries: Vec<EntryMeta> },
    Entry { entry: EntryMeta },
    Imported { inserted: usize },
}
