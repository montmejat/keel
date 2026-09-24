//! `keel top`: a live view of a running dataflow.
//!
//! Polls the daemon's control API and derives rates from counter deltas, so
//! the daemon only ever keeps totals.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use keel_daemon::control::{Client, LinkStatus, LogLine, NodeState, NodeStatus, Status};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Row, Sparkline, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::fmt;

const REFRESH: Duration = Duration::from_millis(250);
/// Sparkline samples kept per node: 30 s at the refresh rate.
const HISTORY: usize = 120;
const LOG_LINES: usize = 1000;
/// Rates are averaged over this many refreshes (1 s), so that a 30 Hz
/// stream doesn't read as 28 then 32 depending on where polls land.
const RATE_WINDOW: usize = 4;
/// Node name colours, assigned in dataflow order.
const PALETTE: [Color; 6] = [Color::Cyan, Color::Green, Color::Yellow, Color::Blue, Color::Magenta, Color::LightRed];
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub fn run(pid: u32) -> io::Result<()> {
    let mut app = App::new(Client::connect(pid)?);
    app.refresh();
    ratatui::run(|terminal| app.run(terminal))
}

/// `(messages, bytes)` by link source.
type Totals = HashMap<String, (u64, u64)>;

#[derive(Default, Clone, Copy)]
struct Rate {
    msgs: f64,
    bytes: f64,
}

impl std::ops::AddAssign for Rate {
    fn add_assign(&mut self, other: Self) {
        self.msgs += other.msgs;
        self.bytes += other.bytes;
    }
}

struct App {
    client: Client,
    status: Option<Status>,
    /// Link counters `(messages, bytes)` at recent polls, oldest first.
    counters: VecDeque<(Instant, Totals)>,
    /// By link source (`node/output`).
    link_rates: HashMap<String, Rate>,
    /// Messages in + out per second, by node.
    history: HashMap<String, VecDeque<u64>>,
    logs: VecDeque<LogLine>,
    next_log: u64,
    table: TableState,
    /// Show only the selected node's logs.
    filter_logs: bool,
    /// Set once the daemon is gone; the last status stays on screen.
    disconnected: bool,
}

impl App {
    fn new(client: Client) -> Self {
        Self {
            client,
            status: None,
            counters: VecDeque::new(),
            link_rates: HashMap::new(),
            history: HashMap::new(),
            logs: VecDeque::new(),
            next_log: 0,
            table: TableState::default().with_selected(0),
            filter_logs: false,
            disconnected: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut next_refresh = Instant::now() + REFRESH;
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(next_refresh.saturating_duration_since(Instant::now()))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        _ if ctrl_c => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => self.select(1),
                        KeyCode::Up | KeyCode::Char('k') => self.select(-1),
                        KeyCode::Char('f') => self.filter_logs = !self.filter_logs,
                        KeyCode::Char('s') if !self.disconnected => {
                            let _ = self.client.stop();
                        }
                        _ => {}
                    }
                }
            }
            if Instant::now() >= next_refresh {
                self.refresh();
                next_refresh = Instant::now() + REFRESH;
            }
        }
    }

    fn select(&mut self, delta: isize) {
        let count = self.status.as_ref().map_or(0, |s| s.nodes.len());
        if count > 0 {
            let current = self.table.selected().unwrap_or(0) as isize;
            self.table.select(Some((current + delta).clamp(0, count as isize - 1) as usize));
        }
    }

    fn selected_node(&self) -> Option<&NodeStatus> {
        self.status.as_ref()?.nodes.get(self.table.selected()?)
    }

    fn refresh(&mut self) {
        if self.disconnected {
            return;
        }
        let polled = self.client.status().and_then(|status| Ok((status, self.client.logs(self.next_log)?)));
        let Ok((status, logs)) = polled else {
            self.disconnected = true;
            self.link_rates.clear();
            return;
        };

        let now = Instant::now();
        if let Some((then, before)) = self.counters.front() {
            let dt = now.duration_since(*then).as_secs_f64();
            for link in &status.links {
                let (msgs, bytes) = before.get(&link.source).copied().unwrap_or_default();
                let rate = Rate {
                    msgs: link.messages.saturating_sub(msgs) as f64 / dt,
                    bytes: link.bytes.saturating_sub(bytes) as f64 / dt,
                };
                self.link_rates.insert(link.source.clone(), rate);
            }
        }
        if self.counters.len() == RATE_WINDOW {
            self.counters.pop_front();
        }
        let totals = status.links.iter().map(|l| (l.source.clone(), (l.messages, l.bytes))).collect();
        self.counters.push_back((now, totals));
        for node in &status.nodes {
            let (rate_in, rate_out) = self.node_rates(&node.id, &status.links);
            let history = self.history.entry(node.id.clone()).or_default();
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back((rate_in.msgs + rate_out.msgs).round() as u64);
        }
        self.status = Some(status);

        self.logs.extend(logs.lines);
        let excess = self.logs.len().saturating_sub(LOG_LINES);
        self.logs.drain(..excess);
        self.next_log = logs.next;
    }

    /// What a node receives and sends per second.
    fn node_rates(&self, node: &str, links: &[LinkStatus]) -> (Rate, Rate) {
        let (mut rate_in, mut rate_out) = (Rate::default(), Rate::default());
        for link in links {
            let rate = self.link_rates.get(&link.source).copied().unwrap_or_default();
            if node_of(&link.source) == node {
                rate_out += rate;
            }
            for _ in link.targets.iter().filter(|t| node_of(t) == node) {
                rate_in += rate;
            }
        }
        (rate_in, rate_out)
    }

    fn node_color(&self, node: &str) -> Color {
        let nodes = self.status.as_ref().map_or(&[][..], |s| &s.nodes);
        match nodes.iter().position(|n| n.id == node) {
            Some(i) => PALETTE[i % PALETTE.len()],
            None => Color::Magenta,
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let Some(status) = self.status.clone() else { return };
        let nodes_height = status.nodes.len() as u16 + 3;
        let links_height = (status.links.len() as u16 + 3).max(7);
        let [header, nodes, middle, logs, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(nodes_height),
            Constraint::Length(links_height),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [links, activity] =
            Layout::horizontal([Constraint::Percentage(66), Constraint::Percentage(34)]).areas(middle);

        self.draw_header(frame, header, &status);
        self.draw_nodes(frame, nodes, &status);
        self.draw_links(frame, links, &status);
        self.draw_activity(frame, activity);
        self.draw_logs(frame, logs);
        draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let (label, color) = match () {
            _ if self.disconnected => ("exited", Color::Red),
            _ if status.stopping => ("stopping", Color::Yellow),
            _ => ("running", Color::Green),
        };
        let line = Line::from(vec![
            Span::styled(" keel ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled(format!("● {label}"), Style::new().fg(color).bold()),
            Span::styled("   pid ", MUTED),
            Span::raw(status.pid.to_string()),
            Span::styled("   up ", MUTED),
            Span::raw(fmt::duration(Duration::from_millis(status.uptime_ms))),
            Span::styled("   ", MUTED),
            Span::styled(status.dataflow.display().to_string(), MUTED),
        ]);
        frame.render_widget(line, area);
    }

    fn draw_nodes(&mut self, frame: &mut Frame, area: Rect, status: &Status) {
        let id_width = status.nodes.iter().map(|n| n.id.len()).max().unwrap_or(4).max(4) as u16 + 2;
        let rows = status.nodes.iter().map(|node| {
            let (rate_in, rate_out) = self.node_rates(&node.id, &status.links);
            let (state, state_color) = state_label(&node.state);
            let shm = match node.shm_regions {
                0 => Span::styled("—", MUTED),
                n => Span::raw(format!("{}/{n} held  {}", node.shm_held, fmt::bytes(node.shm_bytes as f64))),
            };
            let row = Row::new(vec![
                Line::styled(node.id.clone(), Style::new().fg(self.node_color(&node.id)).bold()),
                Line::styled(state, state_color),
                Line::styled(node.pid.map_or("—".into(), |p| p.to_string()), MUTED),
                Line::raw(fmt::rate(rate_in.msgs)).right_aligned(),
                Line::raw(fmt::rate(rate_out.msgs)).right_aligned(),
                Line::raw(format!("{}/s", fmt::bytes(rate_out.bytes))).right_aligned(),
                Line::from(shm),
            ]);
            match node.state {
                NodeState::Exited { success: true, .. } => row.style(Style::new().add_modifier(Modifier::DIM)),
                _ => row,
            }
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(id_width),
                Constraint::Length(11),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(13),
                Constraint::Min(18),
            ],
        )
        .header(header_row(["NODE", "STATE", "PID", "IN/s", "OUT/s", "OUT", "SHARED MEMORY"], [3, 4, 5]))
        .row_highlight_style(Style::new().bg(Color::Indexed(236)))
        .highlight_symbol("▌")
        .block(panel("Nodes"));
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_links(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let widest = |names: &mut dyn Iterator<Item = usize>| names.max().unwrap_or(0).max(4) as u16;
        let from_width = widest(&mut status.links.iter().map(|l| l.source.len()));
        let to_width = widest(&mut status.links.iter().map(|l| l.targets.join(", ").len()));
        let rows = status.links.iter().map(|link| {
            let rate = self.link_rates.get(&link.source).copied().unwrap_or_default();
            let targets = match link.targets.len() {
                0 => Line::styled("—", MUTED),
                _ => Line::raw(link.targets.join(", ")),
            };
            Row::new(vec![
                Line::styled(link.source.clone(), Style::new().fg(self.node_color(node_of(&link.source)))),
                Line::styled("→", MUTED),
                targets,
                Line::raw(fmt::rate(rate.msgs)).right_aligned(),
                Line::raw(format!("{}/s", fmt::bytes(rate.bytes))).right_aligned(),
                Line::styled(fmt::bytes(link.bytes as f64), MUTED).right_aligned(),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(from_width),
                Constraint::Length(1),
                Constraint::Length(to_width),
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(9),
            ],
        )
        .header(header_row(["FROM", "", "TO", "MSG/s", "THROUGHPUT", "TOTAL"], [3, 4, 5]))
        .block(panel("Links"));
        frame.render_widget(table, area);
    }

    fn draw_activity(&self, frame: &mut Frame, area: Rect) {
        let Some(node) = self.selected_node() else { return };
        let history = self.history.get(&node.id);
        let samples: Vec<u64> = history.map_or(Vec::new(), |h| {
            let width = area.width.saturating_sub(2) as usize;
            h.iter().skip(h.len().saturating_sub(width)).copied().collect()
        });
        let now = samples.last().copied().unwrap_or(0);
        let peak = samples.iter().copied().max().unwrap_or(0);
        let color = self.node_color(&node.id);
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(node.id.clone(), Style::new().fg(color).bold()),
            Span::styled(" msg/s ", MUTED),
        ]);
        let stats = Line::from(vec![
            Span::styled(" now ", MUTED),
            Span::raw(now.to_string()),
            Span::styled("  peak ", MUTED),
            Span::raw(format!("{peak} ")),
        ])
        .right_aligned();
        let sparkline = Sparkline::default()
            .block(panel("").title(title).title_bottom(stats))
            .data(&samples)
            .style(Style::new().fg(color));
        frame.render_widget(sparkline, area);
    }

    fn draw_logs(&self, frame: &mut Frame, area: Rect) {
        let filter = self.filter_logs.then(|| self.selected_node().map(|n| n.id.clone())).flatten();
        let height = area.height.saturating_sub(2) as usize;
        let matching: Vec<&LogLine> =
            self.logs.iter().filter(|l| filter.as_ref().is_none_or(|node| &l.node == node)).collect();
        let lines: Vec<Line> = matching[matching.len().saturating_sub(height)..]
            .iter()
            .map(|log| {
                let daemon = log.node == "daemon";
                Line::from(vec![
                    Span::styled(format!("{:>9} ", fmt::timestamp(log.t_ms)), MUTED),
                    Span::styled(format!("{:<10} ", log.node), Style::new().fg(self.node_color(&log.node))),
                    if daemon { Span::styled(log.text.clone(), MUTED) } else { Span::raw(log.text.clone()) },
                ])
            })
            .collect();
        let title = match &filter {
            Some(node) => format!("Logs · {node}"),
            None => "Logs · all nodes".into(),
        };
        frame.render_widget(Paragraph::new(lines).block(panel(&title)), area);
    }
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let key = |k: &'static str| Span::styled(k, Style::new().fg(ACCENT).bold());
    let label = |l: &'static str| Span::styled(l, MUTED);
    let line = Line::from(vec![
        Span::raw(" "),
        key("↑↓"),
        label(" select   "),
        key("f"),
        label(" filter logs   "),
        key("s"),
        label(" stop dataflow   "),
        key("q"),
        label(" quit"),
    ]);
    frame.render_widget(line, area);
}

fn panel(title: &str) -> Block<'static> {
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(MUTED).padding(Padding::horizontal(1));
    match title {
        "" => block,
        _ => block.title(Line::from(format!(" {title} ")).bold()),
    }
}

fn header_row<const N: usize>(labels: [&'static str; N], right_aligned: [usize; 3]) -> Row<'static> {
    let cells = labels.into_iter().enumerate().map(|(i, label)| {
        let line = Line::styled(label, Style::new().fg(ACCENT).bold());
        if right_aligned.contains(&i) {
            line.right_aligned()
        } else {
            line
        }
    });
    Row::new(cells)
}

fn state_label(state: &NodeState) -> (String, Color) {
    match state {
        NodeState::Starting => ("◌ starting".into(), Color::Yellow),
        NodeState::Running => ("● running".into(), Color::Green),
        NodeState::Stopping => ("◐ stopping".into(), Color::Yellow),
        NodeState::Exited { success: true, .. } => ("✓ exited".into(), MUTED),
        NodeState::Exited { success: false, detail } if detail == "killed" => ("✗ killed".into(), Color::Red),
        NodeState::Exited { success: false, .. } => ("✗ failed".into(), Color::Red),
    }
}

/// `node` in `node/port`.
fn node_of(endpoint: &str) -> &str {
    endpoint.split_once('/').map_or(endpoint, |(node, _)| node)
}
