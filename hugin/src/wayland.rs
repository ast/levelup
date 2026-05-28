//! Wayland event loop: binds wlr-data-control, captures new selections into
//! the storage channel, and (when asked by the IPC layer) becomes a clipboard
//! data source for `hugin copy`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::mpsc;

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::pipe2;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::{CapturedEntry, Selection};

/// MIME advertised by password managers (KeePassXC, Bitwarden, 1Password, …)
/// to tell clipboard managers "this selection is a secret; do not persist it".
const PASSWORD_HINT_MIME: &str = "x-kde-passwordManagerHint";
const PASSWORD_HINT_VALUE: &[u8] = b"secret";

/// Commands sent from the IPC layer (tokio tasks) to the wayland thread.
pub enum WaylandCmd {
    Copy {
        selection: Selection,
        parts: Vec<(String, Vec<u8>)>,
        reply: oneshot::Sender<Result<()>>,
    },
}

pub type CmdSender = mpsc::Sender<WaylandCmd>;
pub type CmdReceiver = mpsc::Receiver<WaylandCmd>;

/// Wayland-thread state kept across event dispatches.
struct State {
    /// MIMEs accumulated per pending offer object, keyed by the proxy itself.
    offers: HashMap<ZwlrDataControlOfferV1, Vec<String>>,
    /// Sender to the storage thread.
    capture_tx: mpsc::Sender<CapturedEntry>,
    /// Active data sources we own (one per `hugin copy` call that hasn't been
    /// preempted yet).
    sources: HashMap<ZwlrDataControlSourceV1, SourceData>,
    manager: ZwlrDataControlManagerV1,
    device: ZwlrDataControlDeviceV1,
}

struct SourceData {
    selection: Selection,
    blobs: HashMap<String, Vec<u8>>,
}

impl State {
    fn handle_selection(
        &mut self,
        sel: Selection,
        offer: Option<ZwlrDataControlOfferV1>,
        conn: &Connection,
    ) {
        let Some(offer) = offer else {
            debug!(sel = sel.as_str(), "selection cleared");
            return;
        };
        // If we're the active data source for this selection, the compositor
        // is mirroring our own set_selection back. Trying to `receive` from
        // it would deadlock: handle_selection would block on the pipe read
        // while waiting for the dispatch loop to deliver the matching `Send`
        // event to our own source. Drop the offer and move on.
        if self.sources.values().any(|s| s.selection == sel) {
            debug!(sel = sel.as_str(), "ignoring echo of our own selection");
            self.offers.remove(&offer);
            offer.destroy();
            return;
        }
        let mimes = self.offers.remove(&offer).unwrap_or_default();
        if mimes.is_empty() {
            warn!(sel = sel.as_str(), "offer committed with no MIMEs");
            offer.destroy();
            return;
        }

        // Honour the password-manager hint: if the source advertises one
        // and its content reads back as the literal "secret", skip the entire
        // offer before touching any of the (possibly sensitive) other MIMEs.
        if mimes.iter().any(|m| m == PASSWORD_HINT_MIME) {
            match read_offer(&offer, PASSWORD_HINT_MIME, conn) {
                Ok(bytes) if bytes.trim_ascii() == PASSWORD_HINT_VALUE => {
                    info!(
                        sel = sel.as_str(),
                        "skipping clipboard content marked as password-manager secret"
                    );
                    offer.destroy();
                    return;
                }
                Ok(_) => {
                    // Hint present but value isn't "secret"; treat normally.
                }
                Err(e) => warn!(error = %e, "failed to read password-hint MIME"),
            }
        }

        let mut parts = Vec::with_capacity(mimes.len());
        for mime in &mimes {
            match read_offer(&offer, mime, conn) {
                Ok(bytes) => parts.push((mime.clone(), bytes)),
                Err(e) => warn!(sel = sel.as_str(), %mime, error = %e, "failed to read offer"),
            }
        }
        offer.destroy();

        if parts.is_empty() {
            warn!(sel = sel.as_str(), "no readable MIMEs in offer");
            return;
        }

        let entry = CapturedEntry::now(sel, parts);
        if let Err(e) = self.capture_tx.send(entry) {
            warn!(error = %e, "storage channel closed; dropping capture");
        }
    }

    fn handle_cmd(&mut self, cmd: WaylandCmd, conn: &Connection, qh: &QueueHandle<Self>) {
        match cmd {
            WaylandCmd::Copy {
                selection,
                parts,
                reply,
            } => {
                let result = self.do_copy(selection, parts, conn, qh);
                let _ = reply.send(result);
            }
        }
    }

    fn do_copy(
        &mut self,
        sel: Selection,
        parts: Vec<(String, Vec<u8>)>,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        if parts.is_empty() {
            anyhow::bail!("nothing to copy: entry has no MIME parts");
        }
        let source = self.manager.create_data_source(qh, ());
        for (mime, _) in &parts {
            source.offer(mime.clone());
        }
        match sel {
            Selection::Regular => self.device.set_selection(Some(&source)),
            Selection::Primary => self.device.set_primary_selection(Some(&source)),
        }
        let blobs: HashMap<String, Vec<u8>> = parts.into_iter().collect();
        let mime_count = blobs.len();
        self.sources.insert(
            source,
            SourceData {
                selection: sel,
                blobs,
            },
        );
        conn.flush().context("flush after set_selection")?;
        info!(sel = sel.as_str(), mimes = mime_count, "became clipboard owner");
        Ok(())
    }
}

fn read_offer(
    offer: &ZwlrDataControlOfferV1,
    mime: &str,
    conn: &Connection,
) -> Result<Vec<u8>> {
    let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC).context("pipe2")?;
    offer.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd);
    conn.flush().context("flush wayland connection")?;

    let mut file: std::fs::File = read_fd.into();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).context("read offer pipe")?;
    Ok(buf)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id, Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(Selection::Regular, id, conn);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                state.handle_selection(Selection::Primary, id, conn);
            }
            zwlr_data_control_device_v1::Event::Finished => {
                debug!("data device finished by compositor");
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.clone()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let Some(data) = state.sources.get(source) else {
                    warn!(%mime_type, "send for unknown source; dropping");
                    return;
                };
                let Some(blob) = data.blobs.get(&mime_type) else {
                    warn!(%mime_type, "consumer asked for a mime we did not offer");
                    return;
                };
                // Writing inline blocks the dispatch loop until the consumer
                // drains the pipe. For typical text payloads (KBs) this fits
                // in the pipe buffer and returns immediately; very large
                // blobs could stall hugin. Address with incremental writes
                // when it actually becomes a problem.
                let mut file: std::fs::File = fd.into();
                if let Err(e) = file.write_all(blob) {
                    warn!(error = %e, %mime_type, "write to consumer failed");
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if let Some(data) = state.sources.remove(source) {
                    info!(sel = data.selection.as_str(), "source cancelled");
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore WlSeat);
wayland_client::delegate_noop!(State: ignore ZwlrDataControlManagerV1);

/// Connect to wayland, bind the wlr-data-control manager, and drive the event
/// loop. The loop also polls `cmd_rx` for IPC-originated commands (e.g. copy
/// requests); a short poll timeout keeps wake-up latency under 50 ms.
pub fn run(capture_tx: mpsc::Sender<CapturedEntry>, cmd_rx: CmdReceiver) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland (WAYLAND_DISPLAY)")?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=9, ())
        .context("bind wl_seat")?;
    let manager = globals
        .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
        .context("bind zwlr_data_control_manager_v1 (compositor must support wlr-data-control)")?;
    let device = manager.get_data_device(&seat, &qh, ());

    let mut state = State {
        offers: HashMap::new(),
        capture_tx,
        sources: HashMap::new(),
        manager,
        device,
    };
    info!("watching clipboard");

    let wayland_fd = conn.as_fd();

    loop {
        // 1. Drain IPC commands without blocking.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => state.handle_cmd(cmd, &conn, &qh),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // 2. Dispatch any wayland events that came in since last loop.
        event_queue
            .dispatch_pending(&mut state)
            .context("wayland dispatch_pending")?;
        event_queue.flush().context("wayland flush")?;

        // 3. Prepare for a blocking read on the wayland fd. If new events
        //    arrived in the meantime, restart the loop to dispatch them.
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };

        // 4. Wait for wayland to be readable, or a 50ms timeout (lets us
        //    re-check the command channel and stay responsive to ipc).
        let mut fds = [PollFd::new(wayland_fd, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(50_u8)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {
                drop(read_guard);
                continue;
            }
            Err(e) => return Err(e).context("poll"),
        }
        if fds[0]
            .revents()
            .map_or(false, |r| r.contains(PollFlags::POLLIN))
        {
            read_guard.read().context("read wayland events")?;
        } else {
            drop(read_guard);
        }
    }
}
