use std::sync::mpsc;

use anyhow::{Context, Result};
use clap::Parser;
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use munin::cli::DaemonArgs;
use munin::storage::{Store, StoreCmd, spawn_storage_thread};
use munin::{init_tracing, ipc};

fn main() -> Result<()> {
    let args = DaemonArgs::parse();
    init_tracing();
    info!("munin daemon starting");

    let db_path = args.db_path()?;
    let socket_path = args.socket_path();

    // Open the store synchronously so a schema-version mismatch aborts the
    // daemon with a clear error before anything else starts.
    let store = Store::open(&db_path)?;
    info!(db = %db_path.display(), "storage opened");

    let (store_tx, store_rx) = mpsc::channel::<StoreCmd>();
    let storage = spawn_storage_thread(store, store_rx)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("munin-ipc")
        .enable_io()
        .build()
        .context("build tokio runtime")?;

    runtime.spawn({
        let socket_path = socket_path.clone();
        let db_path = db_path.clone();
        async move {
            if let Err(e) = ipc::serve(socket_path, db_path, store_tx).await {
                warn!(error = %e, "ipc server stopped");
            }
        }
    });

    // No-op when NOTIFY_SOCKET isn't set (i.e. not run under systemd).
    let _ = sd_notify::notify(false, &[NotifyState::Ready]);

    runtime.block_on(wait_for_shutdown_signal());
    info!("shutdown signal received");
    let _ = sd_notify::notify(false, &[NotifyState::Stopping]);

    // Dropping the runtime cancels the IPC server and every per-connection
    // task; all Sender<StoreCmd> clones go with them, which closes the
    // storage thread's recv loop.
    drop(runtime);
    let _ = std::fs::remove_file(&socket_path);
    let _ = storage.join();
    info!("munin daemon stopped");
    Ok(())
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
