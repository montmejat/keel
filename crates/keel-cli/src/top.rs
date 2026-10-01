//! `keel top`: a live view of a running dataflow.
//!
//! Polls the control API and derives rates from counter deltas, so daemons
//! only ever keep totals. Through the coordinator it shows every machine:
//! where each node runs, links across machines, and latency per link.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use keel_daemon::control::{Client, InputReport, LinkStatus, LogLine, NodeState, NodeStatus, Status};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Row, Sparkline, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::fmt;

const REFRESH: Duration = Duration::from_millis(250);
/// Latency is heavier to gather (every daemon reads its nodes' histograms).
const LATENCY_REFRESH: Duration = Duration::from_secs(1);
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
    /// Per `(node, input)`.
    latency: HashMap<(String, String), InputReport>,
    latency_polled: Option<Instant>,
    /// Draw the dataflow as a graph instead of the links table.
    graph: bool,
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
            latency: HashMap::new(),
            latency_polled: None,
            graph: false,
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
                        KeyCode::Char('g') => self.graph = !self.graph,
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

        // A daemon moved on to another dataflow: start over.
        let previous_uptime = self.status.as_ref().map_or(0, |s| s.uptime_ms);
        if logs.next < self.next_log || status.uptime_ms < previous_uptime {
            self.logs.clear();
            self.next_log = 0;
            self.counters.clear();
            self.link_rates.clear();
            self.history.clear();
            self.status = Some(status);
            return;
        }

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
        // Drawing an empty table (an idle daemon) clears the selection.
        if self.table.selected().is_none() {
            self.select(0);
        }

        if self.latency_polled.is_none_or(|t| t.elapsed() >= LATENCY_REFRESH) {
            self.latency_polled = Some(Instant::now());
            if let Ok(report) = self.client.latency() {
                let by_input = report.inputs.into_iter().map(|i| ((i.node.clone(), i.input.clone()), i));
                self.latency = by_input.collect();
            }
        }

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

    /// Latency p50 and p99 into a link's targets: the worst of them.
    fn link_latency(&self, link: &LinkStatus) -> Option<(u64, u64, Option<u64>)> {
        let reports = link.targets.iter().filter_map(|t| {
            let (node, input) = endpoint_parts(t);
            self.latency.get(&(node.to_owned(), input.to_owned()))
        });
        reports.fold(None, |worst, r| {
            let (p50, p99) = (r.latency.p50, r.latency.p99);
            let error = r.clock_error_ns.filter(|&e| e > 0);
            match worst {
                Some((w50, w99, e)) if w99 >= p99 => Some((w50, w99, e)),
                _ if r.latency.count == 0 => worst,
                _ => Some((p50, p99, error)),
            }
        })
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
        let (from_width, to_width) = link_widths(&status);
        // Link columns, spacing, borders and padding.
        let links_width =
            (from_width + to_width + 1 + 7 + 12 + 9 + 18 + 6 + 4).clamp(middle.width * 3 / 5, middle.width * 4 / 5);
        let [links, activity] =
            Layout::horizontal([Constraint::Length(links_width), Constraint::Fill(1)]).areas(middle);

        self.draw_header(frame, header, &status);
        self.draw_nodes(frame, nodes, &status);
        if self.graph {
            self.draw_graph(frame, links, &status);
        } else {
            self.draw_links(frame, links, &status);
        }
        self.draw_activity(frame, activity);
        self.draw_logs(frame, logs);
        draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let (label, color) = match () {
            _ if self.disconnected => ("exited", Color::Red),
            _ if status.dataflow.is_none() => ("idle", Color::Blue),
            _ if status.stopping => ("stopping", Color::Yellow),
            _ => ("running", Color::Green),
        };
        let machines: BTreeSet<&str> = status.nodes.iter().filter_map(|n| n.machine.as_deref()).collect();
        let mut machine = match &status.machine {
            Some(machine) => vec![Span::styled("   machine ", MUTED), Span::raw(machine.clone())],
            None if status.coordinator => vec![
                Span::styled("   cluster ", MUTED),
                Span::raw(machines.iter().copied().collect::<Vec<_>>().join(", ")),
            ],
            None => vec![],
        };
        if let Some(deployment) = &status.deployment {
            machine.extend([Span::styled("   deployment ", MUTED), Span::raw(deployment.clone())]);
        }
        let dataflow = match &status.dataflow {
            Some(path) => path.file_name().map_or(path.display().to_string(), |f| f.to_string_lossy().into()),
            None => "waiting for a dataflow".into(),
        };
        let mut spans = vec![
            Span::styled(" keel ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled(format!("● {label}"), Style::new().fg(color).bold()),
            Span::styled("   pid ", MUTED),
            Span::raw(status.pid.to_string()),
        ];
        spans.extend(machine);
        spans.extend([
            Span::styled("   up ", MUTED),
            Span::raw(fmt::duration(Duration::from_millis(status.uptime_ms))),
            Span::styled("   ", MUTED),
            Span::styled(dataflow, MUTED),
        ]);
        let line = Line::from(spans);
        frame.render_widget(line, area);
    }

    fn draw_nodes(&mut self, frame: &mut Frame, area: Rect, status: &Status) {
        let id_width = status.nodes.iter().map(|n| n.id.len()).max().unwrap_or(4).max(4) as u16 + 2;
        let program_width = status.nodes.iter().map(|n| n.program.len()).max().unwrap_or(7).max(7) as u16;
        let multi = status.nodes.iter().any(|n| n.machine.is_some());
        let machine_width = if multi {
            status.nodes.iter().filter_map(|n| n.machine.as_ref()).map(|m| m.len()).max().unwrap_or(7).max(7) as u16
        } else {
            0
        };
        let rows = status.nodes.iter().map(|node| {
            let (rate_in, rate_out) = self.node_rates(&node.id, &status.links);
            let (mut state, state_color) = state_label(&node.state);
            if node.restarts > 0 {
                state.push_str(&format!(" ↻{}", node.restarts));
            }
            let shm = match node.shm_regions {
                0 => Span::styled("—", MUTED),
                n => Span::raw(format!("{}/{n} held  {}", node.shm_held, fmt::bytes(node.shm_bytes as f64))),
            };
            let row = Row::new(vec![
                Line::styled(node.id.clone(), Style::new().fg(self.node_color(&node.id)).bold()),
                Line::styled(node.program.clone(), MUTED),
                Line::raw(node.machine.clone().unwrap_or_default()),
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
                Constraint::Length(program_width),
                Constraint::Length(machine_width),
                Constraint::Length(14),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(13),
                Constraint::Min(18),
            ],
        )
        .header(header_row(
            [
                "NODE",
                "PROGRAM",
                if multi { "MACHINE" } else { "" },
                "STATE",
                "PID",
                "IN/s",
                "OUT/s",
                "OUT",
                "SHARED MEMORY",
            ],
            [5, 6, 7],
        ))
        .row_highlight_style(Style::new().bg(Color::Indexed(236)))
        .highlight_symbol("▌")
        .block(panel("Nodes"));
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_links(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let (from_width, to_width) = link_widths(status);
        let rows = status.links.iter().map(|link| {
            let rate = self.link_rates.get(&link.source).copied().unwrap_or_default();
            let targets = match link.targets.len() {
                0 => Line::styled("—", MUTED),
                _ => Line::raw(link.targets.join(", ")),
            };
            let (p50, p99) = match self.link_latency(link) {
                Some((p50, p99, error)) => {
                    let error = error.map_or(String::new(), |e| format!(" ±{}", fmt::nanos(e)));
                    (Line::raw(fmt::nanos(p50)), Line::raw(format!("{}{error}", fmt::nanos(p99))))
                }
                None => (Line::styled("—", MUTED), Line::styled("—", MUTED)),
            };
            Row::new(vec![
                Line::styled(link.source.clone(), Style::new().fg(self.node_color(node_of(&link.source)))),
                Line::styled("→", MUTED),
                targets,
                Line::raw(fmt::rate(rate.msgs)).right_aligned(),
                Line::raw(format!("{}/s", fmt::bytes(rate.bytes))).right_aligned(),
                p50.right_aligned(),
                p99.right_aligned(),
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
                Constraint::Length(18),
            ],
        )
        .header(header_row(["FROM", "", "TO", "MSG/s", "THROUGHPUT", "LAT p50", "p99"], [3, 4, 5, 6]))
        .block(panel("Links"));
        frame.render_widget(table, area);
    }

    /// The dataflow as a graph: nodes in columns by depth (sources on the
    /// left), each link labelled with its rate and median latency. Links
    /// crossing machines are yellow.
    fn draw_graph(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let block = panel("Graph");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (w, h) = (inner.width as f64, inner.height as f64);
        if status.nodes.is_empty() || w < 10.0 || h < 3.0 {
            return;
        }
        let order: HashMap<&str, usize> = status.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let machine_of: HashMap<&str, Option<&str>> =
            status.nodes.iter().map(|n| (n.id.as_str(), n.machine.as_deref())).collect();
        let edges: Vec<(&str, &str, &LinkStatus)> = (status.links.iter())
            .flat_map(|l| l.targets.iter().map(move |t| (node_of(&l.source), node_of(t), l)))
            .filter(|(a, b, _)| order.contains_key(a) && order.contains_key(b))
            .collect();
        // Depth by longest path, following links that go down the dataflow's
        // order (a link back up, closing a cycle, doesn't push nodes right).
        let mut depth: HashMap<&str, usize> = order.keys().map(|n| (*n, 0)).collect();
        for _ in 0..order.len() {
            for (a, b, _) in &edges {
                if order[a] < order[b] && depth[b] < depth[a] + 1 {
                    depth.insert(b, depth[a] + 1);
                }
            }
        }
        let columns = depth.values().max().unwrap() + 1;
        let mut rows: Vec<Vec<&str>> = vec![Vec::new(); columns];
        for node in &status.nodes {
            rows[depth[node.id.as_str()]].push(&node.id);
        }
        // Where each node's label starts and ends, and its line (canvas y
        // grows upwards).
        let mut place: HashMap<&str, (f64, f64, f64)> = HashMap::new();
        for (col, nodes) in rows.iter().enumerate() {
            let x = (w * col as f64 / columns as f64).floor() + 1.0;
            for (i, node) in nodes.iter().enumerate() {
                let y = (h - h * (i as f64 + 0.5) / nodes.len() as f64).floor();
                place.insert(node, (x, x + node.len() as f64 + 2.0, y));
            }
        }
        let canvas = Canvas::default().x_bounds([0.0, w]).y_bounds([0.0, h]).paint(|ctx| {
            for (a, b, link) in &edges {
                let ((_, ax, ay), (bx, _, by)) = (place[a], place[b]);
                let remote = machine_of[a] != machine_of[b];
                let color = if remote { Color::Yellow } else { MUTED };
                let rate = self.link_rates.get(&link.source).copied().unwrap_or_default();
                let latency =
                    self.link_latency(link).map_or(String::new(), |(p50, _, _)| format!(" {}", fmt::nanos(p50)));
                let label = format!("{}/s{latency}", fmt::rate(rate.msgs));
                let mut segment = |x1, y1, x2, y2| ctx.draw(&CanvasLine { x1, y1, x2, y2, color });
                if depth[b] > depth[a] {
                    segment(ax, ay + 0.5, bx - 1.0, by + 0.5);
                    ctx.print(bx - 1.0, by, Line::styled(">", Style::new().fg(color)));
                    let mid = ((ax + bx) / 2.0 - label.len() as f64 / 2.0).max(ax);
                    ctx.print(mid, (ay + by) / 2.0 + 1.0, Line::styled(label, Style::new().fg(color)));
                } else {
                    // Back up the dataflow (a cycle): around, underneath.
                    let low = ay.min(by) - 2.0;
                    segment(ax, ay + 0.5, ax, low + 0.5);
                    segment(ax, low + 0.5, bx, low + 0.5);
                    segment(bx, low + 0.5, bx, by - 0.5);
                    ctx.print(bx, by - 1.0, Line::styled("^", Style::new().fg(color)));
                    let mid = ((ax + bx) / 2.0 - label.len() as f64 / 2.0).max(0.0);
                    ctx.print(mid, low, Line::styled(format!(" {label} "), Style::new().fg(color)));
                }
            }
            ctx.layer();
            for (node, (x, _, y)) in &place {
                let style = Style::new().fg(self.node_color(node)).bold();
                ctx.print(*x, *y, Line::styled(format!("[{node}]"), style));
                if let Some(Some(machine)) = machine_of.get(node) {
                    ctx.print(*x + 1.0, *y - 1.0, Line::styled(format!("@{machine}"), MUTED));
                }
            }
        });
        frame.render_widget(canvas, inner);
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
        key("g"),
        label(" graph/links   "),
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

fn header_row<const N: usize, const R: usize>(labels: [&'static str; N], right_aligned: [usize; R]) -> Row<'static> {
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

/// `(node, input)` of `node/input` or `node/input@machine`.
fn endpoint_parts(endpoint: &str) -> (&str, &str) {
    let endpoint = endpoint.split('@').next().unwrap_or(endpoint);
    endpoint.split_once('/').unwrap_or((endpoint, ""))
}

/// Widths of the FROM and TO columns.
fn link_widths(status: &Status) -> (u16, u16) {
    let widest = |widths: &mut dyn Iterator<Item = usize>| widths.max().unwrap_or(0).max(4) as u16;
    let from = widest(&mut status.links.iter().map(|l| l.source.len()));
    let to = widest(&mut status.links.iter().map(|l| l.targets.join(", ").len()));
    (from, to)
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
