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
    /// image-only entries). For `search` responses it's the matched text
    /// with the nucleo-matched chars wrapped in `‹›`.
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
    /// becomes the data source until another app takes the selection. When
    /// `mime` is `Some`, only that MIME is served (used by the interactive
    /// picker's MIME chooser); `None` serves every MIME the entry carries.
    Copy {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Delete an entry from history. `mime_parts` cascade on the foreign key;
    /// the row is gone for good (no undo). Used by the picker's Ctrl-X.
    Delete { id: i64 },
    /// Fuzzy search across stored entries (fzf-style scoring via
    /// nucleo-matcher). Empty/whitespace queries fall through to most-recent.
    Search {
        query: String,
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
