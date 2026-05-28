# Show available recipes
default:
    @just --list

# --- Workspace ---

# Type-check everything
check:
    cargo check --workspace

# Debug build
build:
    cargo build --workspace

# Release build
release:
    cargo build --workspace --release

# Clippy with warnings treated as errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format every Rust file
fmt:
    cargo fmt --all

# Verify formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# --- hugin ---

# Run hugind against your real database. HUGIN_LOG controls log level.
run:
    cargo run --bin hugind -p hugin

# Inspect the default hugin database
schema:
    cargo run --example schema -p hugin

# Inspect a specific database file
schema-of DB:
    cargo run --example schema -p hugin -- {{DB}}

# Run the hugin CLI (forward args after `--`, e.g. `just hugin -- list`)
hugin *ARGS:
    cargo run --quiet --bin hugin -p hugin -- {{ARGS}}

# End-to-end smoke test against a temp DB + socket (overwrites your clipboard; needs wl-clipboard)
smoke: build
    #!/usr/bin/env bash
    set -uo pipefail
    DB=/tmp/hugin-smoke.db
    SOCK=/tmp/hugin-smoke.sock
    LOG=/tmp/hugin-smoke.log
    rm -f "$DB" "$DB-wal" "$DB-shm" "$SOCK" "$LOG"

    echo ">> starting hugind (log: $LOG)"
    HUGIN_LOG=debug ./target/debug/hugind --db "$DB" --socket "$SOCK" 2>"$LOG" &
    PID=$!
    trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT
    sleep 0.5
    if ! kill -0 $PID 2>/dev/null; then
        echo "hugind exited at startup:"
        cat "$LOG"
        exit 1
    fi

    echo ">> regular: 'hello hugin'"
    printf 'hello hugin' | wl-copy
    sleep 0.15

    echo ">> regular: 'second copy' twice (second should dedup)"
    printf 'second copy' | wl-copy; sleep 0.15
    printf 'second copy' | wl-copy; sleep 0.15

    echo ">> primary: 'primary one'"
    printf 'primary one' | wl-copy --primary
    sleep 0.2

    echo ">> ipc: ping"
    ./target/debug/hugin --socket "$SOCK" ping

    echo ">> ipc: list"
    ./target/debug/hugin --socket "$SOCK" list

    LAST=$(./target/debug/hugin --socket "$SOCK" list --limit 1 | tail -1 | awk '{print $1}')
    echo ">> ipc: info $LAST"
    ./target/debug/hugin --socket "$SOCK" info "$LAST"
    echo ">> ipc: get $LAST"
    ./target/debug/hugin --socket "$SOCK" get "$LAST"; echo

    echo ">> stopping daemon"
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    trap - EXIT

    echo
    stored=$(grep -c 'stored ' "$LOG" || true)
    dedup=$(grep -c 'dedup:'  "$LOG" || true)
    echo "summary: stored=$stored dedup=$dedup"
    echo "  expected (clean clipboard at start): stored=3 dedup=1"
    echo "  (extra 'stored' entries are whatever was on your clipboard at startup)"
