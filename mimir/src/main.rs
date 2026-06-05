//! mimir — a status bar for sway. With no subcommand it runs the bar (swaybar
//! protocol on stdout, click events on stdin). See `mimir --help`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use levelup_core::completions::Shell;
use tracing::debug;

use mimir::blocks::{self, Block, Ctx, Segment};
use mimir::config::{self, Config};
use mimir::protocol::{self, ClickEvent};
use mimir::{init_tracing, render};

/// `"<pkgver> (<git-commit>)"` — the git commit is embedded by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")");

#[derive(Parser)]
#[command(name = "mimir", version = VERSION, about = "A compact status bar for sway")]
struct Cli {
    /// Config path. Default: $XDG_CONFIG_HOME/mimir/config.toml
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render a single plain-text snapshot and exit (for debugging, no bar).
    Once,
    /// Print the default config TOML to stdout.
    PrintConfig,
    /// Print a shell-completion script to stdout. SHELL defaults to $SHELL.
    #[command(visible_alias = "comp")]
    Completions {
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // These write to stdout and must not be polluted by the tracing init or
    // a config load, so handle them before anything else.
    match &cli.cmd {
        Some(Cmd::PrintConfig) => {
            print!("{}", config::DEFAULT_CONFIG_TOML);
            return Ok(());
        }
        Some(Cmd::Completions { shell }) => {
            return levelup_core::completions::print(&mut Cli::command(), "mimir", *shell);
        }
        _ => {}
    }

    init_tracing();
    let cfg = config::load_or_default(cli.config.as_deref());

    match cli.cmd {
        Some(Cmd::Once) => run_once(&cfg),
        _ => run_bar(&cfg),
    }
}

/// Render every block once and collect their segments. `advance` controls
/// whether delta-based blocks update their baseline (see [`Ctx::advance`]).
fn sample(
    blocks: &mut [Box<dyn Block>],
    cfg: &Config,
    dt_secs: f64,
    advance: bool,
) -> Vec<Segment> {
    let ctx = Ctx {
        cfg,
        dt_secs,
        advance,
    };
    let mut out = Vec::new();
    for b in blocks.iter_mut() {
        out.extend(b.render(&ctx));
    }
    out
}

/// `mimir once`: warm up (to seed deltas), wait briefly, then print one plain
/// snapshot. The pause lets CPU/network rates be computed from a real delta.
fn run_once(cfg: &Config) -> Result<()> {
    let mut blocks = blocks::build(cfg);
    let _ = sample(&mut blocks, cfg, 0.0, true); // seed baselines
    let dt = 0.3;
    std::thread::sleep(Duration::from_secs_f64(dt));
    let segs = sample(&mut blocks, cfg, dt, true);
    println!("{}", render::to_plain(&segs));
    Ok(())
}

/// Run the bar: emit the protocol header, then a status array per tick, and
/// re-render immediately on each click.
fn run_bar(cfg: &Config) -> Result<()> {
    let mut blocks = blocks::build(cfg);
    let interval = Duration::from_millis(cfg.interval_ms.max(50));

    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", protocol::HEADER_LINE).context("write header")?;
    writeln!(out, "[").context("write body open")?;
    out.flush().ok();

    // Click events arrive on stdin. The keepalive sender means `recv_timeout`
    // only ever times out (never disconnects), so a closed stdin doesn't spin
    // the loop — the timer keeps driving renders.
    let (tx, rx) = mpsc::channel::<ClickEvent>();
    let _keepalive = tx.clone();
    std::thread::spawn(move || read_clicks(tx));

    let mut last_tick = Instant::now();
    loop {
        let dt = last_tick.elapsed().as_secs_f64();
        // First render after this point is a tick (advance baselines).
        let segs = sample(&mut blocks, cfg, dt, true);
        last_tick = Instant::now();
        if write_status(&mut out, &segs, cfg).is_err() {
            break; // swaybar closed our stdout → exit quietly
        }

        // Wait for the next tick, re-rendering immediately on any click.
        match rx.recv_timeout(interval) {
            Ok(ev) => {
                handle_click(&mut blocks, &ev, cfg);
                let dt = last_tick.elapsed().as_secs_f64();
                let segs = sample(&mut blocks, cfg, dt, false);
                if write_status(&mut out, &segs, cfg).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Serialise one status array and write it (with the protocol's trailing
/// comma). Returns `Err` if stdout is gone.
fn write_status(out: &mut impl Write, segs: &[Segment], cfg: &Config) -> std::io::Result<()> {
    let blocks = render::to_protocol(segs.to_vec(), cfg);
    let json = serde_json::to_string(&blocks).expect("blocks serialise");
    writeln!(out, "{json},")?;
    out.flush()
}

/// Route a click: a configured `[[bindings]]` command takes precedence;
/// otherwise the matching block handles it (expand/cycle).
fn handle_click(blocks: &mut [Box<dyn Block>], ev: &ClickEvent, cfg: &Config) {
    if let Some(name) = &ev.name {
        if let Some(binding) = cfg
            .bindings
            .iter()
            .find(|b| &b.block == name && b.button == ev.button)
        {
            spawn_command(&binding.command);
            return;
        }
        for b in blocks.iter_mut() {
            if b.name() == name {
                b.on_click(ev);
            }
        }
    }
}

/// Spawn a bound command via `sh -c`, detached and fire-and-forget (stdio
/// nulled so it can't write onto the bar's stdout).
fn spawn_command(command: &str) {
    if let Err(e) = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        debug!(error = %e, command, "failed to spawn bound command");
    }
}

/// Read swaybar click events from stdin. The stream is a `[` followed by one
/// JSON object per line, each (after the first) prefixed with a comma.
fn read_clicks(tx: mpsc::Sender<ClickEvent>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim().trim_start_matches(',').trim();
        if trimmed.is_empty() || trimmed == "[" {
            continue;
        }
        match serde_json::from_str::<ClickEvent>(trimmed) {
            Ok(ev) => {
                if tx.send(ev).is_err() {
                    break;
                }
            }
            Err(e) => debug!(error = %e, line = trimmed, "ignoring malformed click event"),
        }
    }
}
