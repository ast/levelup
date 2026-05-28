//! `hugin` CLI — query the running daemon over its unix socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use chrono::{Local, TimeZone};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use hugin::default_socket_path;
use hugin::proto::{EntryMeta, Request, Response};

#[derive(Parser)]
#[command(
    name = "hugin",
    version,
    about = "Query the hugin clipboard daemon"
)]
struct Cli {
    /// Override the daemon socket path. Default: $XDG_RUNTIME_DIR/hugin.sock
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

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

    let path = cli.socket.unwrap_or_else(default_socket_path);
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
            send(&mut writer, &Request::Copy { id, selection })?;
            match read_response(&mut reader)? {
                Response::Ok => {}
                Response::Error { message } => return Err(anyhow!("{message}")),
                other => return Err(unexpected("copy", &other)),
            }
        }
        Cmd::Completions { .. } => unreachable!("handled before connecting"),
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
        "{:<6} {:<19} {:<8} {:<9} {}",
        "ID", "TIME", "SEL", "SIZE", "PREVIEW"
    );
    for e in entries {
        let preview = e.preview.as_deref().unwrap_or("");
        let preview: String = preview
            .chars()
            .take(60)
            .collect::<String>()
            .replace('\n', "\u{21B5}")
            .replace('\t', " ");
        let label = if preview.is_empty() {
            format!("({} MIMEs)", e.mimes.len())
        } else {
            preview
        };
        println!(
            "{:<6} {:<19} {:<8} {:<9} {}",
            e.id,
            fmt_ts(e.ts_unix_ns),
            e.selection,
            human_size(e.size_bytes),
            label
        );
    }
}

fn print_info(e: &EntryMeta) {
    println!("id:        {}", e.id);
    println!("time:      {}", fmt_ts(e.ts_unix_ns));
    println!("selection: {}", e.selection);
    println!("size:      {} bytes ({})", e.size_bytes, human_size(e.size_bytes));
    println!("mimes:");
    for m in &e.mimes {
        println!("  - {m}");
    }
    if let Some(p) = &e.preview {
        println!("preview:   {}", p.chars().take(200).collect::<String>());
    }
}

fn fmt_ts(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    match Local.timestamp_opt(secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("@{secs}"),
    }
}

fn human_size(n: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = KB * 1024;
    if n >= MB {
        format!("{:.1}M", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}K", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

