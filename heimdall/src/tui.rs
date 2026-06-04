//! The live device picker. Ported from valkyrie's `/dev/tty` picker (event
//! loop, readline query keymap, fzf-style list, horizontal detail strip). What's
//! new: it drains the discovery engine's channel each tick and merges devices by
//! IP, so the list fills in live; each device shows its last-seen age and **dims
//! when it goes quiet, dropping after a timeout** — the view is who's up *now*.

use std::collections::BTreeMap;
use std::fs::File;
use std::net::Ipv4Addr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use levelup_core::sanitize_display;
use levelup_tui::{editing, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TableRow, Table, TableState};
use rusqlite::Connection;

use crate::actions;
use crate::cache;
use crate::config::{self, Config};
use crate::device::{Device, Seen};
use crate::discover::{self, engine, engine::Engine};

const TICK: Duration = Duration::from_millis(100);
/// Dim a device once it hasn't been re-seen for this long (going quiet).
const WARN_SECS: u64 = 25;
/// Drop a device entirely after this long (offline).
const DROP: Duration = Duration::from_secs(120);

struct Entry {
    device: Device,
    last_seen: Instant,
}

struct Row {
    device: Device,
    last_seen: Instant,
}

struct State {
    query: String,
    cursor: usize,
    engine: Engine,
    devices: BTreeMap<Ipv4Addr, Entry>,
    results: Vec<Row>,
    table_state: TableState,
    cidrs: String,
    /// Results of on-demand port scans flow back here.
    ports_tx: Sender<(Ipv4Addr, Vec<u16>)>,
    ports_rx: Receiver<(Ipv4Addr, Vec<u16>)>,
    /// Transient action feedback (message + when set).
    status: Option<(String, Instant)>,
    /// MAC-keyed speed cache (None if it couldn't be opened).
    cache: Option<Connection>,
    last_flush: Instant,
}

/// What the run loop must do after a key (most actions handle themselves; ssh
/// needs the terminal suspended, which only the loop can do).
enum Step {
    Continue,
    Quit,
    Ssh(Ipv4Addr),
}

/// How long an action message lingers.
const STATUS_LINGER: Duration = Duration::from_secs(4);

pub fn run() -> Result<()> {
    let cfg = config::load_or_default();
    let cidrs = discover::local_cidrs().join(", ");
    let cache = cache::open();
    let engine = engine::spawn();
    let mut term = terminal::setup()?;
    let result = run_loop(&mut term, engine, cidrs, cache, &cfg);
    terminal::restore(&mut term).ok();
    result
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<File>>,
    engine: Engine,
    cidrs: String,
    cache: Option<Connection>,
    cfg: &Config,
) -> Result<()> {
    let (ports_tx, ports_rx) = mpsc::channel();
    let now = Instant::now();
    let mut state = State {
        query: String::new(),
        cursor: 0,
        engine,
        devices: BTreeMap::new(),
        results: Vec::new(),
        table_state: TableState::default(),
        cidrs,
        ports_tx,
        ports_rx,
        status: None,
        cache,
        last_flush: now,
    };

    // Seed from the cache so relaunch shows last-known devices instantly,
    // dimmed (last-seen pushed back past the "quiet" threshold) until the live
    // engine confirms — or the liveness timeout drops them.
    if let Some(c) = &state.cache {
        let seed = now
            .checked_sub(Duration::from_secs(WARN_SECS + 5))
            .unwrap_or(now);
        for cd in cache::load(c) {
            let mut device = Device::new(cd.ip);
            device.mac = Some(cd.mac);
            device.vendor = cd.vendor;
            device.hostname = cd.hostname;
            device.services = cd.services;
            state.devices.insert(
                cd.ip,
                Entry {
                    device,
                    last_seen: seed,
                },
            );
        }
    }

    loop {
        // Drain everything the engine has discovered since last tick.
        while let Ok(seen) = state.engine.rx.try_recv() {
            state.merge(seen);
        }
        // Apply any finished port scans.
        while let Ok((ip, ports)) = state.ports_rx.try_recv() {
            if let Some(e) = state.devices.get_mut(&ip) {
                e.device.open_ports = ports;
            }
        }
        // Drop devices that have gone offline.
        let now = Instant::now();
        state
            .devices
            .retain(|_, e| now.duration_since(e.last_seen) < DROP);
        state.refresh();

        // Periodically persist what we know (so relaunch is instant).
        if state.last_flush.elapsed() > Duration::from_secs(15) {
            flush_cache(&state.cache, &state.devices);
            state.last_flush = Instant::now();
        }

        term.draw(|f| render(f, &mut state, cfg))?;

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match handle_key(key, &mut state) {
                Step::Continue => {}
                Step::Quit => {
                    flush_cache(&state.cache, &state.devices);
                    return Ok(());
                }
                Step::Ssh(ip) => {
                    // ssh takes over the terminal: drop the alternate screen,
                    // run it in the foreground, then re-enter the picker.
                    terminal::restore(term).ok();
                    let _ = std::process::Command::new("ssh")
                        .arg(ip.to_string())
                        .status();
                    *term = terminal::setup()?;
                    let _ = term.clear();
                    state.set_status(format!("returned from ssh {ip}"));
                }
            }
        }
    }
}

impl State {
    fn merge(&mut self, seen: Seen) {
        let now = Instant::now();
        let e = self.devices.entry(seen.ip).or_insert_with(|| Entry {
            device: Device::new(seen.ip),
            last_seen: now,
        });
        e.last_seen = now;
        if let Some(m) = seen.mac {
            e.device.mac = Some(m);
        }
        if let Some(v) = seen.vendor {
            e.device.vendor = Some(v);
        }
        if let Some(h) = seen.hostname
            && e.device.hostname.is_none()
        {
            e.device.hostname = Some(h);
        }
        if let Some(s) = seen.service {
            e.device.add_service(&s);
        }
    }

    fn selected_ip(&self) -> Option<Ipv4Addr> {
        self.table_state
            .selected()
            .and_then(|i| self.results.get(i))
            .map(|r| r.device.ip)
    }

    fn selected(&self) -> Option<&Row> {
        self.table_state
            .selected()
            .and_then(|i| self.results.get(i))
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

    /// Rebuild the ranked view from the device map, keeping the selection on the
    /// same IP across the live churn.
    fn refresh(&mut self) {
        let keep = self.selected_ip();
        let q = self.query.trim();
        let mut rows: Vec<Row> = if q.is_empty() {
            self.devices
                .values()
                .map(|e| Row {
                    device: e.device.clone(),
                    last_seen: e.last_seen,
                })
                .collect()
        } else {
            let mut matcher = levelup_core::fuzzy::matcher();
            let pattern = levelup_core::fuzzy::pattern(q);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, Row)> = Vec::new();
            for e in self.devices.values() {
                let hay = e.device.haystack();
                let h = nucleo_matcher::Utf32Str::new(&hay, &mut buf);
                if let Some(score) = pattern.score(h, &mut matcher) {
                    scored.push((
                        score,
                        Row {
                            device: e.device.clone(),
                            last_seen: e.last_seen,
                        },
                    ));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.device.ip.cmp(&b.1.device.ip)));
            scored.into_iter().map(|(_, r)| r).collect()
        };
        if q.is_empty() {
            rows.sort_by_key(|r| r.device.ip);
        }
        self.results = rows;

        let idx = keep
            .and_then(|ip| self.results.iter().position(|r| r.device.ip == ip))
            .or(if self.results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.table_state.select(idx);
    }
}

fn handle_key(key: KeyEvent, state: &mut State) -> Step {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // ---- quit -------------------------------------------------------
        KeyCode::Esc => return Step::Quit,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return Step::Quit,
        KeyCode::Char('d') if ctrl && state.query.is_empty() => return Step::Quit,

        // ---- actions (non-letter, so they don't collide with the query) -
        // Enter → open the device's web UI; the common "go look at it".
        KeyCode::Enter => {
            if let Some(ip) = state.selected_ip() {
                actions::open_url(&format!("http://{ip}"));
                state.set_status(format!("opening http://{ip}"));
            }
        }
        // Ctrl-S → ssh (the loop suspends the terminal to run it).
        KeyCode::Char('s') if ctrl => {
            if let Some(ip) = state.selected_ip() {
                return Step::Ssh(ip);
            }
        }
        // Ctrl-Y → yank the IP to the clipboard.
        KeyCode::Char('y') if ctrl => {
            if let Some(ip) = state.selected_ip() {
                let msg = if actions::copy(&ip.to_string()) {
                    format!("copied {ip}")
                } else {
                    "no clipboard helper (wl-copy/xclip)".to_string()
                };
                state.set_status(msg);
            }
        }
        // Tab → on-demand TCP port scan of the selected device.
        KeyCode::Tab => {
            if let Some(ip) = state.selected_ip() {
                actions::scan_ports(ip, state.ports_tx.clone());
                state.set_status(format!("scanning ports on {ip}…"));
            }
        }

        // ---- list navigation -------------------------------------------
        KeyCode::Up => state.move_selection(-1),
        KeyCode::Down => state.move_selection(1),
        KeyCode::Char('p') if ctrl => state.move_selection(-1),
        KeyCode::Char('n') if ctrl => state.move_selection(1),
        KeyCode::PageUp => state.move_selection(-10),
        KeyCode::PageDown => state.move_selection(10),

        // ---- query editing (readline subset) ---------------------------
        KeyCode::Left => state.cursor = prev_off(&state.query, state.cursor),
        KeyCode::Right => state.cursor = next_off(&state.query, state.cursor),
        KeyCode::Char('b') if ctrl => state.cursor = prev_off(&state.query, state.cursor),
        KeyCode::Char('f') if ctrl => state.cursor = next_off(&state.query, state.cursor),
        KeyCode::Char('a') if ctrl => state.cursor = 0,
        KeyCode::Char('e') if ctrl => state.cursor = state.query.len(),
        KeyCode::Home => state.cursor = 0,
        KeyCode::End => state.cursor = state.query.len(),
        KeyCode::Backspace => delete_back(state),
        KeyCode::Char('h') if ctrl => delete_back(state),
        KeyCode::Char('u') if ctrl => {
            state.query.clear();
            state.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => delete_word(state),

        // ---- typed char → query ----------------------------------------
        KeyCode::Char(c) if !ctrl => {
            state.query.insert(state.cursor, c);
            state.cursor += c.len_utf8();
        }
        _ => {}
    }
    Step::Continue
}

fn prev_off(s: &str, pos: usize) -> usize {
    editing::prev_offset(s, pos)
}
fn next_off(s: &str, pos: usize) -> usize {
    editing::next_offset(s, pos)
}
fn delete_back(state: &mut State) {
    editing::delete_back(&mut state.query, &mut state.cursor);
}
fn delete_word(state: &mut State) {
    editing::delete_word(&mut state.query, &mut state.cursor);
}

// ---- rendering ------------------------------------------------------------

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    let detail_h = if cfg.preview { 4 } else { 0 };
    // Bottom-anchored like the sibling tools: list on top, then the detail
    // strip, with the status/toolbar and search prompt pinned to the bottom.
    let chunks = LayoutWidget::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),           // list
            Constraint::Length(detail_h), // detail strip
            Constraint::Length(1),        // status / toolbar
            Constraint::Length(1),        // search prompt (bottom row)
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
    // A recent action message takes the line; otherwise the live summary + keys.
    let line = match &state.status {
        Some((msg, at)) if at.elapsed() < STATUS_LINGER => Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(cfg.colors.warn_fg.to_ratatui()),
        )),
        _ => Line::from(Span::styled(
            format!(
                " {n} online · {cidrs} · Enter open · ^S ssh · ^Y copy · Tab port-scan · Esc",
                n = state.results.len(),
                cidrs = state.cidrs,
            ),
            Style::default().fg(cfg.colors.status_fg.to_ratatui()),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

/// A flexible table: IP/MAC are fixed-width; VENDOR/HOST/SERVICES share the
/// remaining width (so they expand on a wide terminal and truncate on a narrow
/// one). A row dims once the device goes quiet, and fuzzy-matched characters in
/// each cell are highlighted in the match colour — the same search feel as the
/// sibling tools.
fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let now = Instant::now();
    let dim = cfg.colors.status_fg.to_ratatui();
    let matched_style = Style::default()
        .fg(cfg.colors.match_fg.to_ratatui())
        .add_modifier(Modifier::BOLD);

    let q = state.query.trim();
    let pattern = (!q.is_empty()).then(|| levelup_core::fuzzy::pattern(q));
    let mut matcher = levelup_core::fuzzy::matcher();

    let header = TableRow::new(["IP", "MAC", "VENDOR", "HOST", "SERVICES"])
        .style(Style::default().fg(dim).add_modifier(Modifier::BOLD));

    let rows: Vec<TableRow> = state
        .results
        .iter()
        .map(|r| {
            let d = &r.device;
            let quiet = now.duration_since(r.last_seen).as_secs() > WARN_SECS;
            let base = if quiet {
                Style::default().fg(dim)
            } else {
                Style::default()
            };
            let texts = [
                d.ip.to_string(),
                d.mac.clone().unwrap_or_else(|| "-".into()),
                d.vendor.clone().unwrap_or_else(|| "?".into()),
                d.hostname.clone().unwrap_or_else(|| "-".into()),
                d.services.join(", "),
            ];
            let cells: Vec<Cell> = texts
                .iter()
                .map(|t| {
                    Cell::from(highlight_cell(
                        t,
                        pattern.as_ref(),
                        &mut matcher,
                        base,
                        matched_style,
                    ))
                })
                .collect();
            TableRow::new(cells)
        })
        .collect();

    let widths = [
        Constraint::Length(15),
        Constraint::Length(17),
        Constraint::Fill(2),
        Constraint::Fill(2),
        Constraint::Fill(3),
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

/// Build a cell `Line`, colouring the query's matched characters in
/// `matched_style` (the rest in `base`). When the whole query matches *this*
/// cell, its hit positions light up — so a single-term search (`hue`, `192`,
/// `ubiquiti`) highlights wherever it lands. Same `Pattern::indices` mechanism
/// the other tools use.
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

/// Horizontal detail strip (bottom): everything Heimdall knows about the
/// selected device. Knowledge at the locus of attention.
fn render_detail(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());
    let block = Block::default().borders(Borders::TOP).border_style(dim);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(r) = state.selected() else {
        return;
    };
    let d = &r.device;
    let age = Instant::now().duration_since(r.last_seen).as_secs();
    let ports = if d.open_ports.is_empty() {
        "—".to_string()
    } else {
        d.open_ports
            .iter()
            .map(|p| format!(":{p}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let services = if d.services.is_empty() {
        "—".to_string()
    } else {
        d.services.join(", ")
    };
    let pair = |k: &str, v: String| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{k} "), dim),
            Span::raw(sanitize_display(&v)),
        ]
    };
    let mut l1 = pair("ip", d.ip.to_string());
    l1.push(Span::styled("   mac ", dim));
    l1.push(Span::raw(d.mac.clone().unwrap_or_else(|| "—".into())));
    l1.push(Span::styled("   vendor ", dim));
    l1.push(Span::raw(sanitize_display(
        d.vendor.as_deref().unwrap_or("?"),
    )));
    let mut l2 = pair("host", d.hostname.clone().unwrap_or_else(|| "—".into()));
    l2.push(Span::styled("   services ", dim));
    l2.push(Span::raw(sanitize_display(&services)));
    let mut l3 = pair("ports", ports);
    l3.push(Span::styled("   seen ", dim));
    l3.push(Span::raw(format!("{age}s ago")));

    f.render_widget(
        Paragraph::new(vec![Line::from(l1), Line::from(l2), Line::from(l3)]),
        inner,
    );
}

/// Upsert all currently-known devices (those with a MAC) into the cache.
fn flush_cache(cache: &Option<Connection>, devices: &BTreeMap<Ipv4Addr, Entry>) {
    let Some(c) = cache else {
        return;
    };
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let devs: Vec<(&Device, i64)> = devices.values().map(|e| (&e.device, now_unix)).collect();
    cache::save(c, &devs);
}
