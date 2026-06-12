//! The PipeWire engine: a background thread owning the `pw_main_loop` (the C
//! library pins loop objects to one thread, the same role tokio plays in
//! gorm's engine). Registry/metadata/param listeners fold every change into an
//! [`AudioState`] and publish a fresh snapshot over `mpsc` after each one; the
//! consumer just keeps the latest. Commands come back the other way over
//! `pw::channel` (an fd source on the loop), the same bidirectional shape as
//! gorm's engine.
//!
//! Unlike gorm's polling, this is purely event-driven: PipeWire pushes node
//! adds/removes, `Props` params (volume/mute), link changes, and metadata
//! (default device) edits as they happen. Commands are fire-and-forget — the
//! confirmation is the resulting graph change echoing back as a snapshot.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use anyhow::{Context as _, Result};
use libspa::param::ParamType;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Pod, Property, Value, ValueArray};
use libspa::utils::dict::DictRef;
use libspa::utils::result::AsyncSeq;
use pipewire as pw;
use pw::core::CoreRc;
use pw::device::Device;
use pw::metadata::Metadata;
use pw::node::Node;
use pw::proxy::Listener;
use pw::registry::{GlobalObject, RegistryRc};
use pw::types::ObjectType;
use tracing::{debug, info, warn};

use crate::state::{AudioNode, AudioState, NodeKind};

/// Commands the TUI sends into the PipeWire loop.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Re-route a stream to a device (`target.object` metadata on the stream;
    /// WirePlumber performs the move and remembers it for the app).
    Move {
        stream: u32,
        target: u32,
    },
    /// Make a device the configured default of its kind.
    SetDefault {
        device: u32,
    },
    /// Nudge a node's volume by a UI-scale delta. The engine computes from its
    /// own state (fresher than the TUI's snapshot, so held-key repeats don't
    /// compound a stale base) and clamps to 0..=1.
    AdjustVolume {
        node: u32,
        delta: f32,
    },
    ToggleMute {
        node: u32,
    },
}

/// Handles back to the running engine: `updates` to drain, `cmds` to drive it.
/// `updates` closes when the loop quits or the connection errors fatally.
pub struct Engine {
    pub updates: Receiver<AudioState>,
    pub cmds: pw::channel::Sender<Cmd>,
}

/// Spawn the engine on its own thread. Errors inside the loop are logged and
/// surface to the consumer as the channel closing.
pub fn spawn() -> Engine {
    let (tx, rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = pw::channel::channel();
    std::thread::Builder::new()
        .name("bragi-pw".into())
        .spawn(move || {
            if let Err(e) = run(tx, cmd_rx) {
                tracing::error!("pipewire engine: {e:#}");
            }
        })
        .expect("spawn bragi-pw thread");
    Engine {
        updates: rx,
        cmds: cmd_tx,
    }
}

/// The mutable heart shared by every listener closure (single-threaded — the
/// loop dispatches callbacks serially, so `RefCell` suffices).
struct Graph {
    state: AudioState,
    tx: Sender<AudioState>,
    /// Seq of the latest `core.sync` round-trip. Each node bind re-arms it, so
    /// the `done` that finally matches means initial enumeration *and* every
    /// bound node's first `Props` param have been delivered.
    pending: Option<AsyncSeq>,
}

impl Graph {
    fn publish(&self) {
        let _ = self.tx.send(self.state.clone());
    }
}

type SharedGraph = Rc<RefCell<Graph>>;
/// Bound node proxies by global id, kept typed so commands can `set_param`.
type NodeProxies = Rc<RefCell<HashMap<u32, Node>>>;
/// Listeners by global id; dropping one silently stops that object's events.
type Listeners = Rc<RefCell<HashMap<u32, Box<dyn Listener>>>>;
/// The bound `default` metadata object — where stream targets and default
/// device choices are written.
type MetaSlot = Rc<RefCell<Option<(u32, Metadata)>>>;

/// A bound `Device` (ALSA card / bluez endpoint) and its active routes:
/// `card.profile.device` index → route index, fed by `Route` param events.
/// Device-node volume/mute writes go through these (see `route_device` on
/// [`AudioNode`]); they're also where M3's profile switching will live.
struct DeviceEntry {
    proxy: Device,
    routes: HashMap<i32, i32>,
}
type Devices = Rc<RefCell<HashMap<u32, DeviceEntry>>>;

fn run(tx: Sender<AudioState>, cmd_rx: pw::channel::Receiver<Cmd>) -> Result<()> {
    pw::init();
    // No signal handlers here: `add_signal_local` insists on the main thread,
    // and bragi holds nothing that needs cleanup — default SIGINT/SIGTERM
    // termination *is* the clean exit for `watch`.
    let main_loop = pw::main_loop::MainLoopRc::new(None).context("create pipewire main loop")?;

    let context =
        pw::context::ContextRc::new(&main_loop, None).context("create pipewire context")?;
    let core = context
        .connect_rc(None)
        .context("connect to PipeWire — is it running?")?;
    let registry = core.get_registry_rc().context("get registry")?;

    let graph: SharedGraph = Rc::new(RefCell::new(Graph {
        state: AudioState::default(),
        tx,
        pending: None,
    }));
    let nodes_px: NodeProxies = Rc::new(RefCell::new(HashMap::new()));
    let listeners: Listeners = Rc::new(RefCell::new(HashMap::new()));
    let default_meta: MetaSlot = Rc::new(RefCell::new(None));
    let devices: Devices = Rc::new(RefCell::new(HashMap::new()));

    // Commands from the TUI wake the loop via an fd source.
    let _cmd_source = cmd_rx.attach(main_loop.loop_(), {
        let graph = graph.clone();
        let nodes_px = nodes_px.clone();
        let default_meta = default_meta.clone();
        let devices = devices.clone();
        move |cmd| handle_cmd(cmd, &graph, &nodes_px, &devices, &default_meta)
    });

    // Arm the first round-trip; node binds re-arm it (see `Graph::pending`).
    graph.borrow_mut().pending = Some(core.sync(0).context("initial sync")?);

    let _core_listener = core
        .add_listener_local()
        .done({
            let graph = graph.clone();
            move |id, seq| {
                let mut g = graph.borrow_mut();
                if id == pw::core::PW_ID_CORE && g.pending == Some(seq) && !g.state.complete {
                    g.state.complete = true;
                    debug!("initial enumeration settled");
                    g.publish();
                }
            }
        })
        .error(|id, _seq, res, message| {
            warn!("pipewire error id={id} res={res}: {message}");
        })
        .register();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let graph = graph.clone();
            let nodes_px = nodes_px.clone();
            let listeners = listeners.clone();
            let default_meta = default_meta.clone();
            let devices = devices.clone();
            let registry = registry.clone();
            let core = core.clone();
            move |obj| match obj.type_ {
                ObjectType::Node => on_node(&graph, &nodes_px, &listeners, &registry, &core, obj),
                ObjectType::Link => on_link(&graph, obj),
                ObjectType::Device => on_device(&devices, &listeners, &registry, obj),
                ObjectType::Metadata => {
                    on_metadata(&graph, &default_meta, &listeners, &registry, obj)
                }
                _ => {}
            }
        })
        .global_remove({
            let graph = graph.clone();
            let nodes_px = nodes_px.clone();
            let listeners = listeners.clone();
            let default_meta = default_meta.clone();
            let devices = devices.clone();
            move |id| {
                nodes_px.borrow_mut().remove(&id);
                listeners.borrow_mut().remove(&id);
                devices.borrow_mut().remove(&id);
                {
                    let mut meta = default_meta.borrow_mut();
                    if meta.as_ref().is_some_and(|(mid, _)| *mid == id) {
                        *meta = None;
                    }
                }
                let mut g = graph.borrow_mut();
                if let Some(n) = g.state.nodes.remove(&id) {
                    info!("- {} id={} {}", n.kind.label(), id, n.name);
                    g.publish();
                } else if g.state.links.remove(&id).is_some() {
                    debug!("- link id={id}");
                    g.publish();
                }
            }
        })
        .register();

    main_loop.run();
    debug!("pipewire loop exited");
    Ok(())
}

/// Execute a TUI command. All writes are fire-and-forget: PipeWire echoes the
/// resulting change back through the normal event path, which publishes the
/// snapshot that confirms it on screen.
fn handle_cmd(
    cmd: Cmd,
    graph: &SharedGraph,
    nodes_px: &NodeProxies,
    devices: &Devices,
    default_meta: &MetaSlot,
) {
    match cmd {
        Cmd::AdjustVolume { node, delta } => {
            let Some((cur, channels)) = graph
                .borrow()
                .state
                .nodes
                .get(&node)
                .and_then(|n| n.volume.map(|v| (v, n.channels)))
            else {
                return;
            };
            let ui = (cur + delta).clamp(0.0, 1.0);
            // UI scale → linear sample factor (inverse of the cbrt at parse).
            let linear = ui.powi(3);
            debug!("cmd: vol id={node} → {}%", (ui * 100.0).round() as u32);
            write_props(
                node,
                vec![Property::new(
                    libspa::sys::SPA_PROP_channelVolumes,
                    Value::ValueArray(ValueArray::Float(vec![linear; channels.max(1)])),
                )],
                graph,
                nodes_px,
                devices,
            );
        }
        Cmd::ToggleMute { node } => {
            let Some(mute) = graph.borrow().state.nodes.get(&node).map(|n| !n.mute) else {
                return;
            };
            debug!("cmd: mute id={node} → {mute}");
            write_props(
                node,
                vec![Property::new(libspa::sys::SPA_PROP_mute, Value::Bool(mute))],
                graph,
                nodes_px,
                devices,
            );
        }
        Cmd::Move { stream, target } => {
            let meta = default_meta.borrow();
            let Some((_, md)) = meta.as_ref() else { return };
            let g = graph.borrow();
            let (Some(s), Some(t)) = (g.state.nodes.get(&stream), g.state.nodes.get(&target))
            else {
                return;
            };
            // `target.object` takes the destination's serial (or node name as
            // a fallback); subject is the stream's global id.
            let value = t
                .serial
                .map(|s| s.to_string())
                .unwrap_or_else(|| t.node_name.clone());
            info!("cmd: move {} → {}", s.name, t.name);
            md.set_property(stream, "target.object", Some("Spa:Id"), Some(&value));
        }
        Cmd::SetDefault { device } => {
            let meta = default_meta.borrow();
            let Some((_, md)) = meta.as_ref() else { return };
            let g = graph.borrow();
            let Some(n) = g.state.nodes.get(&device) else {
                return;
            };
            // `default.configured.*` is the user's wish; WirePlumber reads it,
            // updates the effective `default.audio.*`, and that echoes back
            // into our metadata listener.
            let key = match n.kind {
                NodeKind::Sink => "default.configured.audio.sink",
                NodeKind::Source => "default.configured.audio.source",
                _ => return,
            };
            let value = serde_json::json!({ "name": n.node_name }).to_string();
            info!("cmd: default {} → {}", n.kind.label(), n.name);
            md.set_property(0, key, Some("Spa:String:JSON"), Some(&value));
        }
    }
}

/// Route volume/mute props to the right place for a node. Device nodes
/// (sinks/sources backed by an ALSA card or bluez endpoint) get a `Route`
/// param on their *Device* — the mixer wpctl and the hardware follow; writing
/// node `Props` there would only stack a second software volume on the side.
/// Streams (and route-less virtuals like null sinks) get plain node `Props`.
/// Either way the change echoes back into the node's `Props` event, which is
/// what updates the UI.
fn write_props(
    node: u32,
    properties: Vec<Property>,
    graph: &SharedGraph,
    nodes_px: &NodeProxies,
    devices: &Devices,
) {
    let route = {
        let g = graph.borrow();
        g.state
            .nodes
            .get(&node)
            .and_then(|n| Some((n.device_id?, n.route_device?)))
    };
    if let Some((device_id, route_device)) = route {
        let devs = devices.borrow();
        if let Some(entry) = devs.get(&device_id)
            && let Some(&route_index) = entry.routes.get(&route_device)
        {
            let value = Value::Object(Object {
                type_: libspa::sys::SPA_TYPE_OBJECT_ParamRoute,
                id: libspa::sys::SPA_PARAM_Route,
                properties: vec![
                    Property::new(libspa::sys::SPA_PARAM_ROUTE_index, Value::Int(route_index)),
                    Property::new(
                        libspa::sys::SPA_PARAM_ROUTE_device,
                        Value::Int(route_device),
                    ),
                    Property::new(
                        libspa::sys::SPA_PARAM_ROUTE_props,
                        Value::Object(Object {
                            type_: libspa::sys::SPA_TYPE_OBJECT_Props,
                            id: libspa::sys::SPA_PARAM_Route,
                            properties,
                        }),
                    ),
                    // Persist in WirePlumber's state, like wpctl does.
                    Property::new(libspa::sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
                ],
            });
            set_param(
                |pod| entry.proxy.set_param(ParamType::Route, 0, pod),
                &value,
            );
            return;
        }
    }
    let nodes = nodes_px.borrow();
    let Some(px) = nodes.get(&node) else { return };
    let value = Value::Object(Object {
        type_: libspa::sys::SPA_TYPE_OBJECT_Props,
        id: libspa::sys::SPA_PARAM_Props,
        properties,
    });
    set_param(|pod| px.set_param(ParamType::Props, 0, pod), &value);
}

/// Serialize a pod value and hand it to a `set_param` call.
fn set_param(set: impl FnOnce(&Pod), value: &Value) {
    let Ok((cursor, _)) = PodSerializer::serialize(Cursor::new(Vec::new()), value) else {
        warn!("serialize param pod failed");
        return;
    };
    let bytes = cursor.into_inner();
    let Some(pod) = Pod::from_bytes(&bytes) else {
        warn!("param bytes are not a valid pod");
        return;
    };
    set(pod);
}

/// A new node global: filter by `media.class`, record it, bind it and
/// subscribe to `Props` so volume/mute changes stream in.
fn on_node(
    graph: &SharedGraph,
    nodes_px: &NodeProxies,
    listeners: &Listeners,
    registry: &RegistryRc,
    core: &CoreRc,
    obj: &GlobalObject<&DictRef>,
) {
    let Some(props) = obj.props else { return };
    let Some(kind) = props
        .get("media.class")
        .and_then(NodeKind::from_media_class)
    else {
        return;
    };
    let node: Node = match registry.bind(obj) {
        Ok(n) => n,
        Err(e) => {
            warn!("bind node id={}: {e}", obj.id);
            return;
        }
    };

    let node_name = props.get("node.name").unwrap_or_default().to_string();
    let name = display_name(kind, props);
    let detail = props.get("media.name").map(str::to_string);
    let serial = props
        .get("object.serial")
        .and_then(|s| s.parse::<u64>().ok());
    let device_id = props.get("device.id").and_then(|s| s.parse::<u32>().ok());
    let route_device = props
        .get("card.profile.device")
        .and_then(|s| s.parse::<i32>().ok());
    info!("+ {} id={} {}", kind.label(), obj.id, name);
    {
        let mut g = graph.borrow_mut();
        g.state.nodes.insert(
            obj.id,
            AudioNode {
                id: obj.id,
                kind,
                node_name,
                name,
                detail,
                volume: None,
                mute: false,
                serial,
                channels: 2,
                device_id,
                route_device,
            },
        );
        g.publish();
    }

    node.subscribe_params(&[ParamType::Props]);
    let listener = node
        .add_listener_local()
        // The registry global only carries an abbreviated prop dict; the full
        // set (notably `card.profile.device`, needed for route-targeted volume
        // writes) arrives with the info event. `media.name` also changes over
        // a stream's life (track titles), so keep it fresh here too.
        .info({
            let graph = graph.clone();
            let id = obj.id;
            move |info| {
                let Some(props) = info.props() else { return };
                let mut g = graph.borrow_mut();
                let Some(n) = g.state.nodes.get_mut(&id) else {
                    return;
                };
                let mut changed = false;
                let device_id = props.get("device.id").and_then(|s| s.parse::<u32>().ok());
                if device_id.is_some() && n.device_id != device_id {
                    n.device_id = device_id;
                    changed = true;
                }
                let route_device = props
                    .get("card.profile.device")
                    .and_then(|s| s.parse::<i32>().ok());
                if route_device.is_some() && n.route_device != route_device {
                    n.route_device = route_device;
                    changed = true;
                }
                let detail = props.get("media.name").map(str::to_string);
                if detail.is_some() && n.detail != detail {
                    n.detail = detail;
                    changed = true;
                }
                if changed {
                    g.publish();
                }
            }
        })
        .param({
            let graph = graph.clone();
            let id = obj.id;
            move |_seq, ty, _index, _next, param| {
                if ty != ParamType::Props {
                    return;
                }
                let Some(pod) = param else { return };
                let (volume, mute) = parse_props(pod);
                if volume.is_none() && mute.is_none() {
                    return;
                }
                let mut g = graph.borrow_mut();
                let Some(n) = g.state.nodes.get_mut(&id) else {
                    return;
                };
                if let Some((v, channels)) = volume {
                    n.volume = Some(v);
                    n.channels = channels;
                }
                if let Some(m) = mute {
                    n.mute = m;
                }
                info!(
                    "vol {} id={} {}%{}",
                    n.kind.label(),
                    id,
                    n.volume_pct().unwrap_or(0),
                    if n.mute { " [muted]" } else { "" }
                );
                g.publish();
            }
        })
        .register();
    nodes_px.borrow_mut().insert(obj.id, node);
    listeners.borrow_mut().insert(obj.id, Box::new(listener));

    // Re-arm the round-trip so `complete` lands only after this node's
    // subscribed params have been delivered too.
    if !graph.borrow().state.complete
        && let Ok(seq) = core.sync(0)
    {
        graph.borrow_mut().pending = Some(seq);
    }
}

/// A new audio `Device` (ALSA card / bluez endpoint): bind it and follow its
/// active `Route` params so volume/mute commands know which route to write.
fn on_device(
    devices: &Devices,
    listeners: &Listeners,
    registry: &RegistryRc,
    obj: &GlobalObject<&DictRef>,
) {
    let Some(props) = obj.props else { return };
    if props.get("media.class") != Some("Audio/Device") {
        return;
    }
    let device: Device = match registry.bind(obj) {
        Ok(d) => d,
        Err(e) => {
            warn!("bind device id={}: {e}", obj.id);
            return;
        }
    };
    debug!(
        "+ device id={} {}",
        obj.id,
        props.get("device.description").unwrap_or("(unnamed)")
    );

    device.subscribe_params(&[ParamType::Route]);
    let listener = device
        .add_listener_local()
        .param({
            let devices = devices.clone();
            let id = obj.id;
            move |_seq, ty, _index, _next, param| {
                if ty != ParamType::Route {
                    return;
                }
                let Some(pod) = param else { return };
                let Some((route_index, route_device)) = parse_route(pod) else {
                    return;
                };
                debug!("route device={id} card-dev={route_device} index={route_index}");
                if let Some(entry) = devices.borrow_mut().get_mut(&id) {
                    entry.routes.insert(route_device, route_index);
                }
            }
        })
        .register();
    devices.borrow_mut().insert(
        obj.id,
        DeviceEntry {
            proxy: device,
            routes: HashMap::new(),
        },
    );
    listeners.borrow_mut().insert(obj.id, Box::new(listener));
}

/// A new link global: the node pair is in the global props, no bind needed.
/// Several links per stream/device pair (one per channel) — we log the route
/// only when the pair is new.
fn on_link(graph: &SharedGraph, obj: &GlobalObject<&DictRef>) {
    let Some(props) = obj.props else { return };
    let parse = |key| props.get(key).and_then(|s: &str| s.parse::<u32>().ok());
    let (Some(out), Some(inp)) = (parse("link.output.node"), parse("link.input.node")) else {
        return;
    };
    let mut g = graph.borrow_mut();
    let known_pair = g.state.links.values().any(|&p| p == (out, inp));
    g.state.links.insert(obj.id, (out, inp));
    if !known_pair
        && let (Some(from), Some(to)) = (g.state.nodes.get(&out), g.state.nodes.get(&inp))
    {
        info!("route {} → {}", from.name, to.name);
    }
    g.publish();
}

/// The `default` metadata object carries the default sink/source choices as
/// JSON `{"name": "<node.name>"}` values; WirePlumber updates them live. The
/// bound proxy is also where commands write stream targets and defaults.
fn on_metadata(
    graph: &SharedGraph,
    default_meta: &MetaSlot,
    listeners: &Listeners,
    registry: &RegistryRc,
    obj: &GlobalObject<&DictRef>,
) {
    let Some(props) = obj.props else { return };
    if props.get("metadata.name") != Some("default") {
        return;
    }
    let md: Metadata = match registry.bind(obj) {
        Ok(m) => m,
        Err(e) => {
            warn!("bind metadata id={}: {e}", obj.id);
            return;
        }
    };
    let listener = md
        .add_listener_local()
        .property({
            let graph = graph.clone();
            move |_subject, key, _type, value| {
                let name = value.and_then(default_node_name);
                let mut g = graph.borrow_mut();
                match key {
                    Some("default.audio.sink") => {
                        info!("default sink → {}", name.as_deref().unwrap_or("(none)"));
                        g.state.default_sink = name;
                    }
                    Some("default.audio.source") => {
                        info!("default source → {}", name.as_deref().unwrap_or("(none)"));
                        g.state.default_source = name;
                    }
                    _ => return 0,
                }
                g.publish();
                0
            }
        })
        .register();
    listeners.borrow_mut().insert(obj.id, Box::new(listener));
    *default_meta.borrow_mut() = Some((obj.id, md));
}

/// Human label: devices describe themselves; streams are named by their app.
fn display_name(kind: NodeKind, props: &DictRef) -> String {
    let preferred = if kind.is_device() {
        props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
    } else {
        props.get("app.name")
    };
    preferred
        .or_else(|| props.get("node.name"))
        .unwrap_or("(unnamed)")
        .to_string()
}

/// `{"name": "alsa_output...."}` → the node name.
fn default_node_name(value: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    Some(v.get("name")?.as_str()?.to_string())
}

/// Pull `(index, device)` out of an active `Route` param — the coordinates a
/// route-targeted volume write needs.
fn parse_route(pod: &Pod) -> Option<(i32, i32)> {
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(pod.as_bytes())
    else {
        return None;
    };
    let mut index = None;
    let mut device = None;
    for prop in object.properties {
        match prop.key {
            libspa::sys::SPA_PARAM_ROUTE_index => {
                if let Value::Int(i) = prop.value {
                    index = Some(i);
                }
            }
            libspa::sys::SPA_PARAM_ROUTE_device => {
                if let Value::Int(d) = prop.value {
                    device = Some(d);
                }
            }
            _ => {}
        }
    }
    Some((index?, device?))
}

/// Pull volume + mute out of a `Props` param pod. Volume is the max channel of
/// `channelVolumes`, cube-rooted — linear sample factor → the UI scale
/// PulseAudio/wpctl use; the channel count rides along for volume writes.
fn parse_props(pod: &Pod) -> (Option<(f32, usize)>, Option<bool>) {
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(pod.as_bytes())
    else {
        return (None, None);
    };
    let mut volume = None;
    let mut mute = None;
    for prop in object.properties {
        match prop.key {
            libspa::sys::SPA_PROP_channelVolumes => {
                if let Value::ValueArray(ValueArray::Float(channels)) = prop.value {
                    let n = channels.len();
                    volume = channels.into_iter().reduce(f32::max).map(|v| (v.cbrt(), n));
                }
            }
            libspa::sys::SPA_PROP_mute => {
                if let Value::Bool(b) = prop.value {
                    mute = Some(b);
                }
            }
            _ => {}
        }
    }
    (volume, mute)
}
