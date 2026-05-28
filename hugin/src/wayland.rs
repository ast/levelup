//! Wayland event loop: binds wlr-data-control, reads each new selection,
//! and forwards captures to the storage thread via the supplied channel.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::mpsc;

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::unistd::pipe2;
use tracing::{debug, info, warn};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use crate::{CapturedEntry, Selection};

struct State {
    /// MIMEs accumulated per pending offer object, keyed by the proxy itself.
    offers: HashMap<ZwlrDataControlOfferV1, Vec<String>>,
    tx: mpsc::Sender<CapturedEntry>,
}

impl State {
    fn new(tx: mpsc::Sender<CapturedEntry>) -> Self {
        Self {
            offers: HashMap::new(),
            tx,
        }
    }

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
        let mimes = self.offers.remove(&offer).unwrap_or_default();
        if mimes.is_empty() {
            warn!(sel = sel.as_str(), "offer committed with no MIMEs");
            offer.destroy();
            return;
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
        if let Err(e) = self.tx.send(entry) {
            warn!(error = %e, "storage channel closed; dropping capture");
        }
    }
}

/// Send `receive(mime, write_fd)`, flush, then read the read end to EOF.
fn read_offer(
    offer: &ZwlrDataControlOfferV1,
    mime: &str,
    conn: &Connection,
) -> Result<Vec<u8>> {
    let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC).context("pipe2")?;
    offer.receive(mime.to_string(), write_fd.as_fd());
    // Drop our copy of the write end so the read side sees EOF once the
    // compositor finishes writing. The wayland connection has already dup'd
    // the fd into the queued message.
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

wayland_client::delegate_noop!(State: ignore WlSeat);
wayland_client::delegate_noop!(State: ignore ZwlrDataControlManagerV1);

/// Connect to wayland, bind the wlr-data-control manager, and run the dispatch
/// loop until an unrecoverable error. The supplied `tx` is held until the loop
/// exits, so dropping it cleanly closes the storage channel.
pub fn run(tx: mpsc::Sender<CapturedEntry>) -> Result<()> {
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

    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State::new(tx);
    info!("watching clipboard");

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("wayland dispatch")?;
    }
}
