//! `gorm` — a rootless, humane Bluetooth device manager. Named for Gorm the
//! Old, Harald Bluetooth's father — the namesake lineage of Bluetooth itself.
//! Heimdall watches the LAN; gorm tends the personal Bluetooth radius.
//!
//! Run `gorm` for the live picker, `gorm scan` for a one-shot dump.

mod config;
mod device;
mod engine;
mod tui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use levelup_core::completions::Shell as CompletionShell;

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(
    name = "gorm",
    version = VERSION,
    about = "Rootless Bluetooth device manager — connect, pair, and trust devices"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot, non-interactive device dump (debugging / tests).
    Scan,
    /// Print a shell alias snippet (`eval "$(gorm init)"`).
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
            println!("alias gm='gorm'");
            Ok(())
        }
        Some(Cmd::Completions { shell }) => {
            levelup_core::completions::print(&mut Cli::command(), "gorm", shell)
        }
        Some(Cmd::Scan) => {
            levelup_core::init_tracing("GORM_LOG");
            run_scan()
        }
        // No subcommand → the live interactive picker.
        None => tui::run(),
    }
}

/// `gorm scan` — power on the adapter and print what BlueZ knows.
fn run_scan() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let devices = rt.block_on(engine::scan_once())?;

    println!(
        "{:<17} {:<12} {:>5} {:>5} {:<16} NAME",
        "ADDRESS", "STATE", "RSSI", "BATT", "TYPE"
    );
    for d in &devices {
        let mut state = d.state_label().to_string();
        if d.trusted {
            state.push_str(" ✓");
        }
        println!(
            "{:<17} {:<12} {:>5} {:>5} {:<16} {}",
            d.address,
            state,
            d.rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
            d.battery
                .map(|b| format!("{b}%"))
                .unwrap_or_else(|| "-".into()),
            d.icon.as_deref().unwrap_or("-"),
            d.name.as_deref().unwrap_or("(unknown)"),
        );
    }
    eprintln!("{} device(s) known", devices.len());
    Ok(())
}
