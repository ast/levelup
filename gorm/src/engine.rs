//! The BlueZ engine: a background thread owning a tokio runtime + `bluer`
//! session. It polls the adapter on a tick and streams `Fact`s to the TUI, and
//! takes `Cmd`s back (toggling discovery). This is the bidirectional shape
//! heimdall's one-way discovery engine doesn't need.
//!
//! Why polling rather than pure event subscription: BlueZ device counts are
//! small and a manual picker doesn't need sub-second latency, so a periodic
//! re-read is simpler and robust. Discovery, when on, is kept alive by *holding*
//! the stream `bluer::Adapter::discover_devices` returns (its drop stops the
//! scan) — we don't need to drain it; the next poll picks up new devices.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::warn;

use crate::device::{BtDevice, Cmd, Fact, Prompt, PromptKind};

/// How often we re-read the adapter's devices.
const POLL: Duration = Duration::from_millis(1000);

/// The single in-flight agent reply channel (pairing is sequential, so one
/// slot suffices). `None` text rejects; `Some(text)` accepts with the entered
/// passkey/PIN (empty for a yes/no confirmation).
type ReplySlot = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

/// Handles back to the running engine: `facts` to drain, `cmds` to drive it.
pub struct Engine {
    pub facts: UnboundedReceiver<Fact>,
    pub cmds: UnboundedSender<Cmd>,
}

/// Spawn the engine on its own thread + tokio runtime. It runs until the `cmds`
/// sender is dropped (the TUI quitting) or BlueZ errors fatally.
pub fn spawn() -> Engine {
    let (fact_tx, fact_rx) = mpsc::unbounded_channel::<Fact>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    std::thread::Builder::new()
        .name("gorm-bluez".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                let _ = fact_tx.send(Fact::Error("could not start tokio runtime".into()));
                return;
            };
            rt.block_on(async move {
                if let Err(e) = run(&fact_tx, cmd_rx).await {
                    let _ = fact_tx.send(Fact::Error(e.to_string()));
                }
            });
        })
        .expect("spawn gorm-bluez thread");

    Engine {
        facts: fact_rx,
        cmds: cmd_tx,
    }
}

async fn run(fact_tx: &UnboundedSender<Fact>, mut cmd_rx: UnboundedReceiver<Cmd>) -> Result<()> {
    let session = bluer::Session::new()
        .await
        .context("connect to BlueZ over D-Bus")?;
    let adapter = ready_adapter(&session).await?;
    let _ = fact_tx.send(Fact::Adapter {
        name: adapter.name().to_string(),
    });

    // Register our pairing agent so passkey/confirmation requests surface in
    // the TUI. The handle must stay alive for the agent to remain registered.
    let reply_slot: ReplySlot = Arc::new(Mutex::new(None));
    let _agent = session
        .register_agent(build_agent(fact_tx.clone(), reply_slot.clone()))
        .await
        .context("register pairing agent")?;

    // Discovery is always on while the picker is open (no user-facing toggle).
    // Holding the stream keeps it alive; dropping it stops it. We never poll it
    // — the periodic snapshot picks up whatever discovery surfaces.
    let mut discovery: Option<Box<dyn Any>> = start_discovery(&adapter).await;
    // Count of in-flight connect/pair/… actions. Discovery is paused while any
    // is running — concurrent discovery makes connecting/pairing flaky — and
    // resumes (next tick) once the count hits zero. The user never manages it.
    let inflight = Arc::new(AtomicUsize::new(0));
    let mut ticker = tokio::time::interval(POLL);

    let _ = fact_tx.send(Fact::Snapshot(read_all(&adapter).await));

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                // TUI dropped the sender → quit the engine.
                None => break,
                // Route a user's answer to the waiting agent callback.
                Some(Cmd::AnswerPrompt(reply)) => {
                    if let Some(tx) = reply_slot.lock().await.take() {
                        let _ = tx.send(reply);
                    }
                }
                // Connect/pair can take seconds (pairing waits on the user), so
                // run each action off the poll loop on its own task; it reports
                // back via ActionResult and the next tick reflects the new state.
                Some(action) => {
                    if let Some((address, verb, fut)) = action_for(&adapter, action) {
                        // Pause discovery for the duration of the action.
                        discovery = None;
                        inflight.fetch_add(1, Ordering::SeqCst);
                        let tx = fact_tx.clone();
                        let inflight = inflight.clone();
                        tokio::spawn(async move {
                            let error = fut.await.err().map(|e| e.to_string());
                            inflight.fetch_sub(1, Ordering::SeqCst);
                            let _ = tx.send(Fact::ActionResult { address, verb, error });
                        });
                    }
                }
            },
            _ = ticker.tick() => {
                // Resume discovery once nothing is acting on the adapter.
                if discovery.is_none() && inflight.load(Ordering::SeqCst) == 0 {
                    discovery = start_discovery(&adapter).await;
                }
                if fact_tx.send(Fact::Snapshot(read_all(&adapter).await)).is_err() {
                    break; // TUI gone
                }
            }
        }
    }
    Ok(())
}

/// Begin a discovery session; the returned guard keeps it running until
/// dropped. `None` (logged) if BlueZ refuses — the picker still shows known
/// devices, just won't surface new ones until the next attempt.
async fn start_discovery(adapter: &bluer::Adapter) -> Option<Box<dyn Any>> {
    match adapter.discover_devices().await {
        Ok(stream) => Some(Box::new(stream)),
        Err(e) => {
            warn!(error = %e, "start discovery");
            None
        }
    }
}

type ActionFut = std::pin::Pin<Box<dyn Future<Output = bluer::Result<()>> + Send>>;

/// Resolve an action `Cmd` to (address, status verb, the BlueZ call to run).
/// `None` for non-device commands or an unparseable/unknown address.
fn action_for(adapter: &bluer::Adapter, cmd: Cmd) -> Option<(String, &'static str, ActionFut)> {
    let (address, verb): (String, &'static str) = match &cmd {
        Cmd::AnswerPrompt(_) => return None,
        Cmd::Connect(a) => (a.clone(), "connect"),
        Cmd::Disconnect(a) => (a.clone(), "disconnect"),
        Cmd::SetTrusted(a, true) => (a.clone(), "trust"),
        Cmd::SetTrusted(a, false) => (a.clone(), "untrust"),
        Cmd::SetBlocked(a, true) => (a.clone(), "block"),
        Cmd::SetBlocked(a, false) => (a.clone(), "unblock"),
        Cmd::Pair(a) => (a.clone(), "pair"),
        Cmd::Unpair(a) => (a.clone(), "unpair"),
    };
    let parsed: bluer::Address = address.parse().ok()?;
    let fut: ActionFut = match cmd {
        Cmd::Connect(_) => {
            let dev = adapter.device(parsed).ok()?;
            Box::pin(async move { dev.connect().await })
        }
        Cmd::Disconnect(_) => {
            let dev = adapter.device(parsed).ok()?;
            Box::pin(async move { dev.disconnect().await })
        }
        Cmd::SetTrusted(_, v) => {
            let dev = adapter.device(parsed).ok()?;
            Box::pin(async move { dev.set_trusted(v).await })
        }
        Cmd::SetBlocked(_, v) => {
            let dev = adapter.device(parsed).ok()?;
            Box::pin(async move { dev.set_blocked(v).await })
        }
        Cmd::Pair(_) => {
            let dev = adapter.device(parsed).ok()?;
            Box::pin(async move { dev.pair().await })
        }
        // Forget is an adapter operation, not a device one.
        Cmd::Unpair(_) => {
            let ad = adapter.clone();
            Box::pin(async move { ad.remove_device(parsed).await })
        }
        Cmd::AnswerPrompt(_) => return None,
    };
    Some((address, verb, fut))
}

/// Build the pairing agent. Each request that needs the user sends an
/// `AgentPrompt` fact and waits for the answer routed back through `reply_slot`
/// (fulfilled by the `AnswerPrompt` command). `request_default` makes us the
/// session's agent so we handle all pairings.
fn build_agent(fact_tx: UnboundedSender<Fact>, slot: ReplySlot) -> bluer::agent::Agent {
    use bluer::agent::{
        Agent, AuthorizeService, DisplayPasskey, ReqError, RequestConfirmation, RequestPasskey,
        RequestPinCode,
    };

    Agent {
        request_default: true,
        // Numeric comparison ("does 123456 match the device?") — yes/no.
        request_confirmation: Some(Box::new({
            let fact_tx = fact_tx.clone();
            let slot = slot.clone();
            move |req: RequestConfirmation| {
                let fact_tx = fact_tx.clone();
                let slot = slot.clone();
                Box::pin(async move {
                    let msg = format!(
                        "pair {}? passkey {:06} — Enter confirm · Esc reject",
                        req.device, req.passkey
                    );
                    match prompt(&fact_tx, &slot, req.device, PromptKind::Confirm, msg).await {
                        Some(_) => Ok(()),
                        None => Err(ReqError::Rejected),
                    }
                })
            }
        })),
        // The user types a passkey shown on the device (keyboards).
        request_passkey: Some(Box::new({
            let fact_tx = fact_tx.clone();
            let slot = slot.clone();
            move |req: RequestPasskey| {
                let fact_tx = fact_tx.clone();
                let slot = slot.clone();
                Box::pin(async move {
                    let msg = format!("pair {} — type passkey · Enter · Esc reject", req.device);
                    match prompt(&fact_tx, &slot, req.device, PromptKind::Passkey, msg).await {
                        Some(s) => s.trim().parse::<u32>().map_err(|_| ReqError::Rejected),
                        None => Err(ReqError::Rejected),
                    }
                })
            }
        })),
        // Legacy PIN entry.
        request_pin_code: Some(Box::new({
            let fact_tx = fact_tx.clone();
            let slot = slot.clone();
            move |req: RequestPinCode| {
                let fact_tx = fact_tx.clone();
                let slot = slot.clone();
                Box::pin(async move {
                    let msg = format!("pair {} — type PIN · Enter · Esc reject", req.device);
                    match prompt(&fact_tx, &slot, req.device, PromptKind::Pin, msg).await {
                        Some(s) => Ok(s),
                        None => Err(ReqError::Rejected),
                    }
                })
            }
        })),
        // Device shows a code we display; nothing to answer.
        display_passkey: Some(Box::new({
            let fact_tx = fact_tx.clone();
            move |req: DisplayPasskey| {
                let fact_tx = fact_tx.clone();
                Box::pin(async move {
                    let _ = fact_tx.send(Fact::AgentPrompt(Prompt {
                        address: req.device.to_string(),
                        kind: PromptKind::Display,
                        message: format!("enter {:06} on {}", req.passkey, req.device),
                    }));
                    Ok(())
                })
            }
        })),
        // A device the user just chose to use: authorize its services so audio
        // works without a second prompt (block it if you don't want that).
        authorize_service: Some(Box::new(move |_req: AuthorizeService| {
            Box::pin(async move { Ok(()) })
        })),
        ..Default::default()
    }
}

/// Stash a one-shot reply channel in `slot`, send the prompt to the TUI, and
/// await the user's answer. `None` if rejected or the TUI went away.
async fn prompt(
    fact_tx: &UnboundedSender<Fact>,
    slot: &ReplySlot,
    device: bluer::Address,
    kind: PromptKind,
    message: String,
) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    *slot.lock().await = Some(tx);
    let _ = fact_tx.send(Fact::AgentPrompt(Prompt {
        address: device.to_string(),
        kind,
        message,
    }));
    rx.await.ok().flatten()
}

/// Open the default adapter, powering it on if it's off (the launch behaviour
/// the TUI relies on). Restoring a previously-off adapter on exit is M4.
async fn ready_adapter(session: &bluer::Session) -> Result<bluer::Adapter> {
    let adapter = session
        .default_adapter()
        .await
        .context("no default Bluetooth adapter (is bluetoothd running?)")?;
    if !adapter.is_powered().await.context("read adapter power")? {
        adapter
            .set_powered(true)
            .await
            .context("power on adapter")?;
    }
    Ok(adapter)
}

/// Read every device the adapter knows into our model. Per-device property
/// reads that fail are treated as "unknown" rather than aborting the snapshot.
pub async fn read_all(adapter: &bluer::Adapter) -> Vec<BtDevice> {
    let mut devices = Vec::new();
    let addrs = match adapter.device_addresses().await {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "list device addresses");
            return devices;
        }
    };
    for addr in addrs {
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        // BlueZ defaults a nameless device's alias to its dashed address
        // (e.g. `6B-73-4B-81-C5-59`); treat that as "no name" so the ranking
        // can tell identifiable devices from anonymous beacons and the NAME
        // column doesn't just echo the address.
        let addr_dash = addr.to_string().replace(':', "-");
        let name = match dev.alias().await {
            Ok(a) if !a.is_empty() && a != addr_dash => Some(a),
            _ => dev.name().await.ok().flatten(),
        };
        devices.push(BtDevice {
            address: addr.to_string(),
            name,
            paired: dev.is_paired().await.unwrap_or(false),
            connected: dev.is_connected().await.unwrap_or(false),
            trusted: dev.is_trusted().await.unwrap_or(false),
            blocked: dev.is_blocked().await.unwrap_or(false),
            rssi: dev.rssi().await.ok().flatten(),
            icon: dev.icon().await.ok().flatten(),
            battery: dev.battery_percentage().await.ok().flatten(),
        });
    }
    devices
}

/// One-shot snapshot for `gorm scan` — powers the adapter on, then reads once.
pub async fn scan_once() -> Result<Vec<BtDevice>> {
    let session = bluer::Session::new()
        .await
        .context("connect to BlueZ over D-Bus")?;
    let adapter = ready_adapter(&session).await?;
    Ok(read_all(&adapter).await)
}
