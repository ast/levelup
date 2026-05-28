use std::sync::mpsc;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use hugin::cli::DaemonArgs;
use hugin::storage::spawn_storage_thread;
use hugin::{init_tracing, wayland, CapturedEntry};

fn main() -> Result<()> {
    let args = DaemonArgs::parse();
    init_tracing();
    info!("hugin daemon starting");

    let (tx, rx) = mpsc::channel::<CapturedEntry>();
    let storage = spawn_storage_thread(args.db_path()?, rx, args.retention())?;

    let dispatch_result = wayland::run(tx);
    let _ = storage.join();
    dispatch_result
}
