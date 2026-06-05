//! The live Bluetooth picker. Same shape as heimdall's device picker (event
//! loop, readline query keymap, fzf-style list, horizontal detail strip), over
//! gorm's BlueZ engine. The engine streams full snapshots; the TUI merges them
//! with cache-seeded devices and ranks presence-first — connected and in-range
//! devices float to the top, known-but-silent ones render dimmed.
//!
//! Actions: Enter connect/disconnect, Ctrl-T trust, Alt-B block, Alt-P pair,
//! hold Alt-U forget. Pairing passkeys/confirmations surface inline in the
//! status line (not a modal) — answered with Enter/Esc, the Raskin way.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::File;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use levelup_core::{fuzzy, sanitize_display};
use levelup_tui::{editing, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TableRow, Table, TableState};

use crate::config::{self, Config};
use crate::device::{BtDevice, Cmd, Fact, Prompt, PromptKind};
use crate::engine::{self, Engine};

const TICK: Duration = Duration::from_millis(50);
const STATUS_LINGER: Duration = Duration::from_secs(4);
/// How long Alt-U must be held before unpair (forget) fires.
const UNPAIR_HOLD: Duration = Duration::from_millis(650);
/// Fallback only (terminals without key-release events): if no Alt-U repeat
/// arrives within this gap, assume the key was released → disarm. When the
/// Kitty keyboard protocol is active we use real release events instead.
const UNPAIR_RELEASE_GAP: Duration = Duration::from_millis(200);

struct State {
    query: String,
    cursor: usize,
    engine: Engine,
    /// Devices BlueZ currently knows — replaced wholesale on each snapshot.
    /// This is the *only* source of truth; gorm keeps no shadow state.
    bluez: BTreeMap<String, BtDevice>,
    adapter_name: String,
    show_detail: bool,
    results: Vec<BtDevice>,
    table_state: TableState,
    status: Option<(String, Instant)>,
    /// A pending pairing-agent prompt (passkey confirm/entry), shown in the
    /// status line; `prompt_input` accumulates typed passkey/PIN digits.
    prompt: Option<Prompt>,
    prompt_input: String,
    /// `Some(started_at)` while Alt-U is held; drives the unpair meter.
    unpair_arm: Option<Instant>,
    unpair_last: Instant,
    /// Latched after a forget fires; blocks re-arming until the key is released,
    /// so holding past the meter can't unpair a second device.
    unpair_consumed: bool,
}

pub fn run() -> Result<()> {
    let cfg = config::load_or_default();
    let engine = engine::spawn();
    let mut term = terminal::setup()?;
    // Negotiate the Kitty keyboard protocol so held keys report real release
    // events (a steady, jitter-free hold meter). Best-effort: false on terminals
    // that don't support it, where we fall back to the repeat-timeout heuristic.
    let kbd_enhanced = enable_kbd_enhancement(&mut term);
    let result = run_loop(&mut term, engine, &cfg, kbd_enhanced);
    disable_kbd_enhancement(&mut term, kbd_enhanced);
    terminal::restore(&mut term).ok();
    result
}

/// Push the keyboard-enhancement flags if the terminal supports them; returns
/// whether they were enabled (so the hold logic knows to trust release events).
/// We need DISAMBIGUATE so modified keys (Alt-U) come through as CSI-u with an
/// event type, and REPORT_EVENT_TYPES for the press/repeat/release kinds.
fn enable_kbd_enhancement(term: &mut Terminal<CrosstermBackend<File>>) -> bool {
    if matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        if execute!(term.backend_mut(), PushKeyboardEnhancementFlags(flags)).is_ok() {
            return true;
        }
    }
    false
}

/// Pop the flags we pushed (only if we pushed any), so the terminal isn't left
/// in enhanced mode.
fn disable_kbd_enhancement(term: &mut Terminal<CrosstermBackend<File>>, enabled: bool) {
    if enabled {
        let _ = execute!(term.backend_mut(), PopKeyboardEnhancementFlags);
    }
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<File>>,
    engine: Engine,
    cfg: &Config,
    kbd_enhanced: bool,
) -> Result<()> {
    let mut state = State {
        query: String::new(),
        cursor: 0,
        engine,
        bluez: BTreeMap::new(),
        adapter_name: String::new(),
        show_detail: cfg.preview,
        results: Vec::new(),
        table_state: TableState::default(),
        status: None,
        prompt: None,
        prompt_input: String::new(),
        unpair_arm: None,
        unpair_last: Instant::now(),
        unpair_consumed: false,
    };

    loop {
        while let Ok(fact) = state.engine.facts.try_recv() {
            state.apply(fact);
        }
        state.refresh();

        term.draw(|f| render(f, &mut state, cfg))?;

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && handle_key(key, &mut state, kbd_enhanced)
        {
            return Ok(());
        }

        let now = Instant::now();
        // Fallback release detection (no Kitty protocol): a gap in key repeats
        // means the key was let go → disarm and clear the post-fire latch.
        if !kbd_enhanced && now.duration_since(state.unpair_last) > UNPAIR_RELEASE_GAP {
            state.unpair_arm = None;
            state.unpair_consumed = false;
        }
        // Fire once Alt-U has been held long enough; latch so holding on can't
        // forget a second device.
        if let Some(start) = state.unpair_arm
            && now.duration_since(start) >= UNPAIR_HOLD
        {
            state.unpair_arm = None;
            state.unpair_consumed = true;
            if let Some((addr, name, _, _, _)) = state.selected_info() {
                state.dispatch(Cmd::Unpair(addr), format!("forgetting {name}…"));
            }
        }
    }
}

impl State {
    fn apply(&mut self, fact: Fact) {
        match fact {
            Fact::Snapshot(list) => {
                self.bluez = list.into_iter().map(|d| (d.address.clone(), d)).collect();
            }
            Fact::Adapter { name } => self.adapter_name = name,
            Fact::ActionResult {
                address,
                verb,
                error,
            } => {
                // An action resolving (esp. pairing) ends any pending prompt.
                self.clear_prompt();
                let name = self
                    .results
                    .iter()
                    .find(|d| d.address == address)
                    .and_then(|d| d.name.clone())
                    .unwrap_or(address);
                match error {
                    None => self.set_status(format!("{name}: {verb} ✓")),
                    Some(e) => self.set_status(format!("{name}: {verb} failed — {e}")),
                }
            }
            Fact::AgentPrompt(p) => {
                // A "Display" prompt is informational (code shown on the
                // device) — surface it transiently, don't enter the answer mode.
                if p.kind == PromptKind::Display {
                    self.set_status(p.message);
                } else {
                    self.prompt_input.clear();
                    self.prompt = Some(p);
                }
            }
            Fact::Error(e) => self.set_status(e),
        }
    }

    fn clear_prompt(&mut self) {
        self.prompt = None;
        self.prompt_input.clear();
    }

    /// Answer the pending prompt: `None` rejects, `Some(text)` accepts.
    fn answer_prompt(&mut self, reply: Option<String>) {
        let _ = self.engine.cmds.send(Cmd::AnswerPrompt(reply));
        self.clear_prompt();
    }

    fn selected_addr(&self) -> Option<String> {
        self.table_state
            .selected()
            .and_then(|i| self.results.get(i))
            .map(|d| d.address.clone())
    }

    fn selected(&self) -> Option<&BtDevice> {
        self.table_state
            .selected()
            .and_then(|i| self.results.get(i))
    }

    /// The selected device's (address, display-name, connected, trusted,
    /// blocked) — owned, so callers can act without holding the borrow.
    fn selected_info(&self) -> Option<(String, String, bool, bool, bool)> {
        self.selected().map(|d| {
            (
                d.address.clone(),
                d.name.clone().unwrap_or_else(|| d.address.clone()),
                d.connected,
                d.trusted,
                d.blocked,
            )
        })
    }

    /// Send a command to the engine and show an optimistic status message; the
    /// engine's `ActionResult` (and the next snapshot) confirm the outcome.
    fn dispatch(&mut self, cmd: Cmd, status: String) {
        let _ = self.engine.cmds.send(cmd);
        self.set_status(status);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            self.table_state.select(None);
            return;
        }
        let len = self.results.len() as isize;
        let cur = self.table_state.selected().unwrap_or(0) as isize;
        self.table_state
            .select(Some((cur + delta).clamp(0, len - 1) as usize));
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// Rebuild the ranked view from exactly what BlueZ knows right now,
    /// fuzzy-filtered and sorted presence-first. Selection sticks to the same
    /// address across the churn.
    fn refresh(&mut self) {
        let keep = self.selected_addr();
        let q = self.query.trim();

        let devices: Vec<BtDevice> = self.bluez.values().cloned().collect();

        let mut rows: Vec<BtDevice> = if q.is_empty() {
            devices
        } else {
            let mut matcher = fuzzy::matcher();
            let pattern = fuzzy::pattern(q);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, BtDevice)> = Vec::new();
            for d in devices {
                let hay = d.haystack();
                let h = nucleo_matcher::Utf32Str::new(&hay, &mut buf);
                if let Some(score) = pattern.score(h, &mut matcher) {
                    scored.push((score, d));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| rank_cmp(&a.1, &b.1)));
            scored.into_iter().map(|(_, d)| d).collect()
        };
        if q.is_empty() {
            rows.sort_by(rank_cmp);
        }
        self.results = rows;

        let idx = keep
            .and_then(|a| self.results.iter().position(|d| d.address == a))
            .or(if self.results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.table_state.select(idx);
    }
}

/// Rank a device into a band: your devices first, then things you could pair,
/// then anonymous beacons. "In pairing mode" isn't exposed by BlueZ, so
/// identifiable-and-unpaired (a real name or a known type) is the stand-in.
fn rank_tier(d: &BtDevice) -> u8 {
    if d.connected {
        0 // in use
    } else if d.paired {
        1 // my devices, even when offline
    } else if d.name.is_some() || d.icon.is_some() {
        2 // identifiable → likely available to pair
    } else {
        3 // anonymous beacon → noise
    }
}

/// Ordering: tier asc, then strongest signal, then name/address. Used both as
/// the no-query sort and the fuzzy-score tiebreaker.
fn rank_cmp(a: &BtDevice, b: &BtDevice) -> Ordering {
    rank_tier(a)
        .cmp(&rank_tier(b))
        .then(b.rssi.unwrap_or(i16::MIN).cmp(&a.rssi.unwrap_or(i16::MIN)))
        .then_with(|| {
            a.name
                .as_deref()
                .unwrap_or(&a.address)
                .cmp(b.name.as_deref().unwrap_or(&b.address))
        })
}

/// Returns `true` when the picker should quit.
fn handle_key(key: KeyEvent, state: &mut State, kbd_enhanced: bool) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Key-release events (only delivered under the Kitty protocol) end a hold
    // gesture immediately — the Canon Cat's key-up. We match the bare key code
    // (ignoring modifiers) since the user may lift Alt before U.
    if key.kind == KeyEventKind::Release {
        if matches!(key.code, KeyCode::Char('u')) {
            state.unpair_arm = None;
            state.unpair_consumed = false;
        }
        return false;
    }
    // Everything below is a press or auto-repeat.

    // A pending pairing prompt takes over the relevant keys (a deliberate,
    // clearly-signposted quasimode shown in the status line). Esc always
    // safely rejects, so you can never get stuck. Everything else is ignored
    // while it's up.
    if let Some(kind) = state.prompt.as_ref().map(|p| p.kind) {
        match kind {
            PromptKind::Confirm => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => state.answer_prompt(Some(String::new())),
                KeyCode::Esc | KeyCode::Char('n') => state.answer_prompt(None),
                _ => {}
            },
            PromptKind::Passkey => match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => state.prompt_input.push(c),
                KeyCode::Backspace => {
                    state.prompt_input.pop();
                }
                KeyCode::Enter => {
                    let s = std::mem::take(&mut state.prompt_input);
                    state.answer_prompt(Some(s));
                }
                KeyCode::Esc => state.answer_prompt(None),
                _ => {}
            },
            PromptKind::Pin => match key.code {
                KeyCode::Char(c) => state.prompt_input.push(c),
                KeyCode::Backspace => {
                    state.prompt_input.pop();
                }
                KeyCode::Enter => {
                    let s = std::mem::take(&mut state.prompt_input);
                    state.answer_prompt(Some(s));
                }
                KeyCode::Esc => state.answer_prompt(None),
                _ => {}
            },
            PromptKind::Display => {}
        }
        return false;
    }

    match key.code {
        // ---- quit -------------------------------------------------------
        KeyCode::Esc => return true,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return true,
        KeyCode::Char('d') if ctrl && state.query.is_empty() => return true,

        // ---- device actions --------------------------------------------
        // Enter → connect, or disconnect if already connected.
        KeyCode::Enter => {
            if let Some((addr, name, connected, _, _)) = state.selected_info() {
                if connected {
                    state.dispatch(Cmd::Disconnect(addr), format!("disconnecting {name}…"));
                } else {
                    state.dispatch(Cmd::Connect(addr), format!("connecting {name}…"));
                }
            }
        }
        // Alt-P → pair (bond). Any passkey/confirmation appears in the status line.
        KeyCode::Char('p') if alt => {
            if let Some((addr, name, _, _, _)) = state.selected_info() {
                state.dispatch(
                    Cmd::Pair(addr),
                    format!("pairing {name}… (it must be in pairing mode)"),
                );
            }
        }
        // Hold Alt-U → unpair/forget. Arms on press; the loop fires once it's
        // held long enough. A single tap does nothing — forgetting a bond is
        // destructive, so it needs a sustained gesture (no modal yes/no dialog;
        // same feel as valkyrie's force-kill). `unpair_consumed` blocks re-arm
        // after a fire until release. Without the Kitty protocol, auto-repeats
        // re-confirm the hold (with the timeout heuristic doing release).
        KeyCode::Char('u') if alt => {
            let now = Instant::now();
            if !state.unpair_consumed {
                let rearm = state.unpair_arm.is_none()
                    || (!kbd_enhanced
                        && now.duration_since(state.unpair_last) > UNPAIR_RELEASE_GAP);
                if rearm {
                    state.unpair_arm = Some(now);
                }
            }
            state.unpair_last = now;
        }
        // Ctrl-T → toggle trusted (auto-reconnect without asking).
        KeyCode::Char('t') if ctrl => {
            if let Some((addr, name, _, trusted, _)) = state.selected_info() {
                let verb = if trusted { "untrusting" } else { "trusting" };
                state.dispatch(Cmd::SetTrusted(addr, !trusted), format!("{verb} {name}…"));
            }
        }
        // Alt-B → toggle blocked (Ctrl-B is reserved for cursor-back).
        KeyCode::Char('b') if alt => {
            if let Some((addr, name, _, _, blocked)) = state.selected_info() {
                let verb = if blocked { "unblocking" } else { "blocking" };
                state.dispatch(Cmd::SetBlocked(addr, !blocked), format!("{verb} {name}…"));
            }
        }

        // Ctrl-V → detail strip on/off.
        KeyCode::Char('v') if ctrl => state.show_detail = !state.show_detail,

        // ---- list navigation -------------------------------------------
        KeyCode::Up => state.move_selection(-1),
        KeyCode::Down => state.move_selection(1),
        KeyCode::Char('p') if ctrl => state.move_selection(-1),
        KeyCode::Char('n') if ctrl => state.move_selection(1),
        KeyCode::PageUp => state.move_selection(-10),
        KeyCode::PageDown => state.move_selection(10),

        // ---- query editing (readline subset) ---------------------------
        KeyCode::Left => state.cursor = editing::prev_offset(&state.query, state.cursor),
        KeyCode::Right => state.cursor = editing::next_offset(&state.query, state.cursor),
        KeyCode::Char('b') if ctrl => {
            state.cursor = editing::prev_offset(&state.query, state.cursor)
        }
        KeyCode::Char('f') if ctrl => {
            state.cursor = editing::next_offset(&state.query, state.cursor)
        }
        KeyCode::Char('a') if ctrl => state.cursor = 0,
        KeyCode::Char('e') if ctrl => state.cursor = state.query.len(),
        KeyCode::Home => state.cursor = 0,
        KeyCode::End => state.cursor = state.query.len(),
        KeyCode::Backspace => {
            editing::delete_back(&mut state.query, &mut state.cursor);
        }
        KeyCode::Char('h') if ctrl => {
            editing::delete_back(&mut state.query, &mut state.cursor);
        }
        KeyCode::Char('u') if ctrl => {
            state.query.clear();
            state.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => {
            editing::delete_word(&mut state.query, &mut state.cursor);
        }

        // ---- typed char → query ----------------------------------------
        KeyCode::Char(c) if !ctrl && !alt => {
            state.query.insert(state.cursor, c);
            state.cursor += c.len_utf8();
        }
        _ => {}
    }
    false
}

// ---- rendering ------------------------------------------------------------

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    let detail_h = if state.show_detail { 4 } else { 0 };
    let chunks = LayoutWidget::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),           // list
            Constraint::Length(detail_h), // detail strip
            Constraint::Length(1),        // status / toolbar
            Constraint::Length(1),        // search prompt
        ])
        .split(area);
    render_list(f, state, cfg, chunks[0]);
    if detail_h > 0 {
        render_detail(f, state, cfg, chunks[1]);
    }
    render_status(f, state, cfg, chunks[2]);
    render_prompt(f, state, cfg, chunks[3]);
}

fn render_prompt(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "› ",
            Style::default()
                .fg(cfg.colors.prompt_fg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(state.query.clone()),
    ]);
    f.render_widget(Paragraph::new(line), area);
    if area.width > 0 {
        let chars_before = state.query[..state.cursor].chars().count() as u16;
        let cursor_x = (area.x + 2).saturating_add(chars_before);
        let max_x = area.x.saturating_add(area.width).saturating_sub(1);
        f.set_cursor_position((cursor_x.min(max_x), area.y));
    }
}

fn render_status(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let warn = Style::default().fg(cfg.colors.warn_fg.to_ratatui());
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());

    // Priority: pairing prompt > unpair meter > recent action message > toolbar.
    let line = if let Some(p) = &state.prompt {
        // The pairing request, surfaced inline at the locus of attention — not
        // a modal that covers the list. A device-name prefix when we know it.
        let who = state
            .results
            .iter()
            .find(|d| d.address == p.address)
            .and_then(|d| d.name.clone());
        let mut spans = vec![Span::styled(" ", warn)];
        if matches!(p.kind, PromptKind::Passkey | PromptKind::Pin) {
            spans.push(Span::styled(format!("{} ", p.message), warn));
            spans.push(Span::styled(
                state.prompt_input.clone(),
                warn.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("▏", warn)); // entry caret
        } else {
            let prefix = who.map(|n| format!("{n}: ")).unwrap_or_default();
            spans.push(Span::styled(format!("{prefix}{}", p.message), warn));
        }
        Line::from(spans)
    } else if let Some(start) = state.unpair_arm {
        let name = state
            .selected()
            .and_then(|d| d.name.clone())
            .unwrap_or_else(|| "device".into());
        let frac = start.elapsed().as_secs_f32() / UNPAIR_HOLD.as_secs_f32();
        Line::from(Span::styled(
            format!(" hold M-u to forget {name}  {}", meter(frac)),
            warn,
        ))
    } else if let Some((msg, at)) = &state.status {
        if at.elapsed() < STATUS_LINGER {
            Line::from(Span::styled(format!(" {msg}"), warn))
        } else {
            Line::from(Span::styled(toolbar(state), dim))
        }
    } else {
        Line::from(Span::styled(toolbar(state), dim))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The idle help line.
fn toolbar(state: &State) -> String {
    let adapter = if state.adapter_name.is_empty() {
        "—"
    } else {
        &state.adapter_name
    };
    format!(
        " {n} · {adapter} · Enter (dis)connect · M-p pair · hold M-u forget · ^T trust · M-b block · ^V detail · Esc",
        n = state.results.len(),
    )
}

/// A 5-cell progress meter for the hold-to-forget gesture.
fn meter(frac: f32) -> String {
    let n = 5;
    let filled = (frac.clamp(0.0, 1.0) * n as f32).round() as usize;
    (0..n).map(|i| if i < filled { '▰' } else { '▱' }).collect()
}

fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let dim = cfg.colors.status_fg.to_ratatui();
    let matched_style = Style::default()
        .fg(cfg.colors.match_fg.to_ratatui())
        .add_modifier(Modifier::BOLD);

    let q = state.query.trim();
    let pattern = (!q.is_empty()).then(|| fuzzy::pattern(q));
    let mut matcher = fuzzy::matcher();

    let header = TableRow::new(["NAME", "TYPE", "SIG", "BATT", "STATE", "ADDRESS"])
        .style(Style::default().fg(dim).add_modifier(Modifier::BOLD));

    let rows: Vec<TableRow> = state
        .results
        .iter()
        .map(|d| {
            // Dim the anonymous beacons (the bottom tier) so your devices and
            // pairable candidates stand out; everything else is full-brightness.
            let base = if rank_tier(d) == 3 {
                Style::default().fg(dim)
            } else {
                Style::default()
            };
            let name = d.name.clone().unwrap_or_else(|| "(unknown)".into());
            let icon = d.icon.clone().unwrap_or_else(|| "-".into());
            let cell = |t: &str, m: &mut nucleo_matcher::Matcher| {
                Cell::from(highlight_cell(t, pattern.as_ref(), m, base, matched_style))
            };
            TableRow::new(vec![
                cell(&name, &mut matcher),
                cell(&icon, &mut matcher),
                Cell::from(Span::styled(signal_cell(d.rssi), base)),
                Cell::from(Span::styled(
                    d.battery
                        .map(|b| format!("{b}%"))
                        .unwrap_or_else(|| "·".into()),
                    base,
                )),
                Cell::from(Span::styled(state_cell(d), base)),
                cell(&d.address, &mut matcher),
            ])
        })
        .collect();

    let widths = [
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(11),
        Constraint::Length(17),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(table, area, &mut state.table_state);
}

/// Compact device-state label: connection/bond plus a trust tick / block mark.
fn state_cell(d: &BtDevice) -> String {
    if d.blocked {
        return "blocked".into();
    }
    let mut s = if d.connected {
        "conn".to_string()
    } else if d.paired {
        "paired".to_string()
    } else {
        "·".to_string()
    };
    if d.trusted {
        s.push_str(" ✓");
    }
    s
}

/// A 4-cell signal gauge from RSSI (dBm). `·` when the device isn't currently
/// advertising (out of range, or already connected so it's gone quiet).
fn signal_cell(rssi: Option<i16>) -> String {
    let Some(r) = rssi else {
        return "·".to_string();
    };
    let bars = match r {
        x if x >= -50 => 4,
        x if x >= -65 => 3,
        x if x >= -78 => 2,
        x if x >= -90 => 1,
        _ => 0,
    };
    let glyphs = ["▁", "▃", "▅", "▇"];
    (0..4)
        .map(|i| if i < bars { glyphs[i] } else { " " })
        .collect()
}

fn highlight_cell(
    text: &str,
    pattern: Option<&nucleo_matcher::pattern::Pattern>,
    matcher: &mut nucleo_matcher::Matcher,
    base: Style,
    matched_style: Style,
) -> Line<'static> {
    let text = sanitize_display(text);
    let Some(pattern) = pattern else {
        return Line::from(Span::styled(text, base));
    };
    let mut buf = Vec::new();
    let mut idx = Vec::new();
    let hay = nucleo_matcher::Utf32Str::new(&text, &mut buf);
    if pattern.indices(hay, matcher, &mut idx).is_none() {
        return Line::from(Span::styled(text, base));
    }
    idx.sort_unstable();
    idx.dedup();

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_match = false;
    let mut it = idx.iter().copied().peekable();
    for (i, c) in text.chars().enumerate() {
        let is_match = it.peek() == Some(&(i as u32));
        if is_match {
            it.next();
        }
        if is_match != run_match && !run.is_empty() {
            let style = if run_match { matched_style } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_match = is_match;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(
            run,
            if run_match { matched_style } else { base },
        ));
    }
    Line::from(spans)
}

/// Horizontal detail strip: everything gorm knows about the selected device.
fn render_detail(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());
    let block = Block::default().borders(Borders::TOP).border_style(dim);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(d) = state.selected() else {
        return;
    };
    let pair = |k: &str, v: String| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{k} "), dim),
            Span::raw(sanitize_display(&v)),
        ]
    };
    let yn = |b: bool| if b { "yes" } else { "no" };

    let mut l1 = pair("name", d.name.clone().unwrap_or_else(|| "(unknown)".into()));
    l1.push(Span::styled("   addr ", dim));
    l1.push(Span::raw(d.address.clone()));
    l1.push(Span::styled("   type ", dim));
    l1.push(Span::raw(sanitize_display(
        d.icon.as_deref().unwrap_or("-"),
    )));

    let mut l2 = pair("paired", yn(d.paired).into());
    l2.push(Span::styled("   connected ", dim));
    l2.push(Span::raw(yn(d.connected)));
    l2.push(Span::styled("   trusted ", dim));
    l2.push(Span::raw(yn(d.trusted)));
    l2.push(Span::styled("   blocked ", dim));
    l2.push(Span::raw(yn(d.blocked)));

    let mut l3 = pair(
        "rssi",
        d.rssi
            .map(|r| format!("{r} dBm"))
            .unwrap_or_else(|| "—".into()),
    );
    l3.push(Span::styled("   battery ", dim));
    l3.push(Span::raw(
        d.battery
            .map(|b| format!("{b}%"))
            .unwrap_or_else(|| "—".into()),
    ));

    f.render_widget(
        Paragraph::new(vec![Line::from(l1), Line::from(l2), Line::from(l3)]),
        inner,
    );
}
