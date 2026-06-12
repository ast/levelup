//! The device model + the engine↔TUI channel types.

/// A Bluetooth device as gorm sees it — a snapshot of BlueZ's properties for
/// one device.
#[derive(Debug, Clone)]
pub struct BtDevice {
    /// The device's Bluetooth address (MAC), the stable identity.
    pub address: String,
    /// Friendly name — the user-set alias if any, else the advertised name.
    pub name: Option<String>,
    /// Bonded (pairing keys exchanged).
    pub paired: bool,
    /// A link is currently up.
    pub connected: bool,
    /// Trusted — BlueZ auto-accepts its connections without asking.
    pub trusted: bool,
    /// Blocked — BlueZ refuses connections.
    pub blocked: bool,
    /// Signal strength in dBm when the device is in range (None if unknown,
    /// e.g. a known device not currently advertising).
    pub rssi: Option<i16>,
    /// BlueZ icon hint for the device class, e.g. `audio-headset`, `input-mouse`.
    pub icon: Option<String>,
    /// Manufacturer from the MAC's OUI (None for random/private addresses).
    pub vendor: Option<String>,
    /// Battery level 0–100 when the device exposes it.
    pub battery: Option<u8>,
    /// Recognized capabilities derived from the device's advertised service
    /// UUIDs — what it *can* do (audio out, mic, …), not what's live right now.
    /// Sorted, deduped; empty when the device advertises nothing we recognise.
    pub profiles: Vec<Profile>,
}

/// A user-facing capability inferred from a Bluetooth device's advertised
/// service-class UUIDs (the SDP profiles it supports). This is *capability*,
/// not live routing — it says a headset can play audio and carry a mic, not
/// that audio is flowing right now (that lives in BlueZ's MediaTransport1,
/// owned by PipeWire on this desktop). Declaration order is the display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Profile {
    /// A2DP Audio Sink — the device receives our audio (headphones, speaker).
    AudioOut,
    /// A2DP Audio Source — the device streams audio to us (e.g. a phone).
    AudioIn,
    /// HFP / HSP — headset voice with a microphone.
    Mic,
    /// AVRCP — play / pause / skip transport controls.
    MediaKeys,
    /// HID — keyboard, mouse, gamepad.
    Input,
}

impl Profile {
    /// A short, human label for the detail strip / `scan` dump.
    pub fn label(self) -> &'static str {
        match self {
            Profile::AudioOut => "Audio out",
            Profile::AudioIn => "Audio in",
            Profile::Mic => "Mic",
            Profile::MediaKeys => "Media keys",
            Profile::Input => "Input",
        }
    }

    /// Map a device's advertised 16-bit service-class UUIDs to the capabilities
    /// we surface. Unknown UUIDs are ignored; the result is sorted and deduped.
    pub fn from_uuids(short_uuids: &[u16]) -> Vec<Profile> {
        let mut out = Vec::new();
        let mut add = |p: Profile| {
            if !out.contains(&p) {
                out.push(p);
            }
        };
        for &id in short_uuids {
            match id {
                0x110B => add(Profile::AudioOut),     // AudioSink
                0x110A => add(Profile::AudioIn),      // AudioSource
                0x111E | 0x1108 => add(Profile::Mic), // Handsfree (HFP), Headset (HSP)
                // AVRCP: Remote Control / Target / Controller.
                0x110E | 0x110C | 0x110F => add(Profile::MediaKeys),
                0x1124 => add(Profile::Input), // Human Interface Device
                _ => {}
            }
        }
        out.sort_unstable();
        out
    }
}

impl BtDevice {
    /// A short human label for the device's state, for the `scan` dump.
    pub fn state_label(&self) -> &'static str {
        if self.blocked {
            "blocked"
        } else if self.connected {
            "connected"
        } else if self.paired {
            "paired"
        } else {
            "-"
        }
    }

    /// Capabilities joined for display, e.g. `Audio out · Mic`. Empty string
    /// when nothing recognisable is advertised.
    pub fn profile_summary(&self) -> String {
        self.profiles
            .iter()
            .map(|p| p.label())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The string nucleo matches against in the picker — name, address, device
    /// type, and vendor fused into one content space (type any of them to
    /// filter, e.g. `sony`).
    pub fn haystack(&self) -> String {
        let mut s = self.address.clone();
        for part in [
            self.name.as_deref(),
            self.icon.as_deref(),
            self.vendor.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            s.push(' ');
            s.push_str(part);
        }
        for p in &self.profiles {
            s.push(' ');
            s.push_str(p.label());
        }
        s
    }
}

/// What kind of answer a pairing-agent prompt needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Numeric-comparison or authorization: yes / no.
    Confirm,
    /// User types a numeric passkey (keyboards).
    Passkey,
    /// User types a PIN string (legacy devices).
    Pin,
    /// Informational only — the code shows on the device; no answer needed.
    Display,
}

/// A pairing-agent request surfaced to the user, rendered in the status line.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub address: String,
    pub kind: PromptKind,
    /// The fully-rendered line shown to the user (includes any passkey).
    pub message: String,
}

/// Engine → TUI. The engine polls BlueZ and streams these; the TUI drains them
/// each tick.
#[derive(Debug, Clone)]
pub enum Fact {
    /// The full set of devices BlueZ currently knows (replaces the live set).
    Snapshot(Vec<BtDevice>),
    /// The adapter gorm bound to.
    Adapter { name: String },
    /// An action finished — `error` is `None` on success. `verb` is a short
    /// label ("connect", "trust", …) for the status line.
    ActionResult {
        address: String,
        verb: &'static str,
        error: Option<String>,
    },
    /// The pairing agent needs the user (passkey confirmation / entry).
    AgentPrompt(Prompt),
    /// Something went wrong (shown in the status line).
    Error(String),
}

/// TUI → engine.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Bring up a link to the device.
    Connect(String),
    /// Tear down the link.
    Disconnect(String),
    /// Set the device trusted (`true`) or untrusted (`false`).
    SetTrusted(String, bool),
    /// Set the device blocked (`true`) or unblocked (`false`).
    SetBlocked(String, bool),
    /// Pair (bond) with the device — drives the agent prompt flow.
    Pair(String),
    /// Remove the bond (forget). Gated behind the hold-to-confirm gesture.
    Unpair(String),
    /// Answer the pending agent prompt: `None` rejects, `Some(text)` accepts
    /// (text is the entered passkey/PIN, or empty for a yes/no confirmation).
    AnswerPrompt(Option<String>),
}
