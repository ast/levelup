//! The audio-graph snapshot the engine maintains and publishes. Pure data, no
//! PipeWire types — this is the boundary the TUI (M1) will consume over the
//! channel, so it must stay `Send + Clone`.

use std::collections::BTreeMap;

/// What a node *is*, derived from its `media.class`. Only the four classes the
/// picker cares about; everything else (video, MIDI, filters) is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// `Audio/Sink` — an output device.
    Sink,
    /// `Audio/Source` — an input device.
    Source,
    /// `Stream/Output/Audio` — an app playing sound.
    Playback,
    /// `Stream/Input/Audio` — an app recording sound.
    Record,
}

impl NodeKind {
    pub fn from_media_class(class: &str) -> Option<Self> {
        match class {
            "Audio/Sink" => Some(Self::Sink),
            "Audio/Source" => Some(Self::Source),
            "Stream/Output/Audio" => Some(Self::Playback),
            "Stream/Input/Audio" => Some(Self::Record),
            _ => None,
        }
    }

    /// Short type tag shown in list rows and log lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sink => "sink",
            Self::Source => "source",
            Self::Playback => "stream",
            Self::Record => "rec",
        }
    }

    pub fn is_device(self) -> bool {
        matches!(self, Self::Sink | Self::Source)
    }
}

/// One audio node (device or stream), keyed by its global id.
#[derive(Debug, Clone)]
pub struct AudioNode {
    pub id: u32,
    pub kind: NodeKind,
    /// `node.name` — the stable identifier metadata defaults refer to.
    pub node_name: String,
    /// Human label: `node.description`/`node.nick` for devices, `app.name` for
    /// streams, falling back to `node.name`.
    pub name: String,
    /// `media.name` — what a stream is playing ("YouTube video", track title).
    pub detail: Option<String>,
    /// UI volume 0.0..=1.0+ (cube root of the linear `channelVolumes` max);
    /// `None` until the first `Props` param arrives.
    pub volume: Option<f32>,
    pub mute: bool,
    /// `object.serial` — what `target.object` metadata wants for stream moves.
    pub serial: Option<u64>,
    /// Channel count, from the last seen `channelVolumes`; volume writes set
    /// every channel to the same value.
    pub channels: usize,
    /// `device.id` — the global id of the backing `Device` (ALSA card / bluez
    /// endpoint), present on device nodes only.
    pub device_id: Option<u32>,
    /// `card.profile.device` — which of the device's routes carries this node.
    /// Volume/mute on a device node must be written to that *route* (the mixer
    /// WirePlumber and the hardware follow); node `Props` would only add a
    /// second software volume on the side.
    pub route_device: Option<i32>,
}

impl AudioNode {
    /// Volume as the percentage humans (and `wpctl`) talk in.
    pub fn volume_pct(&self) -> Option<u32> {
        self.volume.map(|v| (v * 100.0).round() as u32)
    }
}

/// Snapshot of everything bragi knows about the graph.
#[derive(Debug, Clone, Default)]
pub struct AudioState {
    /// Nodes by global id; BTreeMap so iteration is stable.
    pub nodes: BTreeMap<u32, AudioNode>,
    /// Links by global id → (output node id, input node id). Several per
    /// stream/sink pair (one per channel); we only care about the node pair.
    pub links: BTreeMap<u32, (u32, u32)>,
    /// `node.name` of the default sink / source (from the `default` metadata).
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
    /// True once the initial enumeration round-trips have settled.
    pub complete: bool,
}

impl AudioState {
    pub fn is_default(&self, node: &AudioNode) -> bool {
        let default = match node.kind {
            NodeKind::Sink => self.default_sink.as_deref(),
            NodeKind::Source => self.default_source.as_deref(),
            _ => return false,
        };
        default == Some(node.node_name.as_str())
    }

    /// The device a stream is linked to (playback → sink, record → source).
    pub fn target_of(&self, stream: &AudioNode) -> Option<&AudioNode> {
        self.links.values().find_map(|&(out, inp)| {
            let peer = match stream.kind {
                NodeKind::Playback if out == stream.id => inp,
                NodeKind::Record if inp == stream.id => out,
                _ => return None,
            };
            self.nodes.get(&peer).filter(|n| n.kind.is_device())
        })
    }
}
