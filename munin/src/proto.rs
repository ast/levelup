//! Wire types shared between the daemon and the `munin` CLI.
//!
//! All control messages are one JSON object per line. The daemon-routed
//! ops are deliberately narrow: `ping` and `import` get a one-line JSON
//! response.
//!
//! Capture ops (`add-start` / `add-end`) and read ops (`list` / `search`
//! / `get`) live in `storage::*` and are called directly by the CLI
//! against the SQLite file; they don't go through this protocol. The
//! daemon is not on the capture hot path — it exists for bulk `import`
//! now and cross-machine sync later.

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
    /// `search`-only: the `cmd` text with each fuzzy-matched character
    /// wrapped in `‹…›` markers (produced by `storage::highlight_indices`
    /// from nucleo's match indices). `None` for `list`/`get`.
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
    /// Import an existing history source — `.zsh_history`, `.bash_history`,
    /// or atuin's `history.db`. `source` selects the parser:
    /// `"zsh"` / `"bash"` / `"atuin"`. Long-running; daemon runs it on a
    /// blocking task.
    Import { path: String, source: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error { message: String },
    Imported { inserted: usize },
}
