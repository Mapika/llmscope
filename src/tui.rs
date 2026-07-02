use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, Table, TableState, Widget};

use crate::diff;
use crate::protocol::Provider;
use crate::proxy::DiffPayload;
use crate::record::RequestRecord;

const POLL_INTERVAL: Duration = Duration::from_millis(300);

const ACCENT: Color = Color::Rgb(34, 211, 238);
const DIM: Color = Color::Rgb(90, 96, 112);
const BORDER: Color = Color::Rgb(55, 60, 75);
const ZEBRA: Color = Color::Rgb(20, 22, 28);
const ANTHROPIC: Color = Color::Rgb(217, 119, 87);
const OPENAI: Color = Color::Rgb(16, 163, 127);

/// Vertical gradient for the tokens/s area graph, bottom → top.
const TOKENS_STOPS: &[(u8, u8, u8)] = &[
    (16, 185, 129),
    (45, 212, 191),
    (34, 211, 238),
    (129, 140, 248),
    (192, 132, 252),
];
/// Value gradient for TTFT bars: fast → slow.
const TTFT_STOPS: &[(u8, u8, u8)] = &[(74, 222, 128), (250, 204, 21), (248, 113, 113)];
/// Cache meter fill, left → right (a full bar ends green).
const CACHE_STOPS: &[(u8, u8, u8)] = &[(248, 113, 113), (250, 204, 21), (74, 222, 128)];
/// Stable per-session accents, assigned in order of first appearance.
const SESSION_COLORS: [Color; 6] = [
    Color::Rgb(34, 211, 238),
    Color::Rgb(167, 139, 250),
    Color::Rgb(52, 211, 153),
    Color::Rgb(251, 191, 36),
    Color::Rgb(244, 114, 182),
    Color::Rgb(56, 189, 248),
];
/// Context-growth area graph, bottom → top: gold into warning red.
const CONTEXT_STOPS: &[(u8, u8, u8)] = &[(202, 138, 4), (245, 158, 11), (248, 113, 113)];

fn gradient(stops: &[(u8, u8, u8)], t: f32) -> Color {
    let last = stops.len() - 1;
    if last == 0 {
        let (r, g, b) = stops[0];
        return Color::Rgb(r, g, b);
    }
    let scaled = t.clamp(0.0, 1.0) * last as f32;
    let i = (scaled.floor() as usize).min(last - 1);
    let f = scaled - i as f32;
    let (ar, ag, ab) = stops[i];
    let (br, bg, bb) = stops[i + 1];
    Color::Rgb(
        (ar as f32 + (br as f32 - ar as f32) * f) as u8,
        (ag as f32 + (bg as f32 - ag as f32) * f) as u8,
        (ab as f32 + (bb as f32 - ab as f32) * f) as u8,
    )
}

/// Filled braille area graph, btop-style: 2×4 dots per cell, one value per
/// dot column (newest right-aligned), vertical color gradient by row.
struct AreaGraph<'a> {
    data: &'a [u64],
    stops: &'a [(u8, u8, u8)],
    /// Draw a one-dot zero line across the full width, btop-style, so the
    /// graph reads as a live signal even when idle.
    baseline: bool,
}

impl Widget for AreaGraph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let dot_cols = area.width as usize * 2;
        let dot_rows = area.height as usize * 4;
        let max = self.data.iter().copied().max().unwrap_or(0).max(1);

        let floor = if self.baseline { 1 } else { 0 };
        let mut heights = vec![floor; dot_cols];
        let n = self.data.len().min(dot_cols);
        for (i, v) in self.data[self.data.len() - n..].iter().enumerate() {
            heights[dot_cols - n + i] = if *v > 0 {
                (((*v as f64 / max as f64) * dot_rows as f64).round() as usize).max(1)
            } else {
                floor
            };
        }

        // Braille dot bits: rows 0-3 of the left and right dot column.
        const LEFT: [u16; 4] = [0x01, 0x02, 0x04, 0x40];
        const RIGHT: [u16; 4] = [0x08, 0x10, 0x20, 0x80];

        for cy in 0..area.height {
            let t = 1.0 - cy as f32 / area.height.max(1) as f32;
            let color = gradient(self.stops, t);
            for cx in 0..area.width {
                let mut bits: u16 = 0;
                for dr in 0..4 {
                    let global_row = cy as usize * 4 + dr;
                    for (dc, col_bits) in [(0, LEFT), (1, RIGHT)] {
                        if dot_rows - global_row <= heights[cx as usize * 2 + dc] {
                            bits |= col_bits[dr];
                        }
                    }
                }
                if bits != 0 {
                    let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                    buf[(area.x + cx, area.y + cy)].set_char(ch).set_fg(color);
                }
            }
        }
    }
}

/// btop-style meter: filled cells colored by a position gradient.
fn meter_spans(frac: f64, segments: usize, stops: &[(u8, u8, u8)]) -> Vec<Span<'static>> {
    let mut filled = (frac.clamp(0.0, 1.0) * segments as f64).round() as usize;
    if filled == 0 && frac > 0.001 {
        filled = 1; // anything non-negligible shows at least one cell
    }
    (0..segments)
        .map(|i| {
            if i < filled {
                let t = i as f32 / (segments - 1).max(1) as f32;
                Span::styled("█", Style::new().fg(gradient(stops, t)))
            } else {
                Span::styled("░", Style::new().fg(BORDER))
            }
        })
        .collect()
}

fn cache_meter(pct: f64, segments: usize) -> Vec<Span<'static>> {
    let mut spans = meter_spans(pct / 100.0, segments, CACHE_STOPS);
    spans.push(Span::styled(
        format!(" {pct:>3.0}%"),
        Style::new().fg(if pct >= 50.0 {
            Color::Rgb(74, 222, 128)
        } else {
            Color::Rgb(250, 204, 21)
        }),
    ));
    spans
}

enum View {
    Dashboard,
    Diff(DiffScreen),
}

struct DiffScreen {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
}

struct App {
    port: u16,
    records: VecDeque<RequestRecord>,
    last_id: i64,
    connected: bool,
    /// Index into `records` (0 = newest) of the highlighted table row.
    selected: usize,
    table: TableState,
    view: View,
}

pub async fn run(port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()?;
    let base = format!("http://127.0.0.1:{port}");

    let mut app = App {
        port,
        records: VecDeque::new(),
        last_id: 0,
        connected: false,
        selected: 0,
        table: TableState::default(),
        view: View::Dashboard,
    };

    let mut terminal = ratatui::init();
    let mut last_poll = Instant::now() - POLL_INTERVAL;

    let result = loop {
        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            match fetch(&client, &base, app.last_id).await {
                Ok(new) => {
                    app.connected = true;
                    let arrived = new.len();
                    for r in new {
                        app.last_id = app.last_id.max(r.id);
                        app.records.push_front(r);
                    }
                    app.records.truncate(5000);
                    // Keep the highlight on the same record as new rows push
                    // everything down; at the top it follows the newest.
                    if arrived > 0 && app.selected > 0 {
                        app.selected =
                            (app.selected + arrived).min(app.records.len().saturating_sub(1));
                    }
                }
                Err(_) => app.connected = false,
            }
        }

        if let Err(e) = terminal.draw(|f| draw(f, &mut app)) {
            break Err(e.into());
        }

        // Blocking poll with a short timeout keeps the loop at ~20fps without
        // needing crossterm's async event stream.
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()? {
                // Windows delivers both press and release events.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl_c = key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl_c {
                    break Ok(());
                }
                match &mut app.view {
                    View::Dashboard => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.selected = app.selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.selected =
                                (app.selected + 1).min(app.records.len().saturating_sub(1));
                        }
                        KeyCode::Enter | KeyCode::Char('d') | KeyCode::Char('b') => {
                            let body_view = key.code == KeyCode::Char('b');
                            if let Some(rec) = app.records.get(app.selected) {
                                let screen = match fetch_diff(&client, &base, rec.id).await {
                                    Ok(payload) if body_view => build_body_screen(&payload),
                                    Ok(payload) => build_diff_screen(&payload),
                                    Err(e) => DiffScreen {
                                        title: format!("#{}", rec.id),
                                        lines: vec![Line::from(Span::styled(
                                            format!("could not load: {e}"),
                                            Style::new().fg(Color::Rgb(248, 113, 113)),
                                        ))],
                                        scroll: 0,
                                    },
                                };
                                app.view = View::Diff(screen);
                            }
                        }
                        _ => {}
                    },
                    View::Diff(screen) => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => {
                            app.view = View::Dashboard;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            screen.scroll = screen.scroll.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            screen.scroll = screen
                                .scroll
                                .saturating_add(1)
                                .min(screen.lines.len() as u16);
                        }
                        KeyCode::PageUp => screen.scroll = screen.scroll.saturating_sub(10),
                        KeyCode::PageDown => {
                            screen.scroll = screen
                                .scroll
                                .saturating_add(10)
                                .min(screen.lines.len() as u16);
                        }
                        _ => {}
                    },
                }
            }
    };

    ratatui::restore();
    result
}

async fn fetch_diff(client: &reqwest::Client, base: &str, id: i64) -> Result<DiffPayload> {
    let url = format!("{base}/_llmscope/diff/{id}");
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.json().await?)
}

async fn fetch(client: &reqwest::Client, base: &str, since: i64) -> Result<Vec<RequestRecord>> {
    let url = format!("{base}/_llmscope/requests?since={since}&limit=2000");
    Ok(client.get(url).send().await?.json().await?)
}

fn panel(title: Vec<Span<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(title))
}

fn draw(f: &mut Frame, app: &mut App) {
    if let View::Diff(screen) = &app.view {
        draw_diff(f, screen);
        return;
    }

    // Mission-control grid. The second panel row folds away on short
    // terminals; the sidebar folds away on narrow ones.
    let dense = f.area().height >= 34;
    let wide = f.area().width >= 110;
    let labels = session_labels(&app.records);

    let mut constraints = vec![Constraint::Length(3), Constraint::Length(9)];
    if dense {
        constraints.push(Constraint::Length(8));
    }
    constraints.extend([Constraint::Min(4), Constraint::Length(1)]);
    let chunks = Layout::vertical(constraints).split(f.area());
    let (header, graphs) = (chunks[0], chunks[1]);
    let (second, main, footer) = if dense {
        (Some(chunks[2]), chunks[3], chunks[4])
    } else {
        (None, chunks[2], chunks[3])
    };

    draw_header(f, app, header);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(graphs);
    draw_tokens_per_sec(f, app, left);
    draw_ttft(f, app, right);
    if let Some(second) = second {
        let [ctx, sessions] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(second);
        draw_context_growth(f, app, &labels, ctx);
        draw_sessions(f, app, &labels, sessions);
    }
    if wide {
        let [table, sidebar] =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(42)]).areas(main);
        let [models, latency] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(sidebar);
        draw_table(f, app, &labels, table);
        draw_models(f, app, models);
        draw_latency(f, app, latency);
    } else {
        draw_table(f, app, &labels, main);
    }

    f.render_widget(
        Line::from(vec![
            Span::styled("  q", Style::new().fg(ACCENT).bold()),
            Span::styled(" quit", Style::new().fg(DIM)),
            Span::styled("   ↑↓", Style::new().fg(ACCENT).bold()),
            Span::styled(" select", Style::new().fg(DIM)),
            Span::styled("   ⏎", Style::new().fg(ACCENT).bold()),
            Span::styled(" turn diff", Style::new().fg(DIM)),
            Span::styled("   b", Style::new().fg(ACCENT).bold()),
            Span::styled(" body", Style::new().fg(DIM)),
            Span::styled(
                format!("   proxy 127.0.0.1:{}", app.port),
                Style::new().fg(DIM),
            ),
        ]),
        footer,
    );
}

fn draw_diff(f: &mut Frame, screen: &DiffScreen) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(f.area());

    let block = panel(vec![
        Span::styled(" turn diff ", Style::new().fg(ACCENT).bold()),
        Span::styled(format!("{} ", screen.title), Style::new().fg(DIM)),
    ]);
    let inner_h = main.height.saturating_sub(2);
    let max_scroll = (screen.lines.len() as u16).saturating_sub(inner_h);
    f.render_widget(
        Paragraph::new(screen.lines.clone())
            .block(block)
            .scroll((screen.scroll.min(max_scroll), 0)),
        main,
    );
    f.render_widget(
        Line::from(vec![
            Span::styled("  esc", Style::new().fg(ACCENT).bold()),
            Span::styled(" back", Style::new().fg(DIM)),
            Span::styled("   ↑↓", Style::new().fg(ACCENT).bold()),
            Span::styled(" scroll", Style::new().fg(DIM)),
        ]),
        footer,
    );
}

fn provider_of(r: &RequestRecord) -> Provider {
    Provider::from_name(&r.provider)
}

fn msg_line(marker: char, m: &diff::Msg, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {marker} "), Style::new().fg(color).bold()),
        Span::styled(format!("{:<10}", m.role), Style::new().fg(color)),
        Span::styled(format!("{:<16}", m.kind), Style::new().fg(DIM)),
        Span::styled(
            format!("{:>8}  ", format!("{} ch", fmt_tokens(m.chars as i64))),
            Style::new().fg(DIM),
        ),
        Span::styled(m.preview.clone(), Style::new().fg(Color::Rgb(150, 156, 170))),
    ])
}

fn stat_line(label: &str, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::styled(
        format!(" {label:<9}"),
        Style::new().fg(DIM).bold(),
    )];
    all.extend(spans);
    Line::from(all)
}

/// Raw request JSON (pretty-printed) and captured response, scrollable.
fn build_body_screen(p: &DiffPayload) -> DiffScreen {
    const MAX_LINES: usize = 4_000;
    let title = format!("#{} · {} · bodies", p.curr.id, p.curr.model);
    let section = |name: &str| {
        Line::from(Span::styled(
            format!(" ── {name} "),
            Style::new().fg(ACCENT).bold(),
        ))
    };
    let mut lines: Vec<Line<'static>> = vec![Line::default(), section("request")];

    let pretty = serde_json::from_str::<serde_json::Value>(&p.curr_body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| p.curr_body.clone());
    for l in pretty.lines().take(MAX_LINES) {
        lines.push(Line::from(Span::styled(
            format!(" {l}"),
            Style::new().fg(Color::Rgb(150, 156, 170)),
        )));
    }

    lines.push(Line::default());
    lines.push(section("response"));
    let remaining = MAX_LINES.saturating_sub(lines.len());
    for l in p.curr_response_body.lines().take(remaining) {
        lines.push(Line::from(Span::styled(
            format!(" {l}"),
            Style::new().fg(Color::Rgb(150, 156, 170)),
        )));
    }
    if pretty.lines().count() + p.curr_response_body.lines().count() > MAX_LINES {
        lines.push(Line::from(Span::styled(
            " … truncated",
            Style::new().fg(DIM),
        )));
    }
    DiffScreen { title, lines, scroll: 0 }
}

fn build_diff_screen(p: &DiffPayload) -> DiffScreen {
    const GREEN: Color = Color::Rgb(74, 222, 128);
    const RED: Color = Color::Rgb(248, 113, 113);
    const YELLOW: Color = Color::Rgb(250, 204, 21);

    let title = match &p.prev {
        Some(prev) => format!("#{} ⇐ #{} · {}", p.curr.id, prev.id, p.curr.model),
        None => format!("#{} · {}", p.curr.id, p.curr.model),
    };
    let mut lines: Vec<Line<'static>> = vec![Line::default()];

    let Some(curr) = diff::parse_convo(provider_of(&p.curr), &p.curr_body) else {
        lines.push(Line::from(Span::styled(
            " this request has no conversation payload to diff",
            Style::new().fg(DIM),
        )));
        return DiffScreen { title, lines, scroll: 0 };
    };
    let prev_convo = p
        .prev_body
        .as_deref()
        .and_then(|b| diff::parse_convo(provider_of(&p.curr), b));

    let changed = |is_changed: bool| -> Span<'static> {
        if is_changed {
            Span::styled("CHANGED", Style::new().fg(YELLOW).bold())
        } else {
            Span::styled("unchanged", Style::new().fg(DIM))
        }
    };

    match prev_convo {
        None => {
            lines.push(Line::from(Span::styled(
                " first captured request of this kind — showing composition",
                Style::new().fg(YELLOW),
            )));
            lines.push(Line::default());
            lines.push(stat_line(
                "system",
                vec![Span::raw(format!("{} chars", fmt_tokens(curr.system_chars as i64)))],
            ));
            lines.push(stat_line(
                "tools",
                vec![Span::raw(format!(
                    "{} tools · {} chars",
                    curr.tools_count,
                    fmt_tokens(curr.tools_chars as i64)
                ))],
            ));
            lines.push(stat_line(
                "messages",
                vec![Span::raw(format!("{}", curr.messages.len()))],
            ));
            lines.push(Line::default());
            for m in &curr.messages {
                lines.push(msg_line(' ', m, Color::Rgb(150, 156, 170)));
            }
        }
        Some(prevc) => {
            let d = diff::diff(&prevc, &curr);

            lines.push(stat_line(
                "system",
                vec![
                    Span::raw(format!("{} chars · ", fmt_tokens(curr.system_chars as i64))),
                    changed(d.system_changed),
                ],
            ));
            lines.push(stat_line(
                "tools",
                vec![
                    Span::raw(format!(
                        "{} tools · {} chars · ",
                        curr.tools_count,
                        fmt_tokens(curr.tools_chars as i64)
                    )),
                    changed(d.tools_changed),
                ],
            ));
            lines.push(stat_line(
                "messages",
                vec![
                    Span::raw(format!("{} kept · ", d.kept)),
                    Span::styled(
                        format!("{} appended", d.appended.len()),
                        Style::new().fg(GREEN),
                    ),
                    Span::raw(" · "),
                    Span::styled(
                        format!("{} dropped", d.dropped.len()),
                        Style::new().fg(if d.dropped.is_empty() { DIM } else { RED }),
                    ),
                ],
            ));

            // The economics line: what the agent re-sent vs. what the
            // provider says it served from cache.
            let resent_chars = curr.system_chars + curr.tools_chars + d.kept_chars;
            let est_resent_tok = (resent_chars / 4) as i64;
            let reported = p.curr.cache_read_tokens;
            if est_resent_tok > 0 {
                let ratio = reported as f64 / est_resent_tok as f64;
                let miss = reported == 0 && est_resent_tok > 1_000;
                let partial = reported > 0 && ratio < 0.7;
                let verdict = if miss {
                    Span::styled(
                        "✗ no cache reads — full re-send billed",
                        Style::new().fg(RED).bold(),
                    )
                } else if ratio >= 0.7 {
                    Span::styled("✓ cache effective", Style::new().fg(GREEN))
                } else {
                    Span::styled("⚠ partial cache", Style::new().fg(YELLOW))
                };
                lines.push(stat_line(
                    "context",
                    vec![
                        Span::raw(format!(
                            "≈{} tok re-sent · API reports {} from cache · ",
                            fmt_tokens(est_resent_tok),
                            fmt_tokens(reported)
                        )),
                        verdict,
                    ],
                ));

                // The miss diagnosis: name the culprit, don't just meter it.
                if miss || partial {
                    let anthropic = provider_of(&p.curr) == Provider::Anthropic;
                    let gap_ms = p.prev.as_ref().map_or(0, |pr| p.curr.ts_ms - pr.ts_ms);
                    let causes = diff::diagnose_miss(&prevc, &curr, &d, anthropic, gap_ms);
                    for (i, cause) in causes.iter().enumerate() {
                        let label = if i == 0 { "why" } else { "" };
                        match cause {
                            diff::MissCause::NoCacheControl => lines.push(stat_line(
                                label,
                                vec![Span::styled(
                                    "no cache_control breakpoints in the request — explicit caching is off",
                                    Style::new().fg(YELLOW),
                                )],
                            )),
                            diff::MissCause::SystemChanged {
                                at_char,
                                prev_snippet,
                                curr_snippet,
                            } => {
                                lines.push(stat_line(
                                    label,
                                    vec![Span::styled(
                                        format!("system prompt changed at char {at_char}"),
                                        Style::new().fg(YELLOW),
                                    )],
                                ));
                                lines.push(Line::from(vec![
                                    Span::styled("             prev  ", Style::new().fg(DIM)),
                                    Span::styled(prev_snippet.clone(), Style::new().fg(RED)),
                                ]));
                                lines.push(Line::from(vec![
                                    Span::styled("             curr  ", Style::new().fg(DIM)),
                                    Span::styled(curr_snippet.clone(), Style::new().fg(GREEN)),
                                ]));
                            }
                            diff::MissCause::ToolsChanged { detail } => lines.push(stat_line(
                                label,
                                vec![Span::styled(
                                    format!("tool definitions changed — {detail}"),
                                    Style::new().fg(YELLOW),
                                )],
                            )),
                            diff::MissCause::HistoryRewritten { at_msg } => lines.push(stat_line(
                                label,
                                vec![Span::styled(
                                    format!(
                                        "history rewritten at message {at_msg} — cacheable prefix broken (compaction?)"
                                    ),
                                    Style::new().fg(YELLOW),
                                )],
                            )),
                            diff::MissCause::TtlExpired { gap_secs } => {
                                let gap = if *gap_secs >= 60 {
                                    format!("{}m", gap_secs / 60)
                                } else {
                                    format!("{gap_secs}s")
                                };
                                lines.push(stat_line(
                                    label,
                                    vec![Span::styled(
                                        format!(
                                            "prefix unchanged, but the previous turn was {gap} ago — cache TTL (~5 min) expired"
                                        ),
                                        Style::new().fg(YELLOW),
                                    )],
                                ));
                            }
                        }
                    }
                    if causes.is_empty() {
                        lines.push(stat_line(
                            "why",
                            vec![Span::styled(
                                "prefix unchanged — miss looks provider-side (cache eviction)",
                                Style::new().fg(DIM),
                            )],
                        ));
                    }
                }
            }

            lines.push(Line::default());
            if d.kept > 0 {
                lines.push(Line::from(Span::styled(
                    format!(
                        " = {} messages kept · {} chars",
                        d.kept,
                        fmt_tokens(d.kept_chars as i64)
                    ),
                    Style::new().fg(DIM),
                )));
            }
            for m in &d.dropped {
                lines.push(msg_line('-', m, RED));
            }
            for m in &d.appended {
                lines.push(msg_line('+', m, GREEN));
            }
        }
    }

    DiffScreen { title, lines, scroll: 0 }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (dot, dot_color) = if app.connected {
        ("●", Color::Rgb(74, 222, 128))
    } else {
        ("○", Color::Rgb(248, 113, 113))
    };

    let reqs = app.records.len();
    let input: i64 = app.records.iter().map(|r| r.input_tokens).sum();
    let read: i64 = app.records.iter().map(|r| r.cache_read_tokens).sum();
    let write: i64 = app.records.iter().map(|r| r.cache_write_tokens).sum();
    let out: i64 = app.records.iter().map(|r| r.output_tokens).sum();
    let spend: f64 = app.records.iter().map(|r| r.cost_usd).sum();
    let total_in = input + read + write;
    let hit = if total_in > 0 {
        100.0 * read as f64 / total_in as f64
    } else {
        0.0
    };

    let sep = Span::styled("  │  ", Style::new().fg(BORDER));
    let stats = if app.connected || reqs > 0 {
        let mut spans = vec![
            Span::styled("req ", Style::new().fg(DIM)),
            Span::raw(reqs.to_string()).bold(),
            sep.clone(),
            Span::styled("in ", Style::new().fg(DIM)),
            Span::raw(fmt_tokens(total_in)).bold(),
            sep.clone(),
            Span::styled("out ", Style::new().fg(DIM)),
            Span::raw(fmt_tokens(out)).bold(),
            sep.clone(),
            Span::styled("spend ", Style::new().fg(DIM)),
            Span::styled(
                format!("${spend:.2}"),
                Style::new().fg(Color::Rgb(74, 222, 128)).bold(),
            ),
            sep.clone(),
            Span::styled("cache ", Style::new().fg(DIM)),
        ];
        spans.extend(cache_meter(hit, 12));
        Line::from(spans)
    } else {
        Line::from(Span::styled(
            format!(
                "waiting for proxy on :{} — start `llmscope run -- <your agent>`",
                app.port
            ),
            Style::new().fg(Color::Rgb(250, 204, 21)),
        ))
    };

    let block = panel(vec![
        Span::styled(" llmscope ", Style::new().fg(ACCENT).bold()),
        Span::styled(dot, Style::new().fg(dot_color)),
        Span::styled(format!(" :{} ", app.port), Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(stats).block(block), area);
}

/// Output tokens per second, one value per braille dot column, with each
/// request's tokens spread evenly across the seconds it was generating.
fn draw_tokens_per_sec(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let dot_cols = inner_w * 2;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let window_start = now_ms - dot_cols as i64 * 1000;

    let mut buckets = vec![0f64; dot_cols];
    for r in &app.records {
        let end = r.ts_ms + r.duration_ms;
        if end < window_start || r.output_tokens == 0 {
            continue;
        }
        let dur_s = (r.duration_ms as f64 / 1000.0).max(0.001);
        let rate = r.output_tokens as f64 / dur_s;
        let mut t = r.ts_ms.max(window_start);
        while t < end.min(now_ms) {
            let idx = ((t - window_start) / 1000) as usize;
            let slice_end = ((t / 1000) + 1) * 1000;
            if idx < dot_cols {
                let covered = (slice_end.min(end) - t) as f64 / 1000.0;
                buckets[idx] += rate * covered;
            }
            t = slice_end;
        }
    }
    // Two light smoothing passes soften the hard edges of per-request
    // rectangles without materially distorting the rates.
    for _ in 0..2 {
        let src = buckets.clone();
        for i in 0..src.len() {
            let prev = if i > 0 { src[i - 1] } else { src[i] };
            let next = if i + 1 < src.len() { src[i + 1] } else { src[i] };
            buckets[i] = 0.25 * prev + 0.5 * src[i] + 0.25 * next;
        }
    }
    let data: Vec<u64> = buckets.iter().map(|v| v.round() as u64).collect();
    let peak = data.iter().max().copied().unwrap_or(0);

    let block = panel(vec![
        Span::styled(" tokens/s ", Style::new().bold()),
        Span::styled(format!("peak {peak} "), Style::new().fg(DIM)),
    ]);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        AreaGraph {
            data: &data,
            stops: TOKENS_STOPS,
            baseline: true,
        },
        inner,
    );
}

/// TTFT per request as a braille area graph, one value per turn stretched
/// across the panel. Vertical green→red gradient: slow spikes get red tips.
fn draw_ttft(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let mut vals: Vec<u64> = app
        .records
        .iter()
        .filter(|r| r.ttft_ms >= 0)
        .take(inner_w * 2)
        .map(|r| r.ttft_ms as u64)
        .collect();
    vals.reverse();
    let avg = if vals.is_empty() {
        0
    } else {
        vals.iter().sum::<u64>() / vals.len() as u64
    };

    let dot_cols = inner_w * 2;
    let data: Vec<u64> = if vals.is_empty() {
        Vec::new()
    } else {
        (0..dot_cols)
            .map(|i| vals[i * vals.len() / dot_cols])
            .collect()
    };

    let block = panel(vec![
        Span::styled(" time-to-first-token ", Style::new().bold()),
        Span::styled(format!("avg {} ", fmt_ms(avg as i64)), Style::new().fg(DIM)),
    ]);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        AreaGraph {
            data: &data,
            stops: TTFT_STOPS,
            baseline: true,
        },
        inner,
    );
}

type SessionLabels = std::collections::HashMap<String, (String, Color)>;

/// s1, s2, … in order of first appearance, each with a stable accent color.
fn session_labels(records: &VecDeque<RequestRecord>) -> SessionLabels {
    let mut map = SessionLabels::new();
    for r in records.iter().rev() {
        if r.session.is_empty() || map.contains_key(&r.session) {
            continue;
        }
        let idx = map.len();
        map.insert(
            r.session.clone(),
            (format!("s{}", idx + 1), SESSION_COLORS[idx % SESSION_COLORS.len()]),
        );
    }
    map
}

fn session_tag(labels: &SessionLabels, session: &str) -> (String, Color) {
    labels
        .get(session)
        .cloned()
        .unwrap_or_else(|| ("—".to_string(), DIM))
}

/// Total input (billed + cached + written) per turn of ONE conversation —
/// the selected row's session — so interleaved agents don't muddy the shape.
fn draw_context_growth(f: &mut Frame, app: &App, labels: &SessionLabels, area: Rect) {
    let selected_session = app
        .records
        .get(app.selected)
        .map(|r| r.session.clone())
        .unwrap_or_default();
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let mut vals: Vec<u64> = app
        .records
        .iter()
        .filter(|r| selected_session.is_empty() || r.session == selected_session)
        .take(inner_w * 2)
        .map(|r| (r.input_tokens + r.cache_read_tokens + r.cache_write_tokens).max(0) as u64)
        .collect();
    vals.reverse();
    let latest = vals.last().copied().unwrap_or(0);
    // Index-based chart, not a time series: stretch the turns across the full
    // panel width (and downsample once there are more turns than dot columns).
    let dot_cols = inner_w * 2;
    let data: Vec<u64> = if vals.is_empty() {
        Vec::new()
    } else {
        (0..dot_cols)
            .map(|i| vals[i * vals.len() / dot_cols])
            .collect()
    };

    let (tag, tag_color) = session_tag(labels, &selected_session);
    let block = panel(vec![
        Span::styled(" context per turn ", Style::new().bold()),
        Span::styled(format!("▍{tag} "), Style::new().fg(tag_color).bold()),
        Span::styled(format!("now {} tok ", fmt_tokens(latest as i64)), Style::new().fg(DIM)),
    ]);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        AreaGraph {
            data: &data,
            stops: CONTEXT_STOPS,
            baseline: true,
        },
        inner,
    );
}

/// One row per agent conversation, newest first — btop's process list.
fn draw_sessions(f: &mut Frame, app: &App, labels: &SessionLabels, area: Rect) {
    struct Agg {
        last_id: i64,
        model: String,
        reqs: usize,
        total_in: i64,
        out: i64,
        cache_read: i64,
        spend: f64,
    }
    let mut agg: std::collections::HashMap<&str, Agg> = std::collections::HashMap::new();
    for r in &app.records {
        if r.session.is_empty() {
            continue;
        }
        let e = agg.entry(r.session.as_str()).or_insert(Agg {
            last_id: r.id,
            model: r.model.clone(),
            reqs: 0,
            total_in: 0,
            out: 0,
            cache_read: 0,
            spend: 0.0,
        });
        e.last_id = e.last_id.max(r.id);
        e.reqs += 1;
        e.total_in += r.input_tokens + r.cache_read_tokens + r.cache_write_tokens;
        e.out += r.output_tokens;
        e.cache_read += r.cache_read_tokens;
        e.spend += r.cost_usd;
    }
    let mut sessions: Vec<(&str, Agg)> = agg.into_iter().collect();
    sessions.sort_by_key(|(_, a)| std::cmp::Reverse(a.last_id));

    let visible = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = sessions
        .iter()
        .take(visible)
        .map(|(key, a)| {
            let (tag, color) = session_tag(labels, key);
            let model: String = a
                .model
                .rsplit('/')
                .next()
                .unwrap_or(&a.model)
                .chars()
                .take(17)
                .collect();
            let hit = if a.total_in > 0 {
                a.cache_read as f64 / a.total_in as f64
            } else {
                0.0
            };
            let mut spans = vec![
                Span::styled(format!(" ▍{tag:<3}"), Style::new().fg(color).bold()),
                Span::styled(format!("{model:<18}"), Style::new().fg(Color::Rgb(150, 156, 170))),
                Span::styled(format!("{:>3}r ", a.reqs), Style::new().fg(DIM)),
                Span::raw(format!("{:>7} ", fmt_tokens(a.total_in))),
                Span::styled(
                    format!("{:>7} ", fmt_cost(a.spend)),
                    Style::new().fg(Color::Rgb(74, 222, 128)),
                ),
            ];
            spans.extend(meter_spans(hit, 5, CACHE_STOPS));
            spans.push(Span::styled(
                format!(" {:>3.0}%", hit * 100.0),
                Style::new().fg(DIM),
            ));
            Line::from(spans)
        })
        .collect();

    let block = panel(vec![
        Span::styled(" sessions ", Style::new().bold()),
        Span::styled(format!("{} live ", sessions.len()), Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Latency and health summary for the sidebar.
fn draw_latency(f: &mut Frame, app: &App, area: Rect) {
    let mut ttfts: Vec<i64> = app
        .records
        .iter()
        .filter(|r| r.ttft_ms >= 0)
        .map(|r| r.ttft_ms)
        .collect();
    ttfts.sort_unstable();
    let avg_ttft = if ttfts.is_empty() {
        0
    } else {
        ttfts.iter().sum::<i64>() / ttfts.len() as i64
    };
    let p95_ttft = ttfts
        .get((ttfts.len().saturating_sub(1)) * 95 / 100)
        .copied()
        .unwrap_or(0);

    let mut rates: Vec<f64> = app
        .records
        .iter()
        .filter(|r| r.output_tokens > 0 && r.duration_ms > r.ttft_ms.max(0))
        .map(|r| r.output_tokens as f64 / ((r.duration_ms - r.ttft_ms.max(0)) as f64 / 1000.0))
        .collect();
    rates.sort_unstable_by(|a, b| a.total_cmp(b));
    let avg_rate = if rates.is_empty() {
        0.0
    } else {
        rates.iter().sum::<f64>() / rates.len() as f64
    };

    let errors = app.records.iter().filter(|r| r.status >= 400 || r.status == 0).count();
    let estimated = app.records.iter().filter(|r| r.estimated).count();

    // Cache economics: what caching saved, and what cold re-sends wasted.
    let mut seen_sessions: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let (mut saved, mut wasted) = (0.0f64, 0.0f64);
    for r in app.records.iter().rev() {
        let had_prior = !r.session.is_empty() && !seen_sessions.insert(r.session.as_str());
        let (s, w) = crate::record::cache_economics(r, had_prior);
        saved += s;
        wasted += w;
    }

    let value = |s: String| Span::styled(s, Style::new().bold());
    let lines = vec![
        stat_line("ttft", vec![
            value(fmt_ms(avg_ttft)),
            Span::styled(" avg · ", Style::new().fg(DIM)),
            value(fmt_ms(p95_ttft)),
            Span::styled(" p95", Style::new().fg(DIM)),
        ]),
        stat_line("gen", vec![
            value(format!("{avg_rate:.0} tok/s")),
            Span::styled(" avg", Style::new().fg(DIM)),
        ]),
        stat_line("errors", vec![Span::styled(
            errors.to_string(),
            if errors > 0 {
                Style::new().fg(Color::Rgb(248, 113, 113)).bold()
            } else {
                Style::new().fg(Color::Rgb(74, 222, 128)).bold()
            },
        )]),
        stat_line("est rows", vec![
            value(estimated.to_string()),
            Span::styled(" (~)", Style::new().fg(DIM)),
        ]),
        stat_line("cache", vec![
            Span::styled(
                format!("saved {}", fmt_cost(saved)),
                Style::new().fg(Color::Rgb(74, 222, 128)).bold(),
            ),
            Span::styled(" · ", Style::new().fg(DIM)),
            Span::styled(
                format!("wasted {}", fmt_cost(wasted)),
                if wasted > 0.01 {
                    Style::new().fg(Color::Rgb(248, 113, 113)).bold()
                } else {
                    Style::new().fg(DIM)
                },
            ),
        ]),
    ];

    let block = panel(vec![Span::styled(" health ", Style::new().bold())]);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Per-model spend, btop per-core style: meter is the share of the most
/// expensive model.
fn draw_models(f: &mut Frame, app: &App, area: Rect) {
    use std::collections::HashMap;

    let mut agg: HashMap<&str, (&str, f64, i64)> = HashMap::new();
    for r in &app.records {
        let e = agg.entry(r.model.as_str()).or_insert((r.provider.as_str(), 0.0, 0));
        e.1 += r.cost_usd;
        e.2 += r.input_tokens + r.cache_read_tokens + r.cache_write_tokens + r.output_tokens;
    }
    let mut models: Vec<(&str, &str, f64, i64)> = agg
        .into_iter()
        .map(|(m, (p, cost, tok))| (m, p, cost, tok))
        .collect();
    models.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.3.cmp(&a.3)));
    let total: f64 = models.iter().map(|m| m.2).sum();
    let max_spend = models.first().map(|m| m.2).unwrap_or(0.0).max(1e-9);

    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let name_w = if inner_w >= 56 { 22usize } else { 17 };
    let value_w = 15usize;
    let meter_w = inner_w.saturating_sub(name_w + value_w).clamp(4, 30);

    let visible = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = models
        .iter()
        .take(visible)
        .map(|(model, provider, cost, tok)| {
            let color = match *provider {
                "anthropic" => ANTHROPIC,
                "openai" => OPENAI,
                _ => ACCENT,
            };
            let name: String = model.chars().take(name_w - 2).collect();
            let mut spans = vec![Span::styled(
                format!(" {name:<width$}", width = name_w - 1),
                Style::new().fg(color),
            )];
            spans.extend(meter_spans(cost / max_spend, meter_w, TOKENS_STOPS));
            spans.push(Span::styled(
                format!(" ${cost:.2}", ),
                Style::new().fg(Color::Rgb(74, 222, 128)),
            ));
            spans.push(Span::styled(
                format!(" · {}", fmt_tokens(*tok)),
                Style::new().fg(DIM),
            ));
            Line::from(spans)
        })
        .collect();

    let block = panel(vec![
        Span::styled(" models ", Style::new().bold()),
        Span::styled(format!("spend ${total:.2} ", ), Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_table(f: &mut Frame, app: &mut App, labels: &SessionLabels, area: Rect) {
    let header = Row::new(
        ["TIME", "SESS", "MODEL", "IN", "CACHE", "OUT", "TTFT", "TOTAL", "COST"]
            .into_iter()
            .map(|h| Cell::from(h).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))),
    );

    let rows = app.records.iter().take(1000).enumerate().map(|(i, r)| {
        let (tag, tag_color) = session_tag(labels, &r.session);
        let time = Local
            .timestamp_millis_opt(r.ts_ms)
            .single()
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_default();
        let total_in = r.input_tokens + r.cache_read_tokens + r.cache_write_tokens;
        let cache_pct = if total_in > 0 {
            format!("{:.0}%", 100.0 * r.cache_read_tokens as f64 / total_in as f64)
        } else {
            "-".to_string()
        };
        let approx = if r.estimated { "~" } else { "" };
        let provider_color = match r.provider.as_str() {
            "anthropic" => ANTHROPIC,
            "openai" => OPENAI,
            _ => ACCENT,
        };
        let (model_style, status_note) = if r.status >= 400 || r.status == 0 {
            (
                Style::new().fg(Color::Rgb(248, 113, 113)),
                format!(" [{}]", r.status),
            )
        } else {
            (Style::new().fg(provider_color), String::new())
        };
        let row = Row::new(vec![
            Cell::from(time).style(Style::new().fg(DIM)),
            Cell::from(format!("▍{tag}")).style(Style::new().fg(tag_color).bold()),
            Cell::from(format!("{}{}", r.model, status_note)).style(model_style),
            Cell::from(format!("{approx}{}", fmt_tokens(total_in))),
            Cell::from(cache_pct),
            Cell::from(format!("{approx}{}", fmt_tokens(r.output_tokens))),
            Cell::from(fmt_ms(r.ttft_ms)),
            Cell::from(fmt_ms(r.duration_ms)),
            Cell::from(fmt_cost(r.cost_usd)).style(Style::new().fg(Color::Rgb(74, 222, 128))),
        ]);
        if i % 2 == 1 {
            row.style(Style::new().bg(ZEBRA))
        } else {
            row
        }
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(4),
            Constraint::Min(18),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .row_highlight_style(Style::new().bg(Color::Rgb(44, 50, 66)))
    .block(panel(vec![Span::styled(" requests ", Style::new().bold())]));

    if app.records.is_empty() {
        app.table.select(None);
    } else {
        app.selected = app.selected.min(app.records.len() - 1);
        app.table.select(Some(app.selected));
    }
    f.render_stateful_widget(table, area, &mut app.table);
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_ms(ms: i64) -> String {
    if ms < 0 {
        "-".to_string()
    } else if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn fmt_cost(c: f64) -> String {
    if c == 0.0 {
        "-".to_string()
    } else if c < 0.01 {
        format!("${c:.4}")
    } else {
        format!("${c:.2}")
    }
}

/// Render one frame of the TUI with synthetic traffic into styled HTML.
/// Powers the hidden `debug-render` subcommand used for screenshots.
pub fn render_demo_html(width: u16, height: u16, view: &str) -> Result<String> {
    use crate::protocol::Usage;
    use crate::record::cost_usd;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut records = VecDeque::new();
    let mut age_ms: i64 = 1_500;
    for i in 0..30u64 {
        // Deterministic pseudo-variety; newest record first. The extra
        // multiply-and-shift mixing keeps values from ramping monotonically.
        let jitter = |salt: u64, span: u64| {
            (((i + 1).wrapping_mul(salt).wrapping_mul(2654435761) >> 13) % span) as i64
        };

        let (provider, model, usage, ttft, dur, session) = if i % 7 == 3 {
            let u = Usage {
                input: 900 + jitter(311, 700),
                output: 60 + jitter(997, 180),
                cache_read: 2_000,
                cache_write: 0,
                ..Usage::default()
            };
            (
                "anthropic",
                "claude-haiku-4-5",
                u,
                150 + jitter(613, 200),
                700 + jitter(431, 900),
                "demo-side-agent",
            )
        } else if i % 9 == 5 {
            let u = Usage {
                input: 800 + jitter(709, 900),
                output: 200 + jitter(499, 400),
                estimated: i % 2 == 1,
                ..Usage::default()
            };
            (
                "openai",
                "openai/gpt-4o-mini",
                u,
                400 + jitter(769, 500),
                2_000 + jitter(577, 2_500),
                "demo-embedder",
            )
        } else {
            let u = Usage {
                input: 700 + jitter(367, 1_500),
                output: 400 + jitter(997, 2_000),
                cache_read: 38_000 + jitter(1499, 55_000),
                cache_write: 200 + jitter(283, 1_800),
                ..Usage::default()
            };
            let out = u.output;
            (
                "anthropic",
                "claude-sonnet-4-5",
                u,
                320 + jitter(613, 600),
                3_000 + out * 7,
                "demo-main-loop",
            )
        };

        let status = if i == 11 { 429 } else { 200 };
        // Overlap requests like an agent running parallel tool calls, so the
        // demo graph shows layered activity instead of isolated rectangles.
        age_ms += dur / 2 + 400 + jitter(35_761, 2_500);
        records.push_back(RequestRecord {
            id: 30 - i as i64,
            ts_ms: now_ms - age_ms,
            provider: provider.to_string(),
            model: model.to_string(),
            path: "/v1/messages".to_string(),
            status,
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: usage.cache_read,
            cache_write_tokens: usage.cache_write,
            ttft_ms: ttft,
            duration_ms: dur,
            cost_usd: cost_usd(model, &usage),
            streamed: true,
            estimated: usage.estimated,
            session: session.to_string(),
        });
    }

    let mut app = App {
        port: 4040,
        records,
        last_id: 30,
        connected: true,
        selected: 0,
        table: TableState::default(),
        view: View::Dashboard,
    };
    if view == "diff" {
        app.view = View::Diff(build_diff_screen(&demo_diff_payload(now_ms)));
    }

    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut term = ratatui::Terminal::new(backend)?;
    term.draw(|f| draw(f, &mut app))?;
    Ok(buffer_to_html(term.backend().buffer()))
}

/// Synthetic agent turn for the diff-view screenshot: prev has six messages,
/// curr appends a tool call and its result — but a timestamp embedded in the
/// system prompt busts the cache, so the shot shows the miss diagnosis.
fn demo_diff_payload(now_ms: i64) -> DiffPayload {
    use serde_json::json;

    let system = |hms: &str| {
        format!(
            "You are an agentic coding assistant working in a Rust repository. \
             Current time: 2026-07-02 09:14:{hms}. {}",
            "Follow the project conventions and keep diffs minimal. ".repeat(260)
        )
    };
    let tools: Vec<_> = (0..24)
        .map(|i| {
            json!({
                "name": format!("tool_{i}"),
                "description": "Executes project operations with structured input. ".repeat(12),
                "input_schema": {"type": "object"}
            })
        })
        .collect();

    let mut messages = vec![json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": "Fix the failing widget tests in llmscope and make the graph smoother",
            "cache_control": {"type": "ephemeral"}
        }]
    })];
    for i in 0..4 {
        messages.push(json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": format!("Running the test suite to inspect failure {i}…")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "bash",
                 "input": {"cmd": "cargo test"}}
            ]
        }));
        messages.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result", "tool_use_id": format!("t{i}"),
                "content": "error[E0063]: missing field `baseline` in initializer of `AreaGraph` — src/tui.rs:590. ".repeat(60)
            }]
        }));
    }
    let prev_body = json!({
        "model": "claude-sonnet-5", "system": system("22"), "tools": tools, "messages": messages
    })
    .to_string();

    messages.push(json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "The widget tests construct AreaGraph without the new baseline field. Patching both test sites."},
            {"type": "tool_use", "id": "t9", "name": "edit", "input": {"file": "src/tui.rs"}}
        ]
    }));
    messages.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result", "tool_use_id": "t9",
            "content": "test result: ok. 4 passed; 0 failed; 0 ignored — ".repeat(25)
        }]
    }));
    let curr_body = json!({
        "model": "claude-sonnet-5", "system": system("41"), "tools": tools, "messages": messages
    })
    .to_string();

    let rec = |id: i64, input: i64, cache_read: i64, cache_write: i64| RequestRecord {
        id,
        ts_ms: now_ms - (48 - id) * 11_000,
        provider: "anthropic".to_string(),
        model: "claude-sonnet-5".to_string(),
        path: "/v1/messages".to_string(),
        status: 200,
        input_tokens: input,
        output_tokens: 1_400,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ttft_ms: 480,
        duration_ms: 9_200,
        cost_usd: 0.048,
        streamed: true,
        estimated: false,
        session: "demo-main-loop".to_string(),
    };
    DiffPayload {
        // The timestamp change re-billed the whole context as cache writes.
        curr: rec(47, 900, 0, 13_800),
        curr_body,
        curr_response_body: String::new(),
        prev: Some(rec(46, 850, 11_900, 420)),
        prev_body: Some(prev_body),
    }
}

fn buffer_to_html(buf: &Buffer) -> String {
    const DEFAULT_FG: &str = "#c8ccd4";
    const DEFAULT_BG: &str = "#0b0d12";

    let css = |c: Color, default: &str| -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => default.to_string(),
        }
    };
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

    let area = buf.area();
    let mut html = format!(
        "<!doctype html><meta charset=\"utf-8\">\
         <body style=\"margin:0;background:{DEFAULT_BG}\">\
         <pre style=\"background:{DEFAULT_BG};color:{DEFAULT_FG};\
         font-family:'Cascadia Mono',Consolas,monospace;font-size:15px;\
         line-height:1.02;padding:24px;display:inline-block;margin:0\">"
    );
    for y in 0..area.height {
        let mut run = String::new();
        let mut run_style: Option<(String, String, bool)> = None;
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            let style = (
                css(cell.fg, DEFAULT_FG),
                css(cell.bg, DEFAULT_BG),
                cell.modifier.contains(Modifier::BOLD),
            );
            if run_style.as_ref() != Some(&style) {
                if let Some((fg, bg, bold)) = run_style.take() {
                    let weight = if bold { ";font-weight:700" } else { "" };
                    html.push_str(&format!(
                        "<span style=\"color:{fg};background:{bg}{weight}\">{}</span>",
                        escape(&run)
                    ));
                    run.clear();
                }
                run_style = Some(style);
            }
            run.push_str(cell.symbol());
        }
        if let Some((fg, bg, bold)) = run_style {
            let weight = if bold { ";font-weight:700" } else { "" };
            html.push_str(&format!(
                "<span style=\"color:{fg};background:{bg}{weight}\">{}</span>",
                escape(&run)
            ));
        }
        html.push('\n');
    }
    html.push_str("</pre>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_strings(widget: impl Widget, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    #[test]
    fn area_graph_renders_ramp() {
        let data: Vec<u64> = (0..80).collect();
        let rows = render_to_strings(
            AreaGraph {
                data: &data,
                stops: TOKENS_STOPS,
                baseline: true,
            },
            40,
            6,
        );
        for row in &rows {
            println!("{row}");
        }
        // Bottom row must be the fullest, top row the emptiest.
        let filled = |s: &String| s.chars().filter(|c| *c != ' ').count();
        assert!(filled(&rows[5]) > filled(&rows[0]));
    }

    #[test]
    fn area_graph_handles_empty_and_tiny() {
        render_to_strings(
            AreaGraph {
                data: &[],
                stops: TOKENS_STOPS,
                baseline: true,
            },
            40,
            6,
        );
        render_to_strings(
            AreaGraph {
                data: &[5],
                stops: TOKENS_STOPS,
                baseline: false,
            },
            1,
            1,
        );
    }

    #[test]
    fn area_graph_handles_more_data_than_width() {
        let data: Vec<u64> = (0..500).collect();
        render_to_strings(
            AreaGraph {
                data: &data,
                stops: TTFT_STOPS,
                baseline: true,
            },
            20,
            4,
        );
    }
}
