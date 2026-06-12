//! Embed the current git commit (short hash, plus `-dirty`) so `--version` can
//! show it. Sets `GIT_COMMIT`; falls back to "unknown" when git isn't available
//! (e.g. building from a source tarball).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let commit = git_commit().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    rerun_on_head_change();
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_commit() -> Option<String> {
    let hash = git(&["rev-parse", "--short", "HEAD"])?;
    if hash.is_empty() {
        return None;
    }
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}

/// Rebuild when HEAD (or the ref it points at) moves, so `--version` stays
/// accurate across commits and branch switches.
fn rerun_on_head_change() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = PathBuf::from(git_dir);
    let head = git_dir.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(contents) = std::fs::read_to_string(&head)
        && let Some(reference) = contents.strip_prefix("ref:").map(str::trim)
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
}
