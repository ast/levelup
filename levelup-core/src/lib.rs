//! Shared, non-TUI helpers for the levelup tools — the leaf utilities that were
//! copy-pasted across the crates during rapid iteration: XDG path resolution,
//! terminal-safe display sanitisation, tracing setup, nucleo fuzzy helpers, and
//! the SQLite schema-version discipline.
//!
//! This is a *toolkit*, not a framework: each tool composes these functions; we
//! deliberately don't impose a shared event loop or item model.

pub mod completions;
pub mod fuzzy;
pub mod sqlite;
pub mod xdg;

use tracing_subscriber::EnvFilter;

/// Install a stderr `tracing` subscriber honouring `env_var` (full
/// `tracing-subscriber` EnvFilter syntax), defaulting to `info`. Each tool
/// passes its own variable, e.g. `init_tracing("MUNIN_LOG")`.
pub fn init_tracing(env_var: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env(env_var).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Make arbitrary stored text safe to print to a terminal: neutralise control
/// bytes so a stored escape sequence can't *drive* the terminal (a stored
/// `ESC[?1003h` would otherwise flip on mouse reporting). C0 → caret notation
/// (`^[`), DEL → `^?`, newline → `↵`, tab → space, other control (C1) → U+FFFD.
pub fn sanitize_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\u{21B5}'),
            '\t' => out.push(' '),
            '\u{7f}' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push((b'@' + c as u8) as char);
            }
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_neutralizes_control_bytes() {
        assert_eq!(sanitize_display("a\x1b[?1003hb"), "a^[[?1003hb");
        assert_eq!(sanitize_display("x\ty\nz"), "x y\u{21B5}z");
        assert_eq!(sanitize_display("a\x7fb\u{0085}c"), "a^?b\u{FFFD}c");
        // The ‹› highlight markers must pass through untouched.
        assert_eq!(sanitize_display("git ‹commit›"), "git ‹commit›");
    }
}
