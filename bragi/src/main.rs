//! `bragi` — humane PipeWire audio control, named for the Norse god of music
//! and poetry. Run `bragi` for the live picker; `dump` / `watch` are the
//! non-interactive views of the same engine.

mod config;
mod engine;
mod state;
mod tui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use levelup_core::completions::Shell as CompletionShell;
use levelup_core::sanitize_display;
use state::{AudioState, NodeKind};

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(
    name = "bragi",
    version = VERSION,
    about = "Humane PipeWire audio control — route streams, switch devices, set volume"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot snapshot of the audio graph (devices, streams, routing).
    Dump,
    /// Run the engine and log every graph change until Ctrl-C (debugging).
    Watch,
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
        Some(Cmd::Completions { shell }) => {
            levelup_core::completions::print(&mut Cli::command(), "bragi", shell)
        }
        Some(Cmd::Watch) => {
            levelup_core::init_tracing("BRAGI_LOG");
            run_watch()
        }
        Some(Cmd::Dump) => {
            levelup_core::init_tracing("BRAGI_LOG");
            run_dump()
        }
        // No subcommand → the live interactive picker. Tracing only when
        // BRAGI_LOG is explicitly set (with stderr redirected to a file) —
        // unconditional logging would scribble over the alternate screen.
        None => {
            if std::env::var_os("BRAGI_LOG").is_some() {
                levelup_core::init_tracing("BRAGI_LOG");
            }
            tui::run()
        }
    }
}

/// `bragi watch` — the engine logs every change; we only hold the channel open
/// so the process lives until Ctrl-C quits the loop.
fn run_watch() -> Result<()> {
    let engine = engine::spawn();
    while engine.updates.recv().is_ok() {}
    Ok(())
}

/// `bragi dump` — wait for the enumeration round-trips to settle, print once.
fn run_dump() -> Result<()> {
    let engine = engine::spawn();
    let state = loop {
        let state = engine
            .updates
            .recv()
            .context("engine exited before enumeration finished")?;
        if state.complete {
            break state;
        }
    };
    print_table(&state);
    Ok(())
}

fn print_table(state: &AudioState) {
    println!("{:<7} {:>4}  {:>4}  NAME", "TYPE", "ID", "VOL");
    for kind in [
        NodeKind::Sink,
        NodeKind::Source,
        NodeKind::Playback,
        NodeKind::Record,
    ] {
        for node in state.nodes.values().filter(|n| n.kind == kind) {
            let default = if state.is_default(node) { "*" } else { " " };
            let vol = node
                .volume_pct()
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "-".into());
            let mut name = sanitize_display(&node.name);
            if let Some(target) = state.target_of(node) {
                name.push_str(&format!(" → {}", sanitize_display(&target.name)));
            }
            if !kind.is_device()
                && let Some(detail) = &node.detail
            {
                name.push_str(&format!("  ({})", sanitize_display(detail)));
            }
            let mute = if node.mute { " [muted]" } else { "" };
            println!(
                "{:<6}{} {:>4}  {:>4}  {}{}",
                kind.label(),
                default,
                node.id,
                vol,
                name,
                mute
            );
        }
    }
}
