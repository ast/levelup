//! `munin` CLI — talks to the running daemon over its unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use clap::{Parser, Subcommand};

use munin::config;
use munin::proto::{EntryMeta, Filters, Request, Response, SearchSort};
use munin::shells::{self, Shell};
use munin::storage::default_db_path;
use munin::{current_hostname, default_socket_path, now_unix_ns, tui};

#[derive(Parser)]
#[command(name = "munin", version, about = "Query the munin shell-history daemon")]
struct Cli {
    /// Override the daemon socket path. Default: $XDG_RUNTIME_DIR/munin.sock
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

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
    /// Full-text search across recorded commands (FTS5). Pass -i to open
    /// an fzf-style interactive picker instead of a one-shot query.
    #[command(visible_alias = "s")]
    Search {
        /// Query terms. Joined with spaces and matched as a single phrase
        /// unless --raw is given. With -i, used as the initial query.
        query: Vec<String>,
        /// Open the interactive TUI seeded with QUERY (if any). Print the
        /// chosen command to stdout on Enter; exit silently on Esc.
        #[arg(short, long)]
        interactive: bool,
        /// Pass the query through to FTS5 verbatim (enables operators like
        /// AND, OR, NEAR, prefix*, "exact phrase"). Non-interactive only.
        #[arg(long)]
        raw: bool,
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
    /// Import an existing shell-history file (.zsh_history / .bash_history).
    Import {
        /// Path to the history file.
        path: PathBuf,
        /// Which format to parse it as.
        #[arg(value_enum)]
        shell: Shell,
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

    // Two subcommands skip the daemon entirely:
    //   - `init` (just prints an embedded script)
    //   - `search -i` (TUI reads SQLite directly; works when munind is down)
    match &cli.cmd {
        Cmd::Init { shell } => {
            print!("{}", shells::init_script(*shell));
            return Ok(());
        }
        Cmd::Search {
            interactive: true, ..
        } => return run_tui(cli.cmd),
        _ => {}
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
        Cmd::List { filters, limit } => {
            let filters = filters.into_proto()?;
            send(
                &mut writer,
                &Request::List {
                    limit: Some(limit),
                    filters,
                },
            )?;
            match read_response(&mut reader)? {
                Response::Entries { entries } => print_table(&entries),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("list", &other)),
            }
        }
        Cmd::Search {
            query,
            interactive: _, // handled above
            raw,
            sort,
            filters,
            limit,
        } => {
            let filters = filters.into_proto()?;
            send(
                &mut writer,
                &Request::Search {
                    query: query.join(" "),
                    raw,
                    sort,
                    limit: Some(limit),
                    filters,
                },
            )?;
            match read_response(&mut reader)? {
                Response::Entries { entries } => print_table(&entries),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("search", &other)),
            }
        }
        Cmd::Get { id } => {
            send(&mut writer, &Request::Get { id })?;
            match read_response(&mut reader)? {
                Response::Entry { entry } => print_entry(&entry),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("get", &other)),
            }
        }
        Cmd::Import { path, shell } => {
            let path_str = path
                .canonicalize()
                .with_context(|| format!("canonicalize {}", path.display()))?
                .display()
                .to_string();
            send(
                &mut writer,
                &Request::Import {
                    path: path_str,
                    shell: match shell {
                        Shell::Zsh => "zsh".into(),
                        Shell::Bash => "bash".into(),
                    },
                },
            )?;
            match read_response(&mut reader)? {
                Response::Imported { inserted } => println!("imported {inserted} entries"),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("import", &other)),
            }
        }
        Cmd::Init { .. } => unreachable!("handled before connecting"),
    }
    Ok(())
}

/// Open the fzf-style picker. Talks to SQLite directly (no daemon
/// dependency); prints the chosen command to stdout on Enter, or nothing
/// on Esc / Ctrl-C.
fn run_tui(cmd: Cmd) -> Result<()> {
    let Cmd::Search {
        query,
        filters,
        // `raw` / `sort` / `limit` are intentionally ignored — the TUI uses
        // the config file's sort and an interactive `limit` of its own
        // (config.limit). Phrase escaping is always on in the TUI; users who
        // want FTS5 operators can fall back to non-interactive `search --raw`.
        ..
    } = cmd
    else {
        unreachable!("run_tui only called for interactive search");
    };
    let cfg = config::load_or_default();
    let db_path = default_db_path()?;
    let filters = filters.into_proto()?;
    let selected = tui::run(&db_path, query.join(" "), filters, &cfg)?;
    if let Some(cmd) = selected {
        println!("{cmd}");
    }
    Ok(())
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

fn fmt_dur(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        let s = ms / 1_000;
        format!("{}m{:02}s", s / 60, s % 60)
    }
}
