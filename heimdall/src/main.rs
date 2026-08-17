//! `heimdall` — a rootless, presence-first LAN device finder. The all-seeing
//! watchman: discover devices on your network (ARP sweep + mDNS + SSDP, no
//! root) and see who's reachable right now.
//!
//! Run `heimdall` for the live picker, `heimdall scan` for a one-shot dump.

mod actions;
mod config;
mod device;
mod discover;
mod mac;
mod tui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use levelup_core::completions::Shell as CompletionShell;

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(
    name = "heimdall",
    version = VERSION,
    about = "Rootless LAN device finder — see who's on your network right now"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot, non-interactive device scan (debugging / tests).
    Scan,
    /// Print a shell alias snippet (`eval "$(heimdall init)"`).
    Init,
    /// Print a shell-completion script to stdout. SHELL defaults to $SHELL.
    #[command(visible_alias = "comp")]
    Completions {
        #[arg(value_enum)]
        shell: Option<CompletionShell>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Init) => {
            println!("alias ht='heimdall'");
            Ok(())
        }
        Some(Cmd::Scan) => {
            levelup_core::init_tracing("HEIMDALL_LOG");
            run_scan()
        }
        Some(Cmd::Completions { shell }) => {
            levelup_core::completions::print(&mut Cli::command(), "heimdall", shell)
        }
        // No subcommand → the live interactive picker.
        None => tui::run(),
    }
}

fn run_scan() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let cidrs = discover::local_cidrs();
    eprintln!(
        "scanning {} …",
        if cidrs.is_empty() {
            "(no IPv4 subnet found)".to_string()
        } else {
            cidrs.join(", ")
        }
    );
    let devices = rt.block_on(discover::scan_once());
    println!(
        "{:<15} {:<17} {:<22} {:<22} SERVICES",
        "IP", "MAC", "VENDOR", "HOST"
    );
    for d in &devices {
        println!(
            "{:<15} {:<17} {:<22} {:<22} {}",
            d.ip,
            d.mac.as_deref().unwrap_or("-"),
            clip(d.vendor_label().as_ref(), 22),
            clip(d.hostname.as_deref().unwrap_or("-"), 22),
            d.services.join(","),
        );
    }
    eprintln!("{} device(s) online", devices.len());
    Ok(())
}

/// Trim a column value to `max` chars (multibyte-safe).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}
