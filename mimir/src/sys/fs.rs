//! Filesystem usage via `statvfs(3)` (through `nix`). No parsing — a syscall.

use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskUsage {
    /// Total size in bytes.
    pub total: u64,
    /// Bytes available to an unprivileged user.
    pub avail: u64,
    /// Bytes in use (total minus free).
    pub used: u64,
    /// Used as a percentage, following `df`'s convention
    /// (`used / (used + avail)`), so reserved blocks don't read as 100%.
    pub used_pct: f64,
}

/// Query usage for the filesystem containing `path`.
pub fn usage(path: &Path) -> Result<DiskUsage> {
    let s =
        nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    let frsize = s.fragment_size() as u64;
    let total = s.blocks() as u64 * frsize;
    let free = s.blocks_free() as u64 * frsize;
    let avail = s.blocks_available() as u64 * frsize;
    let used = total.saturating_sub(free);
    let denom = used + avail;
    let used_pct = if denom == 0 {
        0.0
    } else {
        used as f64 / denom as f64 * 100.0
    };
    Ok(DiskUsage {
        total,
        avail,
        used,
        used_pct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_filesystem_is_sane() {
        // `/` always exists; assert the numbers are internally consistent
        // rather than pinning machine-specific sizes.
        let u = usage(Path::new("/")).unwrap();
        assert!(u.total > 0);
        assert!(u.used <= u.total);
        assert!((0.0..=100.0).contains(&u.used_pct));
    }
}
