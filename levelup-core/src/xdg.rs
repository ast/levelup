//! XDG base-directory resolution — one place for the `$XDG_*` / `$HOME`
//! fallbacks that every tool had reimplemented.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`.
pub fn data_home() -> Result<PathBuf> {
    base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
pub fn config_home() -> Result<PathBuf> {
    base("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_CACHE_HOME`, falling back to `$HOME/.cache`.
pub fn cache_home() -> Result<PathBuf> {
    base("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_RUNTIME_DIR`, falling back to `/tmp` (never fails).
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn base(var: &str, fallback: &str) -> Result<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .with_context(|| format!("neither {var} nor HOME is set"))
}

/// `<data_home>/<app>/<file>` — e.g. `data_file("munin", "munin.db")`.
pub fn data_file(app: &str, file: &str) -> Result<PathBuf> {
    Ok(data_home()?.join(app).join(file))
}

/// `<config_home>/<app>/config.toml`. `None` when neither `$XDG_CONFIG_HOME`
/// nor `$HOME` is set (config loading degrades to defaults, never an error).
pub fn config_file(app: &str) -> Option<PathBuf> {
    config_home().ok().map(|b| b.join(app).join("config.toml"))
}

/// `<cache_home>/<app>/<file>`. `None` when no base is resolvable.
pub fn cache_file(app: &str, file: &str) -> Option<PathBuf> {
    cache_home().ok().map(|b| b.join(app).join(file))
}

/// `<runtime_dir>/<name>` — e.g. a unix socket `runtime_file("munin.sock")`.
pub fn runtime_file(name: &str) -> PathBuf {
    runtime_dir().join(name)
}
