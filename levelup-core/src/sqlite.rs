//! The SQLite schema-version discipline shared by the storage layers
//! (munin/hugin/sleipnir). The version gate runs *before* any pragmas so a
//! rejected DB doesn't scatter `-wal`/`-shm` sidecars; each tool keeps its own
//! `open()` orchestration (client-id bootstrap, tool-specific pragmas) and just
//! calls these two functions.

use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

/// Refuse to open a database produced by a different schema generation. A fresh
/// DB (no `sentinel` table yet, `user_version == 0`) passes through so the
/// caller can apply the schema. `sentinel` is the table whose presence marks an
/// already-initialised DB (e.g. `"entries"`, `"dirs"`).
pub fn ensure_compatible_schema(
    conn: &Connection,
    path: &Path,
    version: i32,
    sentinel: &str,
) -> Result<()> {
    let found: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found == version {
        return Ok(());
    }
    if found == 0 && !table_exists(conn, sentinel)? {
        return Ok(());
    }
    bail!(
        "incompatible database at {}: schema v{found}, expected v{version}. \
         No automatic migration; delete the file and restart to recreate.",
        path.display(),
    );
}

/// Whether a table or view of this name exists.
pub fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            params![name],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_passes_then_version_is_enforced() {
        let conn = Connection::open_in_memory().unwrap();
        let path = Path::new(":memory:");
        // Fresh (no sentinel table, version 0) → OK.
        assert!(ensure_compatible_schema(&conn, path, 1, "entries").is_ok());

        // Stamp v1 + create the sentinel; now v1 is accepted, v2 is refused.
        conn.execute_batch("CREATE TABLE entries (id INTEGER); PRAGMA user_version = 1;")
            .unwrap();
        assert!(ensure_compatible_schema(&conn, path, 1, "entries").is_ok());
        assert!(ensure_compatible_schema(&conn, path, 2, "entries").is_err());
        assert!(table_exists(&conn, "entries").unwrap());
        assert!(!table_exists(&conn, "nope").unwrap());
    }
}
