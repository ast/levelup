//! The live filesystem walk — Sleipnir's *completeness* source.
//!
//! Frecency (chpwd + munin) is only a ranking *bias*; it can't be the thing
//! that decides what exists, or files you never named on a command line (an
//! emacs edit, say) would vanish. So the candidate set is the world itself: a
//! bounded, gitignore-aware walk rooted at your locus. Each walked entry is
//! scored by **mtime recency** — an app-agnostic "I just touched this" signal
//! that catches every editor for free, no per-app integration. The two streams
//! are then fused (`fuse`) keeping the *stronger* signal per path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use tracing::debug;

use crate::storage::{Kind, Row, frecency};
use crate::util::{abbrev_home, home_dir};

/// Stop the walk after this many entries. We log when we hit it rather than
/// truncating silently (a capped walk that looks complete is a lie).
const WALK_CAP: usize = 20_000;
/// Don't descend deeper than this — a backstop against pathological trees.
const WALK_MAX_DEPTH: usize = 24;

/// Heavy directories pruned even when no `.gitignore` covers them (e.g. when
/// you're not inside a git repo). `.git` is also hidden, but belt-and-braces.
const PRUNE_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// The nearest ancestor of `start` containing a `.git` (the repo root), or
/// `None` if `start` isn't inside a repo. Drives the user's chosen reach:
/// "pwd + repo root".
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Walk from the enclosing repo root (or `cwd` itself when not in a repo) and
/// return rows scored by mtime recency. gitignore-aware + hidden-skipping via
/// the `ignore` crate, so build artifacts and VCS internals stay out.
pub fn walk_pool(cwd: &Path, now: i64) -> Vec<Row> {
    let root = repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    // Canonicalise the root so walked paths line up with the canonicalised
    // paths frecency stores (keeps `fuse`'s dedup honest).
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let home = home_dir();
    let home = home.as_deref();
    let mut rows = Vec::new();
    let mut capped = false;

    let walker = WalkBuilder::new(&root)
        .max_depth(Some(WALK_MAX_DEPTH))
        .filter_entry(|e| {
            // Prune heavy dirs by name (don't even descend). Files always pass.
            !e.file_type().is_some_and(|ft| ft.is_dir())
                || !PRUNE_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
        })
        .build();

    for result in walker {
        if rows.len() >= WALK_CAP {
            capped = true;
            break;
        }
        let Ok(entry) = result else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            continue; // symlink-to-nowhere, socket, etc.
        };
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let path = entry.into_path().to_string_lossy().into_owned();
        let display = abbrev_home(&path, home);
        rows.push(Row {
            path,
            display,
            kind,
            // mtime recency reuses the frecency decay curve with rank 1, so a
            // just-edited file scores like a dir visited once in the last hour.
            frecency: frecency(1.0, mtime, now),
            snippet: None,
        });
    }

    if capped {
        debug!(cap = WALK_CAP, root = %root.display(), "walk hit cap; some entries omitted");
    }
    rows
}

/// Fuse the habit stream (frecency, anywhere) with the completeness stream
/// (walk, in-locus), deduping by path and keeping the **stronger** signal. A
/// file that's both freshly edited and historically used takes the higher of
/// its mtime-recency and its frecency — so a just-edited-in-emacs file is never
/// buried under a stale frecency score, and a power-used file keeps its boost.
pub fn fuse(frecency_rows: Vec<Row>, walk_rows: Vec<Row>) -> Vec<Row> {
    let mut by_path: HashMap<String, Row> =
        HashMap::with_capacity(frecency_rows.len() + walk_rows.len());
    for row in frecency_rows.into_iter().chain(walk_rows) {
        match by_path.get_mut(&row.path) {
            Some(existing) => {
                if row.frecency > existing.frecency {
                    existing.frecency = row.frecency;
                }
            }
            None => {
                by_path.insert(row.path.clone(), row);
            }
        }
    }
    by_path.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, score: f64) -> Row {
        Row {
            path: path.into(),
            display: path.into(),
            kind: Kind::File,
            frecency: score,
            snippet: None,
        }
    }

    #[test]
    fn fuse_keeps_the_stronger_signal() {
        // Same path in both streams: a stale frecency (0.5) and a fresh
        // mtime-recency (4.0) → the row should end up at 4.0, not buried.
        let frec = vec![row("/p/a", 0.5)];
        let walk = vec![row("/p/a", 4.0)];
        let fused = fuse(frec, walk);
        assert_eq!(fused.len(), 1, "deduped to one row");
        assert_eq!(fused[0].frecency, 4.0, "stronger signal wins");
    }

    #[test]
    fn fuse_unions_distinct_paths() {
        let frec = vec![row("/p/a", 8.0)]; // habitual file, not under cwd
        let walk = vec![row("/p/b", 4.0)]; // freshly-edited file under cwd
        let mut fused = fuse(frec, walk);
        fused.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].path, "/p/a");
        assert_eq!(fused[1].path, "/p/b");
    }

    #[test]
    fn repo_root_finds_dotgit_ancestor() {
        // CARGO_MANIFEST_DIR is sleipnir/, which has no .git of its own in this
        // workspace; the call should either find an ancestor repo or None, but
        // never panic, and the result (if any) must contain a .git.
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        if let Some(root) = repo_root(here) {
            assert!(root.join(".git").exists());
        }
    }
}
