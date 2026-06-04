//! `valkyrie` — a modeless, humane process finder/handler. The chooser of the
//! slain: fuzzy-find running processes and act on them (SIGTERM by default,
//! hold-to-SIGKILL, pause/resume, any signal), all in one live picker.
//!
//! One binary, no daemon, no storage. Subcommands:
//! - `pick [QUERY]` — the interactive TUI (Alt-P launches it).
//! - `init <shell>` — print the shell hook (Alt-P binding + `vk` alias).
//! - `list [QUERY]` — non-interactive ranked dump (debugging / tests).

mod config;
mod ports;
mod proc;
mod search;
mod shells;
mod signals;
mod tree;
mod tui;
mod util;

use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::proc::Scanner;
use crate::search::{Sort, rank};
use crate::shells::Shell;
use crate::util::sanitize_display;

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(
    name = "valkyrie",
    version = VERSION,
    about = "Modeless, humane process finder/handler — the chooser of the slain"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the interactive picker, seeded with QUERY.
    Pick { query: Vec<String> },
    /// Print the shell hook for SHELL (`eval "$(valkyrie init zsh)"`).
    Init {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Non-interactive ranked process dump (debugging / tests).
    List {
        query: Vec<String>,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { shell } => {
            print!("{}", shells::init_script(shell));
            Ok(())
        }
        Cmd::List { query, limit } => {
            util::init_tracing();
            run_list(&query.join(" "), limit)
        }
        Cmd::Pick { query } => {
            tui::run(query.join(" "))?;
            Ok(())
        }
    }
}

fn run_list(query: &str, limit: usize) -> Result<()> {
    // Two scans ~300ms apart so %CPU is a real delta, not 0 (the TUI gets this
    // from its live tick instead).
    let mut scanner = Scanner::default();
    let _ = scanner.scan();
    std::thread::sleep(Duration::from_millis(300));
    let ranked = rank(scanner.scan(), query, Sort::Cpu);

    println!(
        "{:>7} {:<12} {:>5} {:>9} {:1} CMD",
        "PID", "USER", "%CPU", "RSS", "S"
    );
    for p in ranked.iter().take(limit) {
        println!(
            "{:>7} {:<12} {:>5.1} {:>8}K {} {}",
            p.pid,
            truncate(&p.user, 12),
            p.cpu_pct,
            p.rss_kb,
            p.state,
            sanitize_display(&p.command),
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}
