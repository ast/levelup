//! `munin` CLI — talks to the running daemon over its unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use clap::{Parser, Subcommand};
use rusqlite::Connection;

use munin::config;
use munin::proto::{EntryMeta, Filters, Request, Response, SearchSort};
use munin::shells::{self, Shell};
use munin::storage::{self, default_db_path};
use munin::tui::Outcome;
use munin::{current_hostname, default_socket_path, fmt_dur, now_unix_ns, tui};

#[derive(Parser)]
#[command(name = "munin", version, about = "Query the munin shell-history daemon")]
struct Cli {
    /// Override the daemon socket path. Default: $XDG_RUNTIME_DIR/munin.sock.
    /// Only used by daemon-routed subcommands (ping, add-start, add-end,
    /// import).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Override the SQLite database path. Default:
    /// $XDG_DATA_HOME/munin/munin.db. Only affects read subcommands
    /// (list, search, get) and the interactive TUI — those open the DB
    /// directly.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Verify the daemon is responsive
    Ping,
    /// Record the start of a shell command (used by shell hooks).
    /// Fire-and-forget — the daemon does not respond, the CLI exits as soon
    /// as the request is written.
    AddStart {
        /// The command line about to run.
        cmd: String,
        /// Stable identifier for this shell session (typically $$).
        session: String,
    },
    /// Record the exit of the most recent command in this session.
    /// Fire-and-forget.
    AddEnd {
        /// Session id passed to the matching add-start.
        session: String,
        /// Exit code of the command (zsh / bash `$?`).
        exit_code: i32,
    },
    /// Print the shell-hook script for SHELL to stdout. Wire it up with
    /// `eval "$(munin init zsh)"` (or `bash`) in your rc file.
    Init {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// List recent shell-history entries (newest first).
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        filters: FilterArgs,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Fuzzy search across recorded commands (fzf-style scoring via
    /// nucleo-matcher). Pass -i to open an interactive picker instead of a
    /// one-shot query.
    #[command(visible_alias = "s")]
    Search {
        /// Query terms; joined with spaces and matched fuzzily against each
        /// command. With -i, used as the initial query.
        query: Vec<String>,
        /// Open the interactive TUI seeded with QUERY (if any). Print the
        /// chosen command to stdout on Enter; exit silently on Esc.
        #[arg(short, long)]
        interactive: bool,
        /// Result ordering.
        #[arg(long, value_enum, default_value_t = SearchSort::Relevance)]
        sort: SearchSort,
        #[command(flatten)]
        filters: FilterArgs,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one entry by id.
    #[command(visible_alias = "info")]
    Get { id: i64 },
    /// Import history from a file or another tool's database.
    Import {
        #[command(subcommand)]
        from: ImportFrom,
    },
}

#[derive(Subcommand)]
enum ImportFrom {
    /// Parse a `.zsh_history` file (extended format or plain).
    Zsh { path: PathBuf },
    /// Parse a `.bash_history` file (HISTTIMEFORMAT-aware or plain).
    Bash { path: PathBuf },
    /// Copy from an atuin `history.db`. Defaults to
    /// `~/.local/share/atuin/history.db`. Idempotent — atuin's UUIDv7 ids
    /// are preserved as munin's `uuid`, so re-imports drop dupes.
    Atuin {
        /// Override the path to atuin's history database.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct FilterArgs {
    /// Only entries with this exact cwd.
    #[arg(long)]
    cwd: Option<String>,
    /// Only entries from this session id.
    #[arg(long)]
    session: Option<String>,
    /// Only entries from this shell ("zsh" / "bash").
    #[arg(long)]
    shell: Option<String>,
    /// Only entries on or after this time. Accepts "YYYY-MM-DD" or
    /// "YYYY-MM-DD HH:MM:SS" (interpreted as local time).
    #[arg(long)]
    since: Option<String>,
    /// Only entries on or before this time. Same formats as --since.
    #[arg(long)]
    until: Option<String>,
}

impl FilterArgs {
    fn into_proto(self) -> Result<Filters> {
        Ok(Filters {
            cwd: self.cwd,
            session: self.session,
            shell: self.shell,
            since: self.since.as_deref().map(parse_when).transpose()?,
            until: self.until.as_deref().map(parse_when).transpose()?,
        })
    }
}

/// Parse a `--since` / `--until` value (local-time `YYYY-MM-DD` or
/// `YYYY-MM-DD HH:MM:SS`) into unix nanoseconds.
fn parse_when(s: &str) -> Result<i64> {
    let s = s.trim();
    let dt = if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        d.and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("bad date {s:?}"))?
    } else if let Ok(d) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        d
    } else {
        return Err(anyhow!(
            "could not parse {s:?} as YYYY-MM-DD or YYYY-MM-DD HH:MM:SS"
        ));
    };
    let local = Local
        .from_local_datetime(&dt)
        .single()
        .ok_or_else(|| anyhow!("ambiguous local time {s:?}"))?;
    Ok(local.timestamp_nanos_opt().unwrap_or(i64::MAX))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Subcommands that don't need the daemon:
    //   - `init` (just prints an embedded script)
    //   - `search -i` (interactive TUI, opens SQLite directly)
    //   - `list` / `search` (non-interactive) / `get` (read straight from
    //     the DB so the read CLI keeps working when munind is down)
    match cli.cmd {
        Cmd::Init { shell } => {
            print!("{}", shells::init_script(shell));
            return Ok(());
        }
        Cmd::Search {
            interactive: true, ..
        } => return run_tui(cli.cmd, cli.db.as_deref()),
        Cmd::List { .. } | Cmd::Search { .. } | Cmd::Get { .. } => {
            return run_read(cli.cmd, cli.db.as_deref());
        }
        // Fall through to the daemon-routed path below.
        Cmd::Ping | Cmd::AddStart { .. } | Cmd::AddEnd { .. } | Cmd::Import { .. } => {}
    }

    let path = cli.socket.unwrap_or_else(default_socket_path);
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to munin daemon at {}", path.display()))?;
    let mut reader = BufReader::new(stream.try_clone().context("clone socket")?);
    let mut writer = stream;

    match cli.cmd {
        Cmd::Ping => {
            send(&mut writer, &Request::Ping)?;
            match read_response(&mut reader)? {
                Response::Ok => println!("ok"),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("ping", &other)),
            }
        }
        Cmd::AddStart { cmd, session } => {
            let cwd = std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string());
            // `MUNIN_SHELL` is exported by `munin init <shell>` so we know
            // which shell is actually calling us (rather than guessing from
            // the user's login `$SHELL`, which doesn't change for nested bash).
            let shell = std::env::var("MUNIN_SHELL")
                .ok()
                .or_else(|| std::env::var("SHELL").ok());
            send(
                &mut writer,
                &Request::AddStart {
                    cmd,
                    session,
                    ts_unix_ns: Some(now_unix_ns()),
                    cwd,
                    hostname: current_hostname(),
                    shell,
                },
            )?;
            // Fire-and-forget: no response expected.
        }
        Cmd::AddEnd { session, exit_code } => {
            send(
                &mut writer,
                &Request::AddEnd {
                    session,
                    exit_code,
                    ts_unix_ns: Some(now_unix_ns()),
                },
            )?;
        }
        Cmd::Import { from } => {
            let (path, source) = match from {
                ImportFrom::Zsh { path } => (path, "zsh"),
                ImportFrom::Bash { path } => (path, "bash"),
                ImportFrom::Atuin { path } => {
                    let p = path
                        .or_else(default_atuin_db_path)
                        .ok_or_else(|| anyhow!("can't locate atuin db; pass --path"))?;
                    (p, "atuin")
                }
            };
            let path_str = path
                .canonicalize()
                .with_context(|| format!("canonicalize {}", path.display()))?
                .display()
                .to_string();
            send(
                &mut writer,
                &Request::Import {
                    path: path_str,
                    source: source.into(),
                },
            )?;
            match read_response(&mut reader)? {
                Response::Imported { inserted } => println!("imported {inserted} entries"),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("import", &other)),
            }
        }
        Cmd::Init { .. } => unreachable!("Init returns at the prefix match"),
        Cmd::List { .. } | Cmd::Search { .. } | Cmd::Get { .. } => {
            unreachable!("read commands return from run_read at the prefix match")
        }
    }
    Ok(())
}

/// Run a read subcommand (`list`, `search` non-interactive, `get`) directly
/// against SQLite. The daemon is not consulted — these commands work even
/// when `munind` is down. WAL mode makes the concurrent read safe.
fn run_read(cmd: Cmd, db_override: Option<&Path>) -> Result<()> {
    let db_path = resolve_db_path(db_override)?;
    let conn = Connection::open(&db_path)
        .with_context(|| format!("open db {}", db_path.display()))?;
    match cmd {
        Cmd::List { filters, limit } => {
            let filters = filters.into_proto()?;
            let entries = storage::list(&conn, limit, &filters)?;
            print_table(&entries);
        }
        Cmd::Search {
            query,
            interactive: false,
            sort,
            filters,
            limit,
        } => {
            let filters = filters.into_proto()?;
            let entries =
                storage::search(&conn, &query.join(" "), sort, limit, &filters)?;
            print_table(&entries);
        }
        Cmd::Get { id } => match storage::get(&conn, id)? {
            Some(entry) => print_entry(&entry),
            None => return Err(anyhow!("no entry with id {id}")),
        },
        _ => unreachable!("run_read called with non-read command"),
    }
    Ok(())
}

fn resolve_db_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => default_db_path(),
    }
}

/// Open the fzf-style picker. Talks to SQLite directly (no daemon
/// dependency).
///
/// Exit-code contract (consumed by the shell hook in M5):
/// - `0` + cmd on stdout: Enter — run this command.
/// - `2` + cmd on stdout: Tab — drop on the line buffer for the user to edit.
/// - `1` + nothing on stdout: Esc / Ctrl-C — cancel.
fn run_tui(cmd: Cmd, db_override: Option<&Path>) -> Result<()> {
    let Cmd::Search {
        query,
        filters,
        // `sort` / `limit` are intentionally ignored — the TUI uses the
        // config file's sort and an interactive `limit` of its own
        // (config.limit).
        ..
    } = cmd
    else {
        unreachable!("run_tui only called for interactive search");
    };
    let cfg = config::load_or_default();
    let db_path = resolve_db_path(db_override)?;
    let filters = filters.into_proto()?;
    match tui::run(&db_path, query.join(" "), filters, &cfg)? {
        Outcome::Run(cmd) => {
            println!("{cmd}");
            Ok(())
        }
        Outcome::Edit(cmd) => {
            println!("{cmd}");
            std::process::exit(2);
        }
        Outcome::Cancel => std::process::exit(1),
    }
}

/// Where atuin stores its history by default: `$XDG_DATA_HOME/atuin/history.db`
/// (falling back to `$HOME/.local/share/atuin/history.db`).
fn default_atuin_db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("atuin").join("history.db"))
}

fn send<W: Write>(wr: &mut W, req: &Request) -> Result<()> {
    let mut json = serde_json::to_string(req).context("serialize request")?;
    json.push('\n');
    wr.write_all(json.as_bytes()).context("send request")?;
    wr.flush().context("flush socket")?;
    Ok(())
}

fn read_response<R: BufRead>(rd: &mut R) -> Result<Response> {
    let mut line = String::new();
    let n = rd.read_line(&mut line).context("read response line")?;
    if n == 0 {
        return Err(anyhow!("daemon closed connection without responding"));
    }
    serde_json::from_str(line.trim()).context("parse response")
}

fn unexpected(op: &str, resp: &Response) -> anyhow::Error {
    anyhow!("unexpected response to {op}: {:?}", resp)
}

fn print_table(entries: &[EntryMeta]) {
    if entries.is_empty() {
        eprintln!("(no entries)");
        return;
    }
    println!(
        "{:<6} {:<19} {:<5} {:>4} {:>8} CMD",
        "ID", "TIME", "SHELL", "EXIT", "DUR"
    );
    for e in entries {
        // `search` populates `snippet` with `‹match›` markers; `list`/`get`
        // leave it None and we fall back to the raw cmd.
        let raw = e.snippet.as_deref().unwrap_or(&e.cmd);
        let mut display: String = raw
            .chars()
            .take(80)
            .collect::<String>()
            .replace('\n', "\u{21B5}")
            .replace('\t', " ");
        // Truncation can land inside a `‹match›` pair, leaving a dangling
        // opener that visually merges with the next column. Close each
        // unbalanced opener so the row stays readable.
        let opens = display.matches('‹').count();
        let closes = display.matches('›').count();
        for _ in 0..opens.saturating_sub(closes) {
            display.push('›');
        }
        println!(
            "{:<6} {:<19} {:<5} {:>4} {:>8} {}",
            e.id,
            fmt_ts(e.ts_unix_ns),
            e.shell.as_deref().unwrap_or("-"),
            e.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            fmt_dur(e.duration_ms),
            display,
        );
    }
}

fn print_entry(e: &EntryMeta) {
    println!("id:          {}", e.id);
    println!("uuid:        {}", e.uuid);
    println!("time:        {}", fmt_ts(e.ts_unix_ns));
    if let Some(h) = &e.hostname {
        println!("host:        {h}");
    }
    if let Some(s) = &e.shell {
        println!("shell:       {s}");
    }
    if let Some(s) = &e.session {
        println!("session:     {s}");
    }
    if let Some(c) = &e.cwd {
        println!("cwd:         {c}");
    }
    if let Some(rc) = e.exit_code {
        println!("exit_code:   {rc}");
    }
    if let Some(d) = e.duration_ms {
        println!("duration_ms: {d}");
    }
    println!("cmd:         {}", e.cmd);
}

fn fmt_ts(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    match Local.timestamp_opt(secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("@{secs}"),
    }
}

