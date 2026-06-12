//! The live audio picker. Same shape as gorm's device picker (event loop,
//! readline query keymap, fzf-style highlights, horizontal detail strip), over
//! bragi's PipeWire engine. One flat type-tagged list — sinks, sources, and
//! app streams together, one fuzzy prompt, no tabs or modes. The engine
//! streams whole [`AudioState`] snapshots; the TUI keeps the latest and
//! re-derives its view every frame, so the list is always live.
//!
//! Actions are type-aware with no menus: Enter does the obvious thing per row
//! (stream → device chooser to move it; device → make it default), Alt-←/→
//! nudges volume (±5%, ±1% with Shift), Alt-M toggles mute, Alt-D is the
//! explicit set-default. The picker stays open after an action — the list
//! live-updating *is* the confirmation — so a routing fix and a volume nudge
//! are one session. The only modal hop is the device chooser (hugin's Ctrl-O
//! MIME-chooser pattern), and Esc always backs out of it.

use std::collections::BTreeMap;
use std::fs::File;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use levelup_core::{fuzzy, sanitize_display};
use levelup_tui::{editing, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row as TableRow, Table, TableState,
};

use crate::config::{self, Config};
use crate::engine::{self, Cmd, Engine};
use crate::state::{AudioNode, AudioState, NodeKind};

const TICK: Duration = Duration::from_millis(50);
const STATUS_LINGER: Duration = Duration::from_secs(4);

struct State {
    query: String,
    cursor: usize,
    engine: Engine,
    /// Latest snapshot from the engine — the only source of truth; the TUI
    /// keeps no shadow state.
    audio: AudioState,
    /// Set when the engine channel closes (PipeWire died); shown persistently.
    engine_dead: bool,
    show_detail: bool,
    results: Vec<AudioNode>,
    table_state: TableState,
    status: Option<(String, Instant)>,
    /// An open device chooser ("move this stream to…"); replaces the list
    /// until answered or dismissed.
    chooser: Option<Chooser>,
}

/// The move-stream device chooser.
struct Chooser {
    stream: u32,
    stream_name: String,
    /// Candidate devices: (node id, display label).
    options: Vec<(u32, String)>,
    sel: usize,
}

pub fn run() -> Result<()> {
    let cfg = config::load_or_default();
    let engine = engine::spawn();
    let mut term = terminal::setup()?;
    let result = run_loop(&mut term, engine, &cfg);
    terminal::restore(&mut term).ok();
    result
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<File>>,
    engine: Engine,
    cfg: &Config,
) -> Result<()> {
    let mut state = State {
        query: String::new(),
        cursor: 0,
        engine,
        audio: AudioState::default(),
        engine_dead: false,
        show_detail: cfg.preview,
        results: Vec::new(),
        table_state: TableState::default(),
        status: None,
        chooser: None,
    };

    loop {
        loop {
            match state.engine.updates.try_recv() {
                Ok(snapshot) => state.audio = snapshot,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.engine_dead = true;
                    break;
                }
            }
        }
        state.refresh();

        term.draw(|f| render(f, &mut state, cfg))?;

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && handle_key(key, &mut state)
        {
            return Ok(());
        }
    }
}

impl State {
    fn selected(&self) -> Option<&AudioNode> {
        self.table_state
            .selected()
            .and_then(|i| self.results.get(i))
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// Send a command to the engine; the next snapshot confirms the outcome.
    fn dispatch(&mut self, cmd: Cmd) {
        if self.engine.cmds.send(cmd).is_err() {
            self.engine_dead = true;
        }
    }

    /// Enter on a stream: open the chooser over devices it could move to.
    fn open_chooser(&mut self, stream: &AudioNode) {
        let wanted = match stream.kind {
            NodeKind::Playback => NodeKind::Sink,
            NodeKind::Record => NodeKind::Source,
            _ => return,
        };
        let current = self.audio.target_of(stream).map(|t| t.id);
        let options: Vec<(u32, String)> = self
            .audio
            .nodes
            .values()
            .filter(|n| n.kind == wanted)
            .map(|n| {
                let mut label = n.name.clone();
                if Some(n.id) == current {
                    label.push_str("  (current)");
                }
                (n.id, label)
            })
            .collect();
        if options.is_empty() {
            self.set_status(format!("no {}s to move to", wanted.label()));
            return;
        }
        let sel = options
            .iter()
            .position(|&(id, _)| Some(id) == current)
            .unwrap_or(0);
        self.chooser = Some(Chooser {
            stream: stream.id,
            stream_name: stream.name.clone(),
            options,
            sel,
        });
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

    /// Rebuild the view from the latest snapshot: devices first then streams
    /// when browsing, nucleo-scored when filtering. Selection sticks to the
    /// same node id across the churn.
    fn refresh(&mut self) {
        let keep = self.selected().map(|n| n.id);
        let q = self.query.trim();

        let nodes: Vec<AudioNode> = self.audio.nodes.values().cloned().collect();
        self.results = if q.is_empty() {
            let mut rows = nodes;
            rows.sort_by_key(|n| (rank(n.kind), n.id));
            rows
        } else {
            let mut matcher = fuzzy::matcher();
            let pattern = fuzzy::pattern(q);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, AudioNode)> = Vec::new();
            for n in nodes {
                let hay = self.haystack(&n);
                let h = nucleo_matcher::Utf32Str::new(&hay, &mut buf);
                if let Some(score) = pattern.score(h, &mut matcher) {
                    scored.push((score, n));
                }
            }
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| rank(a.1.kind).cmp(&rank(b.1.kind)))
                    .then(a.1.id.cmp(&b.1.id))
            });
            scored.into_iter().map(|(_, n)| n).collect()
        };

        let idx = keep
            .and_then(|id| self.results.iter().position(|n| n.id == id))
            .or(if self.results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.table_state.select(idx);
    }

    /// What the fuzzy pattern runs against: type tag, names, what a stream is
    /// playing, and where it's routed — so "sink", "firefox", and "xm4" all
    /// find the right rows.
    fn haystack(&self, n: &AudioNode) -> String {
        let mut hay = format!("{} {}", n.kind.label(), n.name);
        if let Some(d) = &n.detail {
            hay.push(' ');
            hay.push_str(d);
        }
        if let Some(t) = self.audio.target_of(n) {
            hay.push(' ');
            hay.push_str(&t.name);
        }
        hay
    }
}

/// Browse order: output devices, input devices, playing streams, recorders.
fn rank(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Sink => 0,
        NodeKind::Source => 1,
        NodeKind::Playback => 2,
        NodeKind::Record => 3,
    }
}

/// Returns `true` when the picker should quit.
fn handle_key(key: KeyEvent, state: &mut State) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // An open device chooser takes over the keys; Esc always backs out.
    if state.chooser.is_some() {
        return handle_chooser_key(key, state, ctrl);
    }

    match key.code {
        // ---- quit -------------------------------------------------------
        KeyCode::Esc => return true,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return true,
        KeyCode::Char('d') if ctrl && state.query.is_empty() => return true,

        // ---- actions ----------------------------------------------------
        // Enter is type-aware: stream → chooser to move it; device → default.
        KeyCode::Enter => {
            if let Some(n) = state.selected().cloned() {
                if n.kind.is_device() {
                    state.set_status(format!("default {} → {}", n.kind.label(), n.name));
                    state.dispatch(Cmd::SetDefault { device: n.id });
                } else {
                    state.open_chooser(&n);
                }
            }
        }
        // Alt-D → explicit set-default (devices only).
        KeyCode::Char('d') if alt => {
            if let Some(n) = state.selected().filter(|n| n.kind.is_device()).cloned() {
                state.set_status(format!("default {} → {}", n.kind.label(), n.name));
                state.dispatch(Cmd::SetDefault { device: n.id });
            }
        }
        // Alt-←/→ → volume down/up; ±5%, ±1% with Shift. The engine computes
        // from its own state, so held-key repeats don't compound a stale base.
        KeyCode::Left | KeyCode::Right if alt => {
            if let Some(id) = state.selected().map(|n| n.id) {
                let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    0.01
                } else {
                    0.05
                };
                let delta = if key.code == KeyCode::Left {
                    -step
                } else {
                    step
                };
                state.dispatch(Cmd::AdjustVolume { node: id, delta });
            }
        }
        // Alt-M → mute toggle.
        KeyCode::Char('m') if alt => {
            if let Some(id) = state.selected().map(|n| n.id) {
                state.dispatch(Cmd::ToggleMute { node: id });
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

/// Device chooser keys: Up/Down move, Enter moves the stream, Esc backs out.
fn handle_chooser_key(key: KeyEvent, state: &mut State, ctrl: bool) -> bool {
    let Some(chooser) = state.chooser.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Char('c') if ctrl => return true,
        KeyCode::Esc => state.chooser = None,
        KeyCode::Char('g') if ctrl => state.chooser = None,
        KeyCode::Up => chooser.sel = chooser.sel.saturating_sub(1),
        KeyCode::Down => chooser.sel = (chooser.sel + 1).min(chooser.options.len() - 1),
        KeyCode::Char('p') if ctrl => chooser.sel = chooser.sel.saturating_sub(1),
        KeyCode::Char('n') if ctrl => {
            chooser.sel = (chooser.sel + 1).min(chooser.options.len() - 1)
        }
        KeyCode::Enter => {
            let (target, label) = chooser.options[chooser.sel].clone();
            let stream = chooser.stream;
            let name = chooser.stream_name.clone();
            state.chooser = None;
            state.set_status(format!("moving {name} → {label}"));
            state.dispatch(Cmd::Move { stream, target });
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
    if state.chooser.is_some() {
        render_chooser(f, state, cfg, chunks[0]);
    } else {
        render_list(f, state, cfg, chunks[0]);
    }
    if detail_h > 0 {
        render_detail(f, state, cfg, chunks[1]);
    }
    render_status(f, state, cfg, chunks[2]);
    render_prompt(f, state, cfg, chunks[3]);
}

/// The move-stream device chooser, in place of the list (hugin's MIME-chooser
/// pattern).
fn render_chooser(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let Some(chooser) = &state.chooser else {
        return;
    };
    let items: Vec<ListItem> = chooser
        .options
        .iter()
        .map(|(_, label)| ListItem::new(Line::from(Span::raw(sanitize_display(label)))))
        .collect();
    let mut ls = ListState::default();
    ls.select(Some(
        chooser.sel.min(chooser.options.len().saturating_sub(1)),
    ));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut ls);
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

    let line = if state.engine_dead {
        Line::from(Span::styled(
            " PipeWire connection lost — showing the last known state",
            warn,
        ))
    } else if let Some(chooser) = &state.chooser {
        Line::from(Span::styled(
            format!(
                " move {} to…  ·  Enter move · Esc back",
                sanitize_display(&chooser.stream_name)
            ),
            dim,
        ))
    } else if let Some((msg, at)) = state
        .status
        .as_ref()
        .filter(|(_, at)| at.elapsed() < STATUS_LINGER)
    {
        let _ = at;
        Line::from(Span::styled(format!(" {msg}"), warn))
    } else {
        Line::from(Span::styled(toolbar(state), dim))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The idle help line.
fn toolbar(state: &State) -> String {
    format!(
        " {n} · Enter move/default · M-←/→ vol · M-m mute · ^V detail · Esc",
        n = state.results.len(),
    )
}

fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let dim = cfg.colors.status_fg.to_ratatui();
    let warn = Style::default().fg(cfg.colors.warn_fg.to_ratatui());
    let matched_style = Style::default()
        .fg(cfg.colors.match_fg.to_ratatui())
        .add_modifier(Modifier::BOLD);

    let q = state.query.trim();
    let pattern = (!q.is_empty()).then(|| fuzzy::pattern(q));
    let mut matcher = fuzzy::matcher();

    let header = TableRow::new(["TYPE", "NAME", "VOL", ""])
        .style(Style::default().fg(dim).add_modifier(Modifier::BOLD));

    let audio = &state.audio;
    let rows: Vec<TableRow> = state
        .results
        .iter()
        .map(|n| {
            let base = Style::default();
            let cell = |t: &str, m: &mut nucleo_matcher::Matcher| {
                Cell::from(highlight_cell(t, pattern.as_ref(), m, base, matched_style))
            };

            // `sink *` marks the default device of its kind.
            let tag = if audio.is_default(n) {
                format!("{} *", n.kind.label())
            } else {
                n.kind.label().to_string()
            };

            let mut name = n.name.clone();
            if let Some(target) = audio.target_of(n) {
                name.push_str(&format!(" → {}", target.name));
            }
            if !n.kind.is_device()
                && let Some(detail) = &n.detail
                && detail != &n.name
            {
                name.push_str(&format!("  ({detail})"));
            }

            let vol_cell = if n.mute {
                Cell::from(Span::styled("muted", warn))
            } else {
                Cell::from(Span::styled(gauge(n.volume), Style::default().fg(dim)))
            };
            let pct = n
                .volume_pct()
                .map(|p| format!("{p:>3}%"))
                .unwrap_or_else(|| "   ·".into());

            TableRow::new(vec![
                cell(&tag, &mut matcher),
                cell(&name, &mut matcher),
                vol_cell,
                Cell::from(Span::styled(pct, base)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(4),
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

/// A 5-cell volume gauge; UI volume can exceed 1.0, the gauge clamps.
fn gauge(volume: Option<f32>) -> String {
    let Some(v) = volume else {
        return "  ·  ".into();
    };
    let filled = (v.clamp(0.0, 1.0) * 5.0).round() as usize;
    (0..5).map(|i| if i < filled { '▮' } else { '▯' }).collect()
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

/// Horizontal detail strip: everything bragi knows about the selected node.
fn render_detail(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());
    let block = Block::default().borders(Borders::TOP).border_style(dim);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(n) = state.selected() else {
        return;
    };
    let pair = |k: &str, v: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{k} "), dim),
            Span::raw(sanitize_display(v)),
        ]
    };

    let mut l1 = pair("name", &n.name);
    l1.push(Span::styled("   type ", dim));
    l1.push(Span::raw(n.kind.label()));
    if state.audio.is_default(n) {
        l1.push(Span::raw(" (default)"));
    }
    l1.push(Span::styled("   id ", dim));
    l1.push(Span::raw(n.id.to_string()));

    let mut l2 = pair(
        "vol",
        &n.volume_pct()
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".into()),
    );
    l2.push(Span::styled("   muted ", dim));
    l2.push(Span::raw(if n.mute { "yes" } else { "no" }));
    if let Some(d) = &n.detail {
        l2.push(Span::styled("   playing ", dim));
        l2.push(Span::raw(sanitize_display(d)));
    }

    // Third line: routing — where a stream goes, or what a device is carrying.
    let l3 = if n.kind.is_device() {
        let feeding: Vec<String> = streams_of(&state.audio, n)
            .map(|s| sanitize_display(&s.name))
            .collect();
        let mut l = pair(
            "streams",
            &(if feeding.is_empty() {
                "—".to_string()
            } else {
                feeding.join(", ")
            }),
        );
        l.push(Span::styled("   node ", dim));
        l.push(Span::raw(sanitize_display(&n.node_name)));
        l
    } else {
        let mut l = pair(
            "→",
            &state
                .audio
                .target_of(n)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "—".into()),
        );
        l.push(Span::styled("   node ", dim));
        l.push(Span::raw(sanitize_display(&n.node_name)));
        l
    };

    f.render_widget(
        Paragraph::new(vec![Line::from(l1), Line::from(l2), Line::from(l3)]),
        inner,
    );
}

/// Streams currently routed to a device (the reverse of `target_of`).
fn streams_of<'a>(
    audio: &'a AudioState,
    device: &'a AudioNode,
) -> impl Iterator<Item = &'a AudioNode> {
    // Collect distinct peer ids first — several links per stream (one per channel).
    let peers: BTreeMap<u32, ()> = audio
        .links
        .values()
        .filter_map(|&(out, inp)| match device.kind {
            NodeKind::Sink if inp == device.id => Some((out, ())),
            NodeKind::Source if out == device.id => Some((inp, ())),
            _ => None,
        })
        .collect();
    peers
        .into_keys()
        .filter_map(|id| audio.nodes.get(&id).filter(|n| !n.kind.is_device()))
}
