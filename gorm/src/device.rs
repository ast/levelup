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
    /// Battery level 0–100 when the device exposes it.
    pub battery: Option<u8>,
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

    /// The string nucleo matches against in the picker — name, address, and
    /// device type fused into one content space.
    pub fn haystack(&self) -> String {
        let mut s = self.address.clone();
        if let Some(name) = &self.name {
            s.push(' ');
            s.push_str(name);
        }
        if let Some(icon) = &self.icon {
            s.push(' ');
            s.push_str(icon);
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
