use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use hugin::cli::DaemonArgs;
use hugin::storage::{Store, spawn_storage_thread};
use hugin::wayland::WaylandCmd;
use hugin::{CapturedEntry, init_tracing, ipc, wayland};

fn main() -> Result<()> {
    let args = DaemonArgs::parse();

    if let Some(shell) = args.generate_completions {
        let mut cmd = DaemonArgs::command();
        clap_complete::generate(shell, &mut cmd, "hugind", &mut std::io::stdout());
        return Ok(());
    }

    init_tracing();
    info!("hugin daemon starting");

    let db_path = args.db_path()?;
    let socket_path = args.socket_path();
    let watch_primary = args.primary;

    // Open the store synchronously so a schema-version mismatch aborts the
    // daemon with a clear error before anything else starts.
    let store = Store::open(&db_path)?;
    info!(db = %db_path.display(), "storage opened");

    let (capture_tx, capture_rx) = mpsc::channel::<CapturedEntry>();
    let storage = spawn_storage_thread(store, capture_rx, args.retention())?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<WaylandCmd>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hugin-ipc")
        .enable_io()
        .build()
        .context("build tokio runtime")?;

    runtime.spawn({
        let socket_path = socket_path.clone();
        let db_path = db_path.clone();
        let cmd_tx = cmd_tx.clone();
        async move {
            if let Err(e) = ipc::serve(socket_path, db_path, cmd_tx).await {
                warn!(error = %e, "ipc server stopped");
            }
        }
    });
    drop(cmd_tx);

    let shutdown = Arc::new(AtomicBool::new(false));
    runtime.spawn({
        let shutdown = shutdown.clone();
        async move {
            wait_for_shutdown_signal().await;
            info!("shutdown signal received");
            let _ = sd_notify::notify(false, &[NotifyState::Stopping]);
            shutdown.store(true, Ordering::Relaxed);
        }
    });

    // No-op when NOTIFY_SOCKET isn't set (i.e. not run under systemd).
    let _ = sd_notify::notify(false, &[NotifyState::Ready]);

    let dispatch_result = wayland::run(capture_tx, cmd_rx, watch_primary, shutdown);

    drop(runtime);
    let _ = std::fs::remove_file(&socket_path);
    let _ = storage.join();
    dispatch_result
}

async fn wait_for_shutdown_signal() {
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not install SIGTERM handler");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not install SIGINT handler");
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => info!("SIGTERM"),
        _ = sigint.recv() => info!("SIGINT"),
    }
}
