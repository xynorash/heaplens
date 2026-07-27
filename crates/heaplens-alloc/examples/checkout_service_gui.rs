//! `checkout_service_gui` — an iced-based, production-realistic GUI wrapper
//! around the same allocation logic as `checkout_service.rs` (the console
//! fallback). This target exists because Flutter's Dart VM heap is invisible
//! to a Rust `#[global_allocator]` — a Flutter-based demo target would show
//! nothing on HeapLens. This binary links `HeapLensAlloc` directly and
//! drives the exact same `checkout_common` allocation sites as the console
//! version, from an iced Elm-style app instead of a `loop { }`.
//!
//! Timing model:
//!   - Idle (pre "Démarrer"): ambient healthy traffic only, indefinitely.
//!   - Scripted (T+0..90s post "Démarrer"): deterministic schedule matching
//!     checkout_service.rs's own phase shapes (leak / hot cluster / storm),
//!     compressed into a fixed 90s window instead of an infinite 15s-cycle
//!     repeat — see SCHEDULE below.
//!   - Ambient-indefinite (T+90s+): jittered, bounded ambient traffic. The
//!     leaked connections from the scripted leak keep accumulating forever
//!     by design (that's the point of a leak); everything else stays
//!     bounded so the UI stays readable over a long run.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

#[path = "support/checkout_common.rs"]
mod checkout_common;
use checkout_common::{
    metrics_flush_write_entry, order_queue_accept_orders,
    payment_gateway_pool_checkout_connections, request_handler_handle,
};

use iced::widget::{button, column, container, progress_bar, row, scrollable, text, Space};
use iced::{Alignment, Color, Element, Length, Subscription, Theme};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const TICK_MS: u64 = 150;
const MAX_ORDER_ROWS: usize = 10;
const MAX_LOG_LINES: usize = 200;
const MAX_SPARK_BARS: usize = 36;

/// (offset from "Démarrer" press, event) — the deterministic scripted
/// window. Mirrors checkout_service.rs's phase shapes exactly, just
/// compressed into one fixed pass instead of an infinite repeat.
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
    FuiteActive,      // "leak active"
    GrappeEnCroissance, // "cluster growing"
    RafaleDeTraitement, // "processing burst" / storm
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
            OrderStatus::EnAttente => Color::from_rgb(0.75, 0.65, 0.2),
            OrderStatus::Traitement => Color::from_rgb(0.2, 0.5, 0.85),
            OrderStatus::Terminee => Color::from_rgb(0.2, 0.7, 0.35),
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

    // Run state.
    started: bool,
    run_start: Option<Instant>,
    scripted_done_through: usize, // index into SCHEDULE already fired

    // Leak (owner-allocation pattern preserved — see leak_phase_tick).
    pool_manager: Option<Vec<u8>>,
    leaked_connections_pending: Option<Vec<Vec<u8>>>,
    leaked_connections: Vec<Vec<u8>>,

    // Hot cluster (owner-allocation pattern preserved — see hot_phase_tick).
    queue_owner: Option<Vec<u8>>,
    backlog: Vec<Vec<u8>>,
    hot_extra_added: bool,

    // Storm.
    storm_active: bool,
    storm_bursts_this_run: usize,

    phase: Phase,

    // Display-only state.
    orders: VecDeque<OrderRow>,
    order_seq: u64,
    throughput_history: VecDeque<u32>,
    this_tick_alloc_events: u32,
    log_lines: VecDeque<String>,
}

impl Default for State {
    fn default() -> Self {
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
        };
        s.log("checkout_service_gui starting — request_handler online, payment_gateway_pool warm, order_queue idle");
        s
    }
}

#[derive(Clone, Copy, Debug)]
enum Message {
    Tick(Instant),
    StartPressed,
}

impl State {
    fn log(&mut self, line: impl Into<String>) {
        self.log_lines.push_front(format!("[{:>6}ms] {}", self.app_start.elapsed().as_millis(), line.into()));
        while self.log_lines.len() > MAX_LOG_LINES {
            self.log_lines.pop_back();
        }
    }

    // ── Owner-allocation funnels ────────────────────────────────────────
    // Both call sites for a given owner's children route through the SAME
    // named function so phi's ancestor-frame matching sees a consistent
    // effective site for the owner, exactly like `main`'s loop body does
    // for the console version. Do not split these into separate methods.

    #[inline(never)]
    fn leak_phase_tick(&mut self, release: bool) {
        if !release {
            self.pool_manager = Some(vec![0u8; 256]);
            let connections = payment_gateway_pool_checkout_connections(12);
            self.this_tick_alloc_events += 1 + connections.len() as u32;
            // Held live alongside pool_manager until release, matching the
            // console version's 2s hold before the manager is dropped.
            self.leaked_connections_pending = Some(connections);
        } else if let Some(conns) = self.leaked_connections_pending.take() {
            self.pool_manager = None; // the bug: manager torn down first
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
            self.this_tick_alloc_events += 1 + initial.len() as u32;
            self.backlog.extend(initial);
        }
        if add_extra && !self.hot_extra_added {
            let extra = order_queue_accept_orders(30);
            self.this_tick_alloc_events += extra.len() as u32;
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
        self.this_tick_alloc_events += n as u32;
        self.storm_bursts_this_run += 1;
    }

    #[inline(never)]
    fn ambient_tick(&mut self, n: usize) {
        for i in 0..n {
            request_handler_handle(i);
        }
        self.this_tick_alloc_events += n as u32;
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

    fn next_script_event_countdown(&self) -> Option<(Duration, ScriptEvent)> {
        let run_start = self.run_start?;
        let elapsed_ms = run_start.elapsed().as_millis() as u64;
        SCHEDULE
            .iter()
            .find(|(offset, _)| *offset > elapsed_ms)
            .map(|(offset, ev)| (Duration::from_millis(offset - elapsed_ms), *ev))
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
                ScriptEvent::LeakRelease => {
                    self.leak_phase_tick(true);
                }
                ScriptEvent::HotStart => {
                    self.phase = Phase::GrappeEnCroissance;
                    self.log("EVENT: FLAW — order_queue backlog growing past healthy size (hot cluster)");
                    self.hot_phase_tick(false);
                }
                ScriptEvent::HotGrow => {
                    self.hot_phase_tick(true);
                }
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

    fn update(&mut self, message: Message) {
        match message {
            Message::StartPressed => {
                if !self.started {
                    self.started = true;
                    self.run_start = Some(Instant::now());
                    self.log("=== Démarrer pressé — scénario programmé démarré ===");
                }
            }
            Message::Tick(_now) => {
                self.tick_count += 1;
                self.this_tick_alloc_events = 0;

                // Validation-only hook: HEAPLENS_GUI_AUTOSTART lets an
                // automated wire/soak test drive the scripted window
                // deterministically without a real mouse click. Unset in
                // normal/demo use, so this never affects the presenter path.
                if !self.started && self.tick_count == 2 && std::env::var_os("HEAPLENS_GUI_AUTOSTART").is_some() {
                    self.started = true;
                    self.run_start = Some(Instant::now());
                    self.log("=== HEAPLENS_GUI_AUTOSTART: scénario programmé démarré ===");
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
                    // Ambient healthy traffic — always flowing, both before
                    // "Démarrer" and between/after scripted events, so the
                    // UI never looks frozen.
                    self.ambient_tick(2);
                    if self.tick_count % 6 == 0 {
                        self.spawn_order();
                    }
                }

                if ambient_indefinite {
                    // Jittered, bounded extra traffic. Never accumulates —
                    // orders complete and free at a rate that keeps the
                    // view readable. The leak accumulator is deliberately
                    // exempt: it's supposed to keep growing forever.
                    let jitter = (self.tick_count.wrapping_mul(2654435761) >> 24) % 37;
                    if jitter == 0 {
                        self.ambient_tick(40); // small blip
                    }
                    if jitter == 5 {
                        let mini = order_queue_accept_orders(6);
                        drop(mini); // bounded — freed immediately, no owner link needed
                    }
                }

                self.advance_order_statuses();
                self.throughput_history.push_back(self.this_tick_alloc_events);
                while self.throughput_history.len() > MAX_SPARK_BARS {
                    self.throughput_history.pop_front();
                }
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(TICK_MS)).map(Message::Tick)
    }

    fn view(&self) -> Element<'_, Message> {
        let header = row![
            text("Checkout Ops Console").size(22),
            Space::with_width(Length::Fill),
            container(text("Système opérationnel").color(Color::WHITE))
                .padding([4, 12])
                .style(|_theme: &Theme| container::Style {
                    background: Some(Color::from_rgb(0.15, 0.6, 0.3).into()),
                    text_color: Some(Color::WHITE),
                    border: iced::Border::default().rounded(12),
                    ..Default::default()
                }),
            Space::with_width(16),
            text(format!("{:>6}s", self.app_start.elapsed().as_secs())),
        ]
        .align_y(Alignment::Center)
        .spacing(8)
        .padding(12);

        let orders_col = column(
            std::iter::once(text("Commandes en direct").size(16).into()).chain(
                self.orders.iter().map(|o| {
                    row![
                        text(format!("#{:05}", o.id)).width(Length::Fixed(70.0)),
                        text(o.customer.clone()).width(Length::Fixed(120.0)),
                        text(format!("{:.2} €", o.amount_cents as f32 / 100.0)).width(Length::Fixed(90.0)),
                        text(o.status.label()).color(o.status.color()),
                    ]
                    .spacing(10)
                    .into()
                }),
            ),
        )
        .spacing(4);

        let checked_out = self.leaked_connections.len() + self.pool_manager.as_ref().map(|_| 12).unwrap_or(0);
        let gauge_color = if checked_out == 0 {
            Color::from_rgb(0.2, 0.7, 0.35)
        } else if checked_out < 40 {
            Color::from_rgb(0.8, 0.6, 0.15)
        } else {
            Color::from_rgb(0.8, 0.2, 0.2)
        };
        let pool_gauge = column![
            text("Pool de connexions (payment_gateway_pool)").size(16),
            progress_bar(0.0..=80.0, checked_out.min(80) as f32),
            text(format!("{checked_out} connexions retenues")).color(gauge_color),
        ]
        .spacing(6);

        let spark_bars: Element<'_, Message> = row(self
            .throughput_history
            .iter()
            .map(|&v| {
                let h = (v as f32).min(150.0).max(2.0);
                container(Space::with_height(Length::Fixed(h)))
                    .width(Length::Fixed(6.0))
                    .style(move |_theme: &Theme| container::Style {
                        background: Some(Color::from_rgb(0.3, 0.55, 0.9).into()),
                        ..Default::default()
                    })
                    .into()
            })
            .collect::<Vec<_>>())
        .align_y(Alignment::End)
        .spacing(2)
        .height(Length::Fixed(150.0))
        .into();

        let throughput = column![
            text("Débit (metrics_flush)").size(16),
            container(spark_bars).height(Length::Fixed(154.0)),
        ]
        .spacing(6);

        let log_console: Element<'_, Message> = scrollable(
            column(self.log_lines.iter().map(|l| text(l.clone()).size(12).into())).spacing(2),
        )
        .height(Length::Fixed(160.0))
        .into();

        let left_panel = column![
            orders_col,
            pool_gauge,
            throughput,
            text("Journal").size(16),
            container(log_console).padding(8).style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.08, 0.08, 0.1).into()),
                text_color: Some(Color::from_rgb(0.8, 0.85, 0.8)),
                ..Default::default()
            }),
        ]
        .spacing(16)
        .padding(12)
        .width(Length::FillPortion(3));

        let start_button = if self.started {
            button(text("En cours…")).padding([8, 20])
        } else {
            button(text("Démarrer")).on_press(Message::StartPressed).padding([8, 20]).style(button::success)
        };

        let countdown_text = if self.started {
            match self.next_script_event_countdown() {
                Some((remaining, _)) => format!(
                    "Prochain traitement programmé dans : {}s",
                    remaining.as_secs()
                ),
                None => "Trafic ambiant indéfini en cours".to_string(),
            }
        } else {
            "En attente de démarrage".to_string()
        };

        let right_panel = column![
            text("Contrôleur de test").size(18),
            text("Simule des conditions de charge réalistes sur ce service.").size(12),
            start_button,
            Space::with_height(8),
            text(countdown_text).size(14),
            Space::with_height(8),
            text(format!("Phase : {}", self.phase.label())).size(14).color(
                match self.phase {
                    Phase::Nominal => Color::from_rgb(0.2, 0.7, 0.35),
                    Phase::FuiteActive => Color::from_rgb(0.8, 0.2, 0.2),
                    Phase::GrappeEnCroissance => Color::from_rgb(0.8, 0.6, 0.15),
                    Phase::RafaleDeTraitement => Color::from_rgb(0.75, 0.3, 0.75),
                }
            ),
        ]
        .spacing(10)
        .padding(16)
        .width(Length::FillPortion(1));

        column![
            header,
            row![left_panel, right_panel].height(Length::Fill),
        ]
        .into()
    }
}

fn main() -> iced::Result {
    iced::application("Checkout Ops Console", State::update, State::view)
        .subscription(State::subscription)
        .theme(|_state: &State| Theme::Dark)
        .run()
}
