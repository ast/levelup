# levelup

A Rust workspace of two personal-computing tools:

- **munin** — atuin-like shell-history search with an fzf-style TUI. Daemon + CLI + interactive picker, fuzzy matching via [nucleo-matcher], SQLite storage.
- **hugin** — Wayland clipboard manager for sway/wlroots compositors. Captures every selection change to a local SQLite DB.

Both crates target Rust 2024 (resolver 3), share workspace dependencies, and are built independently.

[nucleo-matcher]: https://crates.io/crates/nucleo-matcher

---

## munin

A shell-history daemon (`munind`) and CLI (`munin`) that captures every command you run, indexes it for fuzzy search, and exposes an fzf-style interactive picker on Ctrl-R.

### Install

```sh
# from the repo root
cargo build --release -p munin
install -Dm755 target/release/munind ~/.local/bin/munind
install -Dm755 target/release/munin  ~/.local/bin/munin
```

Then install the systemd user unit so `munind` runs in the background:

```sh
install -Dm644 dist/munind.service ~/.config/systemd/user/munind.service
systemctl --user daemon-reload
systemctl --user enable --now munind
systemctl --user status munind        # should show "active (running)"
```

The daemon listens on `$XDG_RUNTIME_DIR/munin.sock` and writes to `$XDG_DATA_HOME/munin/munin.db` (default `~/.local/share/munin/munin.db`).

### Shell setup

Add to `~/.zshrc`:

```sh
eval "$(munin init zsh)"
```

…or to `~/.bashrc`:

```sh
eval "$(munin init bash)"
```

This wires up two things:

1. **Capture hooks** — every command you run is recorded (cmd, cwd, hostname, session, shell, exit code, duration). Commands prefixed with a space are skipped on zsh (the standard `HIST_IGNORE_SPACE` convention).
2. **Ctrl-R picker** — replaces your shell's native history search with the munin TUI:
   - **Enter** → splice and run the chosen command (zsh only; see bash caveat below).
   - **Tab** → splice the command onto the prompt for editing, don't run.
   - **Esc** / **Ctrl-C** → leave the prompt untouched.

The TUI bypasses the daemon and reads SQLite directly, so it works even if `munind` is down.

**Bash caveat:** `bind -x` (the only way to wire a Rust binary into readline) cannot trigger Enter from inside the bound function. On bash, both Enter and Tab in the picker land the chosen command on the prompt and you hit Enter yourself to run it. zsh honours the run/edit distinction natively.

### Importing existing history

```sh
munin import atuin                       # from ~/.local/share/atuin/history.db
munin import atuin --path /custom/path   # override
munin import zsh   ~/.zsh_history
munin import bash  ~/.bash_history
```

Imports go through the daemon (writes serialize through the storage thread). Re-imports are idempotent — atuin's UUIDv7 ids are preserved as munin's `uuid`, and shell-history files dedupe on the same key.

### Searching from the CLI

These don't need the daemon — they open SQLite directly:

```sh
munin search gcm                         # fuzzy: matches "git commit -m …"
munin search "cargo test" --limit 20
munin list --limit 20                    # most-recent first
munin list --cwd "$PWD" --shell zsh
munin get 1234                           # one entry's metadata
```

Filters available on `list` / `search`: `--cwd`, `--session`, `--shell`, `--since`, `--until` (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`, local time), `--limit`.

`munin search -i [QUERY]` opens the interactive TUI seeded with the query — same picker the Ctrl-R hook invokes.

### Config

Optional TOML file at `$XDG_CONFIG_HOME/munin/config.toml`. All keys default if absent; bad TOML logs a warning and falls back to defaults (the TUI never refuses to open over a config error).

```toml
sort   = "relevance"     # "relevance" | "recent"
limit  = 200             # rows fetched per keystroke
layout = "bottom"        # "bottom" (fzf-style) | "top"

[colors]
selection_fg = "black"
selection_bg = "cyan"
match_fg     = "yellow"
prompt_fg    = "green"
status_fg    = "gray"
```

Colours accept the named ANSI palette (`black`/`red`/.../`gray`/`darkgray`/`light*`).

### Daemon vs CLI — what needs the daemon

Only writes and the liveness probe go through `munind`:

| Command                             | Path           | Needs daemon? |
| ----------------------------------- | -------------- | ------------- |
| `munin add-start` / `munin add-end` | unix socket    | yes (writes)  |
| `munin import`                      | unix socket    | yes (writes)  |
| `munin ping`                        | unix socket    | yes           |
| `munin list` / `search` / `get`     | direct SQLite  | no            |
| `munin search -i` (TUI)             | direct SQLite  | no            |
| `munin init <shell>`                | embedded script | no           |

WAL mode makes concurrent reads safe alongside the daemon's writes.

### Logging

`tracing` to stderr, gated by `MUNIN_LOG` (full `tracing-subscriber` EnvFilter syntax). Default is `info`.

```sh
MUNIN_LOG=debug munind          # foreground, verbose
journalctl --user -u munind -f  # follow the systemd unit
```

---

## hugin

Wayland clipboard manager for sway / hyprland / river (anything supporting `wlr-data-control-unstable-v1`). Persists every selection change to SQLite with dedup and retention.

Not covered in detail here — see `dist/hugind.service` and `cargo run --bin hugind -p hugin --help` for the rough shape. Status: working, separate scope from munin.

---

## Development

Run all commands from the workspace root:

```sh
cargo check  --workspace
cargo build  --workspace
cargo build  --workspace --release
cargo clippy --workspace --all-targets
cargo fmt    --all

cargo run --bin munind -p munin                       # run the daemon
MUNIN_LOG=debug cargo run --bin munind -p munin       # verbose

cargo run --bin hugind -p hugin                       # the other daemon
```

### Project layout

```
levelup/
├── Cargo.toml              # workspace root, shared deps
├── README.md               # you are here
├── CLAUDE.md               # codebase notes / invariants
├── dist/
│   ├── munind.service      # systemd user unit
│   └── hugind.service
├── munin/
│   └── src/
│       ├── bin/munind.rs   # daemon entry
│       ├── bin/munin.rs    # CLI entry
│       ├── storage.rs      # SQLite, schema, importers, search
│       ├── tui.rs          # ratatui picker (direct-SQLite)
│       ├── ipc.rs          # unix socket, JSON-lines protocol
│       ├── proto.rs        # wire types
│       ├── config.rs       # ~/.config/munin/config.toml
│       └── shells/         # zsh.sh / bash.sh hook scripts
└── hugin/
    └── src/                # similar shape; see CLAUDE.md
```
