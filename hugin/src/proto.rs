//! Wire types shared between the daemon and the `hugin` CLI.
//!
//! All control messages are one JSON object per line. A successful
//! `read-blob` request is answered with a `BlobHeader` JSON line followed
//! by `len` raw bytes on the same stream.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub id: i64,
    pub ts_unix_ns: i64,
    /// "regular" or "primary"
    pub selection: String,
    pub mimes: Vec<String>,
    pub size_bytes: i64,
    /// Short text excerpt of the entry. For `list` / `get` responses this
    /// is the leading 200 chars of the indexable text (or `None` for
    /// image-only entries). For `search` responses it's an FTS5
    /// `snippet()` excerpt with match terms wrapped in `‹›` and elided
    /// context shown as `…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// List recent entries, newest first.
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<String>,
    },
    /// Fetch metadata for a single entry.
    Get { id: i64 },
    /// Fetch the raw blob for a (entry, mime) pair. `mime: None` lets the
    /// daemon pick (first `text/*`, else first available).
    ReadBlob {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Make an old entry the current clipboard selection again. The daemon
    /// becomes the data source until another app takes the selection.
    Copy {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<String>,
    },
    /// Full-text search across indexed entries. `query` is treated as a
    /// single phrase unless `raw` is true (then it's passed to FTS5 as-is).
    Search {
        query: String,
        #[serde(default)]
        raw: bool,
        #[serde(default)]
        sort: SearchSort,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchSort {
    #[default]
    Relevance,
    Recent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error {
        message: String,
    },
    Entries {
        entries: Vec<EntryMeta>,
    },
    Entry {
        entry: EntryMeta,
    },
    /// Sent in response to `ReadBlob`. The header is one JSON line; the next
    /// `len` bytes on the stream are the blob itself.
    BlobHeader {
        mime: String,
        len: usize,
    },
}
