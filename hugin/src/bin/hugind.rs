use std::sync::mpsc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use hugin::cli::DaemonArgs;
use hugin::storage::spawn_storage_thread;
use hugin::{init_tracing, ipc, wayland, CapturedEntry};

fn main() -> Result<()> {
    let args = DaemonArgs::parse();
    init_tracing();
    info!("hugin daemon starting");

    let db_path = args.db_path()?;
    let socket_path = args.socket_path();

    let (tx, rx) = mpsc::channel::<CapturedEntry>();
    let storage = spawn_storage_thread(db_path.clone(), rx, args.retention())?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hugin-ipc")
        .enable_io()
        .build()
        .context("build tokio runtime")?;

    runtime.spawn({
        let socket_path = socket_path.clone();
        async move {
            if let Err(e) = ipc::serve(socket_path, db_path).await {
                warn!(error = %e, "ipc server stopped");
            }
        }
    });

    let dispatch_result = wayland::run(tx);

    // wayland loop has exited — drop the runtime to cancel IPC tasks, then
    // unlink the socket and join the storage thread.
    drop(runtime);
    let _ = std::fs::remove_file(&socket_path);
    let _ = storage.join();
    dispatch_result
}
