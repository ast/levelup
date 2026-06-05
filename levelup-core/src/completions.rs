//! Shared shell-completion plumbing, so every tool exposes an identical
//! `completions [SHELL]` subcommand (alias `comp`): optional `SHELL` that
//! defaults to `$SHELL`, generated via `clap_complete`.
//!
//! Each tool declares the subcommand with `shell: Option<Shell>` (re-exported
//! here) and calls [`print`] with its own `clap::Command` and binary name.

use anyhow::{Result, anyhow};
use std::path::Path;

/// Re-exported so tools name the shell type as `levelup_core::completions::Shell`
/// rather than depending on `clap_complete` directly.
pub use clap_complete::Shell;

/// Print a completion script for `shell` (or, if `None`, the shell detected
/// from `$SHELL`) to stdout. Errors only when no shell is given and `$SHELL`
/// is unset or names a shell `clap_complete` can't generate for.
pub fn print(cmd: &mut clap::Command, bin: &str, shell: Option<Shell>) -> Result<()> {
    let shell = shell.or_else(detect_shell).ok_or_else(|| {
        anyhow!(
            "could not detect shell from $SHELL; pass one explicitly (e.g. `{bin} completions zsh`)"
        )
    })?;
    clap_complete::generate(shell, cmd, bin, &mut std::io::stdout());
    Ok(())
}

/// Best-effort shell detection from `$SHELL` (e.g. `/usr/bin/zsh` → `zsh`).
/// `None` if `$SHELL` is unset or names a shell `clap_complete` can't generate
/// for.
pub fn detect_shell() -> Option<Shell> {
    let shell = std::env::var("SHELL").ok()?;
    let name = Path::new(&shell).file_name()?.to_str()?;
    name.parse::<Shell>().ok()
}
