//! `checkout_service_tui` — a ratatui/crossterm TUI wrapper around the same
//! allocation logic as `checkout_service.rs` (console) and
//! `checkout_service_gui.rs` (iced GUI, parked — its `wgpu`/`winit`
//! dependency surface is implicated in an undiagnosed injection-path crash;
//! this target avoids that dependency surface entirely: no GPU/windowing
//! threads, just a terminal redraw loop). `HeapLensAlloc` is still the
//! `#[global_allocator]`, and this remains a single native Rust binary.
//!
//! Timing model is identical to checkout_service_gui.rs's: idle ambient
//! traffic pre-start, a deterministic 90s scripted window post-start
//! (leak@15s, hot-cluster@30s/33s/40s, storm@45s/60s), then bounded
//! ambient-indefinite traffic — see SCHEDULE below.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

#[path = "support/checkout_common.rs"]
mod checkout_common;
use checkout_common::{
    metrics_flush_write_entry, order_queue_accept_orders,
    payment_gateway_pool_checkout_connections, request_handler_handle,
};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Sparkline};
use ratatui::Frame;
use std::collections::VecDeque;
use std::io::Stdout;
use std::time::{Duration, Instant};

const TICK_MS: u64 = 150;
const MAX_ORDER_ROWS: usize = 10;
const MAX_LOG_LINES: usize = 200;
const MAX_SPARK_BARS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptEvent {
    LeakCheckout,
    LeakRelease,
    HotStart,
    HotGrow,
    HotDrain,
    StormStart,
    StormEnd,
    ScriptedWindowEnd,
}

const SCHEDULE: &[(u64, ScriptEvent)] = &[
    (15_000, ScriptEvent::LeakCheckout),
    (17_000, ScriptEvent::LeakRelease),
    (30_000, ScriptEvent::HotStart),
    (33_000, ScriptEvent::HotGrow),
    (40_000, ScriptEvent::HotDrain),
    (45_000, ScriptEvent::StormStart),
    (60_000, ScriptEvent::StormEnd),
    (90_000, ScriptEvent::ScriptedWindowEnd),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Nominal,
    FuiteActive,
    GrappeEnCroissance,
    RafaleDeTraitement,
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Phase::Nominal => "nominal",
            Phase::FuiteActive => "fuite active",
            Phase::GrappeEnCroissance => "grappe en croissance",
            Phase::RafaleDeTraitement => "rafale de traitement",
        }
    }
    fn color(&self) -> Color {
        match self {
            Phase::Nominal => Color::Green,
            Phase::FuiteActive => Color::Red,
            Phase::GrappeEnCroissance => Color::Yellow,
            Phase::RafaleDeTraitement => Color::Magenta,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderStatus {
    EnAttente,
    Traitement,
    Terminee,
}

impl OrderStatus {
    fn label(&self) -> &'static str {
        match self {
            OrderStatus::EnAttente => "En attente",
            OrderStatus::Traitement => "Traitement",
            OrderStatus::Terminee => "Terminée",
        }
    }
    fn color(&self) -> Color {
        match self {
            OrderStatus::EnAttente => Color::Yellow,
            OrderStatus::Traitement => Color::Blue,
            OrderStatus::Terminee => Color::Green,
        }
    }
}

struct OrderRow {
    id: u64,
    customer: String,
    amount_cents: u32,
    status: OrderStatus,
    born_tick: u64,
}

struct State {
    app_start: Instant,
    tick_count: u64,

    started: bool,
    run_start: Option<Instant>,
    scripted_done_through: usize,

    pool_manager: Option<Vec<u8>>,
    leaked_connections_pending: Option<Vec<Vec<u8>>>,
    leaked_connections: Vec<Vec<u8>>,

    queue_owner: Option<Vec<u8>>,
    backlog: Vec<Vec<u8>>,
    hot_extra_added: bool,

    storm_active: bool,
    storm_bursts_this_run: usize,

    phase: Phase,

    orders: VecDeque<OrderRow>,
    order_seq: u64,
    throughput_history: VecDeque<u64>,
    this_tick_alloc_events: u64,
    log_lines: VecDeque<String>,

    quit: bool,
}

impl State {
    fn new() -> Self {
        let mut s = State {
            app_start: Instant::now(),
            tick_count: 0,
            started: false,
            run_start: None,
            scripted_done_through: 0,
            pool_manager: None,
            leaked_connections_pending: None,
            leaked_connections: Vec::new(),
            queue_owner: None,
            backlog: Vec::new(),
            hot_extra_added: false,
            storm_active: false,
            storm_bursts_this_run: 0,
            phase: Phase::Nominal,
            orders: VecDeque::new(),
            order_seq: 0,
            throughput_history: VecDeque::new(),
            this_tick_alloc_events: 0,
            log_lines: VecDeque::new(),
            quit: false,
        };
        s.log("checkout_service_tui starting — request_handler online, payment_gateway_pool warm, order_queue idle");
        s
    }

    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        if std::env::var_os("HEAPLENS_GUI_AUTOSTART").is_some() {
            eprintln!("[{:>6}ms] {}", self.app_start.elapsed().as_millis(), line);
        }
        self.log_lines.push_front(format!("[{:>6}ms] {}", self.app_start.elapsed().as_millis(), line));
        while self.log_lines.len() > MAX_LOG_LINES {
            self.log_lines.pop_back();
        }
    }

    // ── Owner-allocation funnels ────────────────────────────────────────
    // Both call sites for a given owner's children route through the SAME
    // named function so phi's ancestor-frame matching sees a consistent
    // effective site for the owner — identical pattern to
    // checkout_service_gui.rs, unchanged by the rendering-layer pivot.

    #[inline(never)]
    fn leak_phase_tick(&mut self, release: bool) {
        if !release {
            self.pool_manager = Some(vec![0u8; 256]);
            let connections = payment_gateway_pool_checkout_connections(12);
            self.this_tick_alloc_events += 1 + connections.len() as u64;
            self.leaked_connections_pending = Some(connections);
        } else if let Some(conns) = self.leaked_connections_pending.take() {
            self.pool_manager = None;
            let n = conns.len();
            self.leaked_connections.extend(conns);
            self.log(format!(
                "payment_gateway_pool: manager freed — {n} connections now orphaned and will never be released"
            ));
        }
    }

    #[inline(never)]
    fn hot_phase_tick(&mut self, add_extra: bool) {
        if self.queue_owner.is_none() {
            self.queue_owner = Some(vec![0u8; 512]);
            let initial = order_queue_accept_orders(10);
            self.this_tick_alloc_events += 1 + initial.len() as u64;
            self.backlog.extend(initial);
        }
        if add_extra && !self.hot_extra_added {
            let extra = order_queue_accept_orders(30);
            self.this_tick_alloc_events += extra.len() as u64;
            self.backlog.extend(extra);
            self.hot_extra_added = true;
            self.log(format!("order_queue: backlog at {} orders and still growing", self.backlog.len()));
        }
    }

    #[inline(never)]
    fn storm_burst(&mut self, n: usize) {
        for i in 0..n {
            metrics_flush_write_entry(i);
        }
        self.this_tick_alloc_events += n as u64;
        self.storm_bursts_this_run += 1;
    }

    #[inline(never)]
    fn ambient_tick(&mut self, n: usize) {
        for i in 0..n {
            request_handler_handle(i);
        }
        self.this_tick_alloc_events += n as u64;
    }

    fn spawn_order(&mut self) {
        self.order_seq += 1;
        let amount_cents = 500 + ((self.order_seq * 137) % 9500) as u32;
        self.orders.push_front(OrderRow {
            id: self.order_seq,
            customer: format!("client-{:04}", (self.order_seq * 977) % 9973),
            amount_cents,
            status: OrderStatus::EnAttente,
            born_tick: self.tick_count,
        });
        while self.orders.len() > MAX_ORDER_ROWS {
            self.orders.pop_back();
        }
    }

    fn advance_order_statuses(&mut self) {
        for o in self.orders.iter_mut() {
            let age = self.tick_count.saturating_sub(o.born_tick);
            o.status = if age < 4 {
                OrderStatus::EnAttente
            } else if age < 10 {
                OrderStatus::Traitement
            } else {
                OrderStatus::Terminee
            };
        }
    }

    fn next_script_event_countdown(&self) -> Option<Duration> {
        let run_start = self.run_start?;
        let elapsed_ms = run_start.elapsed().as_millis() as u64;
        SCHEDULE
            .iter()
            .find(|(offset, _)| *offset > elapsed_ms)
            .map(|(offset, _)| Duration::from_millis(offset - elapsed_ms))
    }

    fn fire_due_script_events(&mut self) {
        let Some(run_start) = self.run_start else { return };
        let elapsed_ms = run_start.elapsed().as_millis() as u64;
        while self.scripted_done_through < SCHEDULE.len()
            && SCHEDULE[self.scripted_done_through].0 <= elapsed_ms
        {
            let (_, event) = SCHEDULE[self.scripted_done_through];
            self.scripted_done_through += 1;
            match event {
                ScriptEvent::LeakCheckout => {
                    self.phase = Phase::FuiteActive;
                    self.log("EVENT: FLAW — payment_gateway_pool checking out 12 connections (leak incoming)");
                    self.leak_phase_tick(false);
                }
                ScriptEvent::LeakRelease => self.leak_phase_tick(true),
                ScriptEvent::HotStart => {
                    self.phase = Phase::GrappeEnCroissance;
                    self.log("EVENT: FLAW — order_queue backlog growing past healthy size (hot cluster)");
                    self.hot_phase_tick(false);
                }
                ScriptEvent::HotGrow => self.hot_phase_tick(true),
                ScriptEvent::HotDrain => {
                    self.backlog.clear();
                    self.queue_owner = None;
                    self.hot_extra_added = false;
                    self.log("order_queue: backlog drained, back to a healthy depth");
                    self.phase = Phase::Nominal;
                }
                ScriptEvent::StormStart => {
                    self.phase = Phase::RafaleDeTraitement;
                    self.storm_active = true;
                    self.log("EVENT: FLAW — metrics_flush bursting log writes far above the healthy rate (allocation storm)");
                }
                ScriptEvent::StormEnd => {
                    self.storm_active = false;
                    self.phase = Phase::Nominal;
                    self.log(format!(
                        "metrics_flush: {} unthrottled bursts written this phase",
                        self.storm_bursts_this_run
                    ));
                }
                ScriptEvent::ScriptedWindowEnd => {
                    self.log("scripted window complete — moving to ambient indefinite traffic");
                }
            }
        }
    }

    fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.run_start = Some(Instant::now());
            self.log("=== Démarrer ('s') pressé — scénario programmé démarré ===");
        }
    }

    fn tick(&mut self) {
        self.tick_count += 1;
        self.this_tick_alloc_events = 0;

        if !self.started && self.tick_count == 2 && std::env::var_os("HEAPLENS_GUI_AUTOSTART").is_some() {
            self.start();
        }

        if self.started {
            self.fire_due_script_events();
        }

        let in_scripted_window = self
            .run_start
            .map(|rs| rs.elapsed() < Duration::from_millis(SCHEDULE.last().unwrap().0))
            .unwrap_or(false);
        let ambient_indefinite = self.started && !in_scripted_window;

        if self.storm_active {
            if self.tick_count % 3 == 0 {
                self.storm_burst(120);
            }
        } else {
            self.ambient_tick(2);
            if self.tick_count % 6 == 0 {
                self.spawn_order();
            }
        }

        if ambient_indefinite {
            let jitter = (self.tick_count.wrapping_mul(2654435761) >> 24) % 37;
            if jitter == 0 {
                self.ambient_tick(40);
            }
            if jitter == 5 {
                let mini = order_queue_accept_orders(6);
                drop(mini);
            }
        }

        self.advance_order_statuses();
        self.throughput_history.push_back(self.this_tick_alloc_events);
        while self.throughput_history.len() > MAX_SPARK_BARS {
            self.throughput_history.pop_front();
        }
    }
}

fn ui(f: &mut Frame, s: &State) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(f.area());

    draw_header(f, root[0], s);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(root[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Min(6),
        ])
        .split(body[0]);

    draw_orders(f, left[0], s);
    draw_gauge(f, left[1], s);
    draw_sparkline(f, left[2], s);
    draw_log(f, left[3], s);

    draw_controller(f, body[1], s);
}

fn draw_header(f: &mut Frame, area: Rect, s: &State) {
    let text = Line::from(vec![
        Span::styled(" Checkout Ops Console ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(" Système opérationnel ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw(format!("   {:>6}s", s.app_start.elapsed().as_secs())),
    ]);
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
}

fn draw_orders(f: &mut Frame, area: Rect, s: &State) {
    let items: Vec<ListItem> = s
        .orders
        .iter()
        .map(|o| {
            let line = Line::from(vec![
                Span::raw(format!("#{:05}  ", o.id)),
                Span::raw(format!("{:<14}", o.customer)),
                Span::raw(format!("{:>8.2} €  ", o.amount_cents as f32 / 100.0)),
                Span::styled(o.status.label(), Style::default().fg(o.status.color())),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Commandes en direct "));
    f.render_widget(list, area);
}

fn draw_gauge(f: &mut Frame, area: Rect, s: &State) {
    let checked_out = s.leaked_connections.len() + s.pool_manager.as_ref().map(|_| 12).unwrap_or(0);
    let color = if checked_out == 0 {
        Color::Green
    } else if checked_out < 40 {
        Color::Yellow
    } else {
        Color::Red
    };
    let ratio = (checked_out as f64 / 80.0).min(1.0);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Pool de connexions (payment_gateway_pool) "))
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(format!("{checked_out} connexions retenues"));
    f.render_widget(gauge, area);
}

fn draw_sparkline(f: &mut Frame, area: Rect, s: &State) {
    let data: Vec<u64> = s.throughput_history.iter().copied().collect();
    let spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" Débit (metrics_flush) "))
        .data(&data)
        .style(Style::default().fg(Color::Blue));
    f.render_widget(spark, area);
}

fn draw_log(f: &mut Frame, area: Rect, s: &State) {
    let items: Vec<ListItem> = s.log_lines.iter().map(|l| ListItem::new(l.clone())).collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Journal "));
    f.render_widget(list, area);
}

fn draw_controller(f: &mut Frame, area: Rect, s: &State) {
    let mut lines = vec![
        Line::from(Span::styled("Contrôleur de test", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
    ];
    if s.started {
        lines.push(Line::from(Span::styled("En cours…", Style::default().fg(Color::Blue))));
    } else {
        lines.push(Line::from(Span::styled("[s] Démarrer", Style::default().fg(Color::Green))));
    }
    lines.push(Line::from(""));
    let countdown_text = if s.started {
        match s.next_script_event_countdown() {
            Some(remaining) => format!("Prochain traitement programmé dans : {}s", remaining.as_secs()),
            None => "Trafic ambiant indéfini en cours".to_string(),
        }
    } else {
        "En attente de démarrage".to_string()
    };
    lines.push(Line::from(countdown_text));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("Phase : "),
        Span::styled(s.phase.label(), Style::default().fg(s.phase.color())),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from("[q] Quitter"));

    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Test "));
    f.render_widget(p, area);
}

fn main() -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut state = State::new();
    let result = run(&mut terminal, &mut state);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>, state: &mut State) -> std::io::Result<()> {
    // Validation-only: HEAPLENS_GUI_AUTOSTART (same env var that drives
    // autostart) also skips the actual terminal draw and event::poll here.
    // Rendering to an inherited console under an automated test harness can
    // be far slower than a native terminal, which starves the real-time
    // schedule loop and lets multiple T+Ns events become due in the same
    // tick — their alloc+free events then land in the same daemon diff
    // batch and cancel out (drain_diff's documented same-window
    // cancellation), hiding real peak topology from an observer. The
    // schedule itself is still driven by real Instant::now(), so this only
    // removes console I/O from the loop, not timing accuracy.
    let headless = std::env::var_os("HEAPLENS_GUI_AUTOSTART").is_some();

    loop {
        if !headless {
            terminal.draw(|f| ui(f, state))?;

            if event::poll(Duration::from_millis(TICK_MS))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => state.quit = true,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => state.quit = true,
                        KeyCode::Char('s') => state.start(),
                        _ => {}
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(TICK_MS));
        }

        if state.quit {
            return Ok(());
        }

        state.tick();
    }
}
