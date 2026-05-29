//! `hugin` CLI — query the running daemon over its unix socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use hugin::proto::{EntryMeta, Request, Response, SearchSort};
use hugin::tui::{self, Outcome};
use hugin::{config, default_socket_path, fmt_ts, human_size, storage};

#[derive(Parser)]
#[command(name = "hugin", version, about = "Query the hugin clipboard daemon")]
struct Cli {
    /// Override the daemon socket path. Default: $XDG_RUNTIME_DIR/hugin.sock
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// SQLite database path for the interactive picker's direct reads.
    /// Default: $XDG_DATA_HOME/hugin/hugin.db
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Verify the daemon is responsive
    Ping,
    /// List recent clipboard entries (newest first)
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, value_parser = ["regular", "primary"])]
        selection: Option<String>,
    },
    /// Show metadata for a single entry
    #[command(visible_alias = "stat")]
    Info { id: i64 },
    /// Write the contents of an entry to stdout
    #[command(visible_alias = "cat")]
    Get {
        id: i64,
        /// MIME to fetch. Defaults to first `text/*`, else first available.
        #[arg(long)]
        mime: Option<String>,
    },
    /// Put an old entry back onto the clipboard
    #[command(visible_alias = "cp")]
    Copy {
        id: i64,
        #[arg(long, value_parser = ["regular", "primary"])]
        selection: Option<String>,
    },
    /// Fuzzy-search clipboard history (fzf-style scoring via nucleo-matcher).
    /// Pass -i to open an interactive picker instead of printing a table.
    #[command(visible_alias = "s")]
    Search {
        /// Query string. Joined with spaces.
        query: Vec<String>,
        /// Open the interactive fzf-style picker seeded with QUERY. Reads
        /// SQLite directly (works even when the daemon is down); Enter copies
        /// the chosen entry, Tab prints it, Ctrl-X deletes it.
        #[arg(short, long)]
        interactive: bool,
        /// Result ordering (non-interactive mode; the picker reads the config).
        #[arg(long, value_enum, default_value_t = SearchSort::Relevance)]
        sort: SearchSort,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, value_parser = ["regular", "primary"])]
        selection: Option<String>,
    },
    /// Print a shell-completion script for SHELL to stdout
    #[command(visible_alias = "comp")]
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Completions don't talk to the daemon; handle before any socket connect.
    if let Cmd::Completions { shell } = cli.cmd {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "hugin", &mut std::io::stdout());
        return Ok(());
    }

    let socket_path = cli.socket.clone().unwrap_or_else(default_socket_path);

    // The interactive picker reads SQLite directly and only contacts the
    // daemon for copy/delete — so it must not require a live socket up front.
    if let Cmd::Search {
        interactive: true, ..
    } = &cli.cmd
    {
        return run_tui(cli.cmd, cli.db.as_deref(), &socket_path);
    }

    let path = socket_path;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to hugin daemon at {}", path.display()))?;
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
        Cmd::List { limit, selection } => {
            send(
                &mut writer,
                &Request::List {
                    limit: Some(limit),
                    selection,
                },
            )?;
            match read_response(&mut reader)? {
                Response::Entries { entries } => print_table(&entries),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("list", &other)),
            }
        }
        Cmd::Info { id } => {
            send(&mut writer, &Request::Get { id })?;
            match read_response(&mut reader)? {
                Response::Entry { entry } => print_info(&entry),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("info", &other)),
            }
        }
        Cmd::Get { id, mime } => {
            send(&mut writer, &Request::ReadBlob { id, mime })?;
            match read_response(&mut reader)? {
                Response::BlobHeader { mime: _, len } => {
                    let mut buf = vec![0u8; len];
                    reader.read_exact(&mut buf).context("read blob body")?;
                    std::io::stdout()
                        .write_all(&buf)
                        .context("write blob to stdout")?;
                }
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("get", &other)),
            }
        }
        Cmd::Copy { id, selection } => {
            send(
                &mut writer,
                &Request::Copy {
                    id,
                    selection,
                    mime: None,
                },
            )?;
            match read_response(&mut reader)? {
                Response::Ok => {}
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("copy", &other)),
            }
        }
        Cmd::Search {
            query,
            interactive: false,
            sort,
            limit,
            selection,
        } => {
            send(
                &mut writer,
                &Request::Search {
                    query: query.join(" "),
                    sort,
                    limit: Some(limit),
                    selection,
                },
            )?;
            match read_response(&mut reader)? {
                Response::Entries { entries } => print_table(&entries),
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("search", &other)),
            }
        }
        Cmd::Search {
            interactive: true, ..
        } => unreachable!("handled before connecting"),
        Cmd::Completions { .. } => unreachable!("handled before connecting"),
    }
    Ok(())
}

/// Open the fzf-style picker. Reads SQLite directly (no daemon dependency for
/// search/preview); copy and delete round-trip to the daemon. Exit codes:
/// `0` = copied or printed, `1` = cancelled.
fn run_tui(cmd: Cmd, db_override: Option<&Path>, socket_path: &Path) -> Result<()> {
    let Cmd::Search {
        query,
        selection,
        // `sort` / `limit` are intentionally ignored here — the picker uses
        // the config file's sort and its own `limit`.
        ..
    } = cmd
    else {
        unreachable!("run_tui only called for interactive search");
    };
    let cfg = config::load_or_default();
    let db_path = match db_override {
        Some(p) => p.to_path_buf(),
        None => storage::default_db_path()?,
    };
    match tui::run(&db_path, socket_path, query.join(" "), selection, &cfg)? {
        Outcome::Copied => Ok(()),
        Outcome::Print(id) => print_blob(&db_path, id),
        Outcome::Cancel => std::process::exit(1),
    }
}

/// Read the chosen entry's content straight from SQLite and write it to
/// stdout (the picker's Tab action). Picks the first `text/*` MIME, else the
/// first available — same rule as `hugin get`.
fn print_blob(db_path: &Path, id: i64) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("open db {}", db_path.display()))?;
    match storage::read_blob(&conn, id, None)? {
        Some((_mime, blob)) => std::io::stdout()
            .write_all(&blob)
            .context("write blob to stdout"),
        None => Err(anyhow!("no content for entry {id}")),
    }
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
        "{:<6} {:<19} {:<8} {:<9} {}",
        "ID", "TIME", "SEL", "SIZE", "SNIPPET"
    );
    for e in entries {
        let snippet = e.snippet.as_deref().unwrap_or("");
        let mut label: String = snippet
            .chars()
            .take(60)
            .collect::<String>()
            .replace('\n', "\u{21B5}")
            .replace('\t', " ");
        // Search snippets carry ‹match› markers. If truncation lands inside
        // a pair we'd render a dangling ‹ — append a closer for each
        // unclosed opener so the row stays balanced.
        let opens = label.matches('‹').count();
        let closes = label.matches('›').count();
        for _ in 0..opens.saturating_sub(closes) {
            label.push('›');
        }
        let display = if label.is_empty() {
            format!("({} MIMEs)", e.mimes.len())
        } else {
            label
        };
        println!(
            "{:<6} {:<19} {:<8} {:<9} {}",
            e.id,
            fmt_ts(e.ts_unix_ns),
            e.selection,
            human_size(e.size_bytes),
            display
        );
    }
}

fn print_info(e: &EntryMeta) {
    println!("id:        {}", e.id);
    println!("time:      {}", fmt_ts(e.ts_unix_ns));
    println!("selection: {}", e.selection);
    println!(
        "size:      {} bytes ({})",
        e.size_bytes,
        human_size(e.size_bytes)
    );
    println!("mimes:");
    for m in &e.mimes {
        println!("  - {m}");
    }
    if let Some(s) = &e.snippet {
        println!("snippet:   {}", s.chars().take(200).collect::<String>());
    }
}
