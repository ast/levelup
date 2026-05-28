use std::sync::mpsc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use hugin::cli::DaemonArgs;
use hugin::storage::spawn_storage_thread;
use hugin::wayland::WaylandCmd;
use hugin::{init_tracing, ipc, wayland, CapturedEntry};

fn main() -> Result<()> {
    let args = DaemonArgs::parse();
    init_tracing();
    info!("hugin daemon starting");

    let db_path = args.db_path()?;
    let socket_path = args.socket_path();

    let (capture_tx, capture_rx) = mpsc::channel::<CapturedEntry>();
    let storage = spawn_storage_thread(db_path.clone(), capture_rx, args.retention())?;

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
    // Drop our own copy of cmd_tx so the wayland thread sees disconnect if
    // the IPC server stops cleanly. (Not strictly important until graceful
    // shutdown lands in M4.)
    drop(cmd_tx);

    let dispatch_result = wayland::run(capture_tx, cmd_rx);

    drop(runtime);
    let _ = std::fs::remove_file(&socket_path);
    let _ = storage.join();
    dispatch_result
}
