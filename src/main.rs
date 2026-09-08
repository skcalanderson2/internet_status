//! Internet Status — a small macOS desktop monitor for intermittent
//! connectivity drops.
//!
//! Every few seconds it pings the home router, two public IP addresses and one
//! hostname. That layering is what makes the diagnosis possible: the router
//! separates local faults from upstream ones, the raw IPs test routing without
//! involving DNS, and the hostname tests resolution. When the Internet targets
//! stop answering, a traceroute is captured so the failing hop is recorded
//! while the outage is still happening, and the whole outage is appended to a
//! JSON Lines log.

mod chart;
mod config;
mod outage;
mod probe;
mod tray;

use std::collections::VecDeque;

use chrono::{DateTime, Local};
use iced::widget::{button, canvas, column, container, row, rule, scrollable, space, text};
use iced::{Center, Color, Element, Fill, Length, Subscription, Task, Theme, window};

use config::{Config, Kind, Method, Target};
use outage::{Outage, Status};
use probe::{Hop, Outcome};

const WINDOW_SIZE: (f32, f32) = (720.0, 780.0);

pub fn main() -> iced::Result {
    // A headless single round, for checking the probes without opening a
    // window — handy when the GUI and the shell disagree about what is
    // reachable.
    if std::env::args().any(|argument| argument == "--probe") {
        probe_once();

        return Ok(());
    }

    iced::application(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size(WINDOW_SIZE)
        .antialiasing(true)
        // Closing the window leaves the app in the menu bar; Quit there is
        // the way out.
        .exit_on_close_request(false)
        .run()
}

/// Live state for one probe target.
struct TargetState {
    target: Target,
    latency_ms: Option<f64>,
    detail: String,
    history: VecDeque<Option<f64>>,
    probes: u64,
    failures: u64,
}

impl TargetState {
    fn new(target: Target) -> Self {
        Self {
            target,
            latency_ms: None,
            detail: String::new(),
            history: VecDeque::new(),
            probes: 0,
            failures: 0,
        }
    }

    fn record(&mut self, outcome: &Outcome, history_len: usize) {
        self.latency_ms = outcome.latency_ms;
        self.detail = outcome.detail.clone();
        self.probes += 1;

        if !outcome.is_up() {
            self.failures += 1;
        }

        self.history.push_back(outcome.latency_ms);

        while self.history.len() > history_len.max(2) {
            self.history.pop_front();
        }
    }

    fn loss_percent(&self) -> f64 {
        if self.probes == 0 {
            return 0.0;
        }

        self.failures as f64 * 100.0 / self.probes as f64
    }
}

struct App {
    config: Config,
    targets: Vec<TargetState>,
    status: Status,
    /// Consecutive rounds that looked like an outage, against
    /// `config.fail_threshold`.
    bad_rounds: u32,
    /// Consecutive clean rounds, against `config.recover_threshold`.
    good_rounds: u32,
    /// When the first clean round arrived, which is when the connection
    /// actually came back. Recovery is only *confirmed* later.
    recovering_since: Option<DateTime<Local>>,
    /// The outage in progress, if any.
    current: Option<Outage>,
    /// A traceroute started on the first bad round, before the outage was
    /// declared. Attached once it is.
    pending_trace: Option<Vec<Hop>>,
    /// Identifies the current traceroute so a late one from an episode that
    /// already recovered is discarded instead of mislabelling the next outage.
    trace_seq: u64,
    /// Logged outages, newest first.
    outages: Vec<Outage>,
    /// Id of the outage whose traceroute is expanded in the history list.
    expanded: Option<String>,
    probing: bool,
    paused: bool,
    /// The menu-bar item. Created on the first update, on the main thread.
    tray: Option<tray::Tray>,
    last_probe: Option<DateTime<Local>>,
    notice: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    /// The interval elapsed.
    Tick,
    /// A probe round finished.
    Probed(Vec<Outcome>),
    /// A traceroute finished, tagged with the generation it was started for.
    Traced(u64, Vec<Hop>),
    ProbeNow,
    TogglePause,
    /// Show or hide an outage's traceroute.
    Expand(Option<String>),
    RevealLog,
    /// Redraws the ongoing-outage timer without changing anything.
    Redraw,
    /// Time to read any menu-bar clicks.
    PollTray,
    /// The window's close button was pressed.
    CloseRequested,
    /// Bring the window back from the menu bar.
    ShowWindow,
    Quit,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let (config, mut notice) = Config::load();

        // The gateway is discovered rather than configured, so the app keeps
        // working on a different network without an edit.
        let gateway = if config.gateway.trim().is_empty() {
            probe::detect_gateway()
        } else {
            Some(config.gateway.trim().to_string())
        };

        let mut targets: Vec<TargetState> = Vec::new();

        match gateway {
            Some(host) => targets.push(TargetState::new(Target::gateway(host))),
            None => {
                notice.get_or_insert_with(|| {
                    "Could not find a default gateway; local faults cannot be told apart \
                     from upstream ones."
                        .into()
                });
            }
        }

        targets.extend(config.targets.iter().cloned().map(TargetState::new));

        let app = Self {
            config,
            targets,
            status: Status::Unknown,
            bad_rounds: 0,
            good_rounds: 0,
            recovering_since: None,
            current: None,
            pending_trace: None,
            trace_seq: 0,
            outages: outage::load(),
            expanded: None,
            probing: false,
            paused: false,
            tray: None,
            last_probe: None,
            notice,
        };

        (app, Task::done(Message::ProbeNow))
    }

    fn title(&self) -> String {
        format!("Internet Status — {}", self.status.label())
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::new();

        if !self.paused {
            subscriptions
                .push(iced::time::every(self.config.interval()).map(|_| Message::Tick));
        }

        // Keeps the "down for 42s" counter moving between probe rounds.
        if self.current.is_some() {
            subscriptions.push(
                iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Redraw),
            );
        }

        // `tray_icon` posts menu clicks to a process-wide channel, so they are
        // collected on a tick instead of arriving as a stream.
        subscriptions.push(
            iced::time::every(std::time::Duration::from_millis(150)).map(|_| Message::PollTray),
        );

        subscriptions.push(window::close_requests().map(|_| Message::CloseRequested));

        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // The status item has to be created on the main thread once the app is
        // running, which is exactly where `update` is called from.
        if self.tray.is_none() {
            match tray::Tray::new() {
                Ok(tray) => self.tray = Some(tray),
                Err(error) => {
                    // The window is still perfectly usable without it.
                    self.notice = Some(format!("no menu-bar icon: {error}"));
                }
            }
        }

        let task = self.handle(message);

        self.refresh_tray();

        task
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Redraw => Task::none(),

            Message::PollTray => {
                let tasks: Vec<_> = tray::drain()
                    .into_iter()
                    .map(|action| match action {
                        tray::Action::ShowWindow => Task::done(Message::ShowWindow),
                        tray::Action::ProbeNow => Task::done(Message::ProbeNow),
                        tray::Action::TogglePause => Task::done(Message::TogglePause),
                        tray::Action::OpenLog => Task::done(Message::RevealLog),
                        tray::Action::Quit => Task::done(Message::Quit),
                    })
                    .collect();

                Task::batch(tasks)
            }

            // Hide rather than exit, so monitoring continues from the menu bar.
            Message::CloseRequested => {
                window::latest().and_then(|id| window::set_mode(id, window::Mode::Hidden))
            }

            Message::ShowWindow => window::latest().and_then(|id| {
                Task::batch([
                    window::set_mode(id, window::Mode::Windowed),
                    window::gain_focus(id),
                ])
            }),

            Message::Quit => iced::exit(),

            Message::Tick | Message::ProbeNow => {
                // Skip the round if the previous one is still running, which
                // happens when every target is timing out.
                if self.probing {
                    return Task::none();
                }

                self.probing = true;

                let targets = self.targets.iter().map(|state| state.target.clone()).collect();

                Task::perform(
                    probe::probe_all(targets, self.config.clone()),
                    Message::Probed,
                )
            }

            Message::Probed(outcomes) => {
                self.probing = false;
                self.last_probe = Some(Local::now());
                self.apply(&outcomes)
            }

            Message::Traced(seq, hops) => {
                // A trace from an episode that has already recovered says
                // nothing about the next one.
                if seq != self.trace_seq {
                    return Task::none();
                }

                match &mut self.current {
                    Some(outage) => {
                        outage.set_traceroute(hops);

                        // Re-append so the hops survive a restart; the loader
                        // keeps the last record for each id.
                        let snapshot = outage.clone();

                        self.note_io(outage::append(&snapshot));
                        self.mirror_into_history(&snapshot);
                        self.expanded = Some(snapshot.id);
                    }
                    // Still below `fail_threshold`. Hold the hops until the
                    // outage is declared, or drop them if it never is.
                    None => self.pending_trace = Some(hops),
                }

                Task::none()
            }

            Message::TogglePause => {
                self.paused = !self.paused;
                Task::none()
            }

            Message::Expand(id) => {
                self.expanded = id;
                Task::none()
            }

            Message::RevealLog => {
                // Fire and forget: Finder either opens or it does not, and a
                // failure here should not disturb monitoring.
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg(config::data_dir())
                    .spawn();

                Task::none()
            }
        }
    }

    /// Folds a finished probe round into the state and drives the outage
    /// state machine.
    fn apply(&mut self, outcomes: &[Outcome]) -> Task<Message> {
        let history_len = self.config.history_len;

        for outcome in outcomes {
            if let Some(state) = self
                .targets
                .iter_mut()
                .find(|state| state.target.name == outcome.target.name)
            {
                state.record(outcome, history_len);
            }
        }

        self.status = outage::classify(outcomes);

        if self.status.is_outage() {
            self.bad_rounds += 1;
            self.good_rounds = 0;
            self.recovering_since = None;

            // Trace on the very first bad round. Waiting for the threshold
            // means the trace runs after a short outage is already over: of
            // the logged outages lasting 15s or less, 95% captured a healthy
            // path because the trace started too late to see anything.
            let trace = if self.bad_rounds == 1 {
                self.start_trace()
            } else {
                Task::none()
            };

            if self.current.is_none() && self.bad_rounds >= self.config.fail_threshold.max(1) {
                let mut outage = Outage::new(self.status, outcomes);

                if let Some(hops) = self.pending_trace.take() {
                    outage.set_traceroute(hops);
                }

                self.note_io(outage::append(&outage));
                self.outages.insert(0, outage.clone());
                self.current = Some(outage);
            }

            return trace;
        }

        self.bad_rounds = 0;
        self.good_rounds += 1;

        // The connection came back on the *first* clean round; the rounds
        // after it only confirm that it stayed back.
        let returned_at = *self.recovering_since.get_or_insert_with(Local::now);

        if self.good_rounds < self.config.recover_threshold.max(1) {
            return Task::none();
        }

        self.recovering_since = None;
        // Any trace still in flight belongs to an episode that is over.
        self.trace_seq += 1;
        self.pending_trace = None;

        if let Some(mut outage) = self.current.take() {
            outage.close(returned_at);

            self.note_io(outage::append(&outage));
            self.mirror_into_history(&outage);
        }

        Task::none()
    }

    /// Starts a traceroute for the current episode.
    fn start_trace(&mut self) -> Task<Message> {
        let Some(host) = self.trace_host() else {
            return Task::none();
        };

        self.trace_seq += 1;
        self.pending_trace = None;

        let seq = self.trace_seq;
        let max_hops = self.config.traceroute_max_hops;

        Task::perform(probe::traceroute(host, max_hops), move |hops| {
            Message::Traced(seq, hops)
        })
    }

    /// The address a traceroute should aim at: the first Internet target, or
    /// any target if none is configured.
    fn trace_host(&self) -> Option<String> {
        self.targets
            .iter()
            .find(|state| state.target.kind == Kind::Internet)
            .or_else(|| self.targets.last())
            .map(|state| state.target.host.clone())
    }

    /// Keeps the history list in step with an updated outage record.
    fn mirror_into_history(&mut self, outage: &Outage) {
        match self
            .outages
            .iter_mut()
            .find(|existing| existing.id == outage.id)
        {
            Some(existing) => *existing = outage.clone(),
            None => self.outages.insert(0, outage.clone()),
        }
    }

    fn refresh_tray(&mut self) {
        let today = self.today_summary();
        let latency_ms = self.charted().and_then(|state| state.latency_ms);

        let state = tray::State {
            status: self.status,
            paused: self.paused,
            latency_ms,
            last_probe: self.last_probe,
            today: &today,
        };

        if let Some(tray) = &mut self.tray {
            tray.refresh(state);
        }
    }

    /// "3 outages today · 3.7 min down", for the menu.
    fn today_summary(&self) -> String {
        let today = Local::now().date_naive();

        let entries: Vec<_> = self
            .outages
            .iter()
            .filter(|entry| entry.started_at.date_naive() == today)
            .collect();

        if entries.is_empty() {
            return "No outages today".into();
        }

        let seconds: i64 = entries.iter().map(|entry| entry.duration().num_seconds()).sum();
        let plural = if entries.len() == 1 { "" } else { "s" };

        format!(
            "{} outage{plural} today · {} down",
            entries.len(),
            outage::format_duration(chrono::Duration::seconds(seconds))
        )
    }

    fn note_io(&mut self, result: std::io::Result<()>) {
        if let Err(error) = result {
            self.notice = Some(format!("could not write the outage log: {error}"));
        }
    }

    /// The target whose latency the chart shows: the first Internet target,
    /// since it is the one that defines an outage.
    fn charted(&self) -> Option<&TargetState> {
        self.targets
            .iter()
            .find(|state| state.target.kind == Kind::Internet)
            .or_else(|| self.targets.first())
    }

    fn view(&self) -> Element<'_, Message> {
        let footer = text("Closing this window keeps monitoring in the menu bar. Quit from there.")
            .size(10)
            .style(text::secondary);

        let body = column![
            self.header(),
            rule::horizontal(1),
            self.targets_panel(),
            self.chart_panel(),
            self.history_panel(),
            footer,
        ]
        .spacing(14)
        .padding(16);

        let mut screen = column![].spacing(0).height(Fill);

        if let Some(notice) = &self.notice {
            screen = screen.push(banner(notice, Tone::Warning));
        }

        screen.push(body).into()
    }

    fn header(&self) -> Element<'_, Message> {
        let tone = Tone::of(self.status);

        let badge = container(text(self.status.label()).size(20))
            .padding([10, 16])
            .style(move |theme: &Theme| {
                let pair = tone.pair(theme);

                container::Style {
                    background: Some(pair.color.into()),
                    text_color: Some(pair.text),
                    border: iced::border::rounded(8),
                    ..container::Style::default()
                }
            });

        let checked = match self.last_probe {
            Some(at) => format!("Last checked {}", at.format("%H:%M:%S")),
            None => "No probe has finished yet".into(),
        };

        let summary = column![
            text(self.status.description()).size(13),
            text(checked).size(11).style(text::secondary),
        ]
        .spacing(3);

        let controls = row![
            button(text("Check now").size(12))
                .on_press(Message::ProbeNow)
                .style(button::secondary),
            button(text(if self.paused { "Resume" } else { "Pause" }).size(12))
                .on_press(Message::TogglePause)
                .style(button::secondary),
            button(text("Open log").size(12))
                .on_press(Message::RevealLog)
                .style(button::secondary),
        ]
        .spacing(6);

        let mut header = row![badge, summary, space::horizontal(), controls]
            .spacing(14)
            .align_y(Center);

        if self.probing {
            header = header.push(text("…").size(16).style(text::secondary));
        }

        let mut top = column![header].spacing(12);

        if let Some(entry) = &self.current {
            top = top.push(self.ongoing_banner(entry));
        }

        top.into()
    }

    /// The banner for the outage in progress. While recovery is being
    /// confirmed the connection is already back, and saying so avoids looking
    /// like the app is stuck reporting an outage that has ended.
    fn ongoing_banner(&self, entry: &Outage) -> Element<'_, Message> {
        match self.recovering_since {
            Some(returned_at) => {
                let needed = self.config.recover_threshold.max(1);
                let back_for = outage::format_duration(Local::now() - returned_at);

                banner(
                    &format!(
                        "Recovering — back for {back_for}, confirming ({} of {needed} clean rounds)",
                        self.good_rounds.min(needed)
                    ),
                    Tone::Warning,
                )
            }
            None => banner(
                &format!(
                    "{} for {} — started {}",
                    entry.status.label(),
                    outage::format_duration(entry.duration()),
                    entry.started_at.format("%H:%M:%S")
                ),
                Tone::Bad,
            ),
        }
    }

    fn targets_panel(&self) -> Element<'_, Message> {
        let rows = self.targets.iter().map(|state| {
            let tone = if state.latency_ms.is_some() {
                Tone::Good
            } else {
                Tone::Bad
            };

            let dot = container(space::Space::new().width(9).height(9))
                .style(move |theme: &Theme| container::Style {
                    background: Some(tone.pair(theme).color.into()),
                    border: iced::border::rounded(5),
                    ..container::Style::default()
                });

            let reading = match state.latency_ms {
                Some(latency) => text(format!("{latency:.1} ms")).size(12),
                None => text("no reply").size(12).style(text::danger),
            };

            row![
                dot,
                column![
                    text(&state.target.name).size(13),
                    text(format!("{} · {}", state.target.host, describe(&state.target)))
                        .size(10)
                        .style(text::secondary),
                ]
                .spacing(2)
                .width(Length::FillPortion(2)),
                column![
                    reading,
                    text(format!("{:.1}% loss over {} probes", state.loss_percent(), state.probes))
                        .size(10)
                        .style(text::secondary),
                ]
                .spacing(2)
                .width(Length::FillPortion(1)),
                text(&state.detail)
                    .size(10)
                    .style(text::secondary)
                    .width(Length::FillPortion(2)),
            ]
            .spacing(10)
            .align_y(Center)
            .into()
        });

        container(column(rows).spacing(10))
            .padding(12)
            .width(Fill)
            .style(container::bordered_box)
            .into()
    }

    fn chart_panel(&self) -> Element<'_, Message> {
        let Some(state) = self.charted() else {
            return space::Space::new().into();
        };

        let samples: Vec<Option<f64>> = state.history.iter().copied().collect();

        let chart = canvas(ChartOwner {
            samples,
            capacity: self.config.history_len,
            label: format!("{} latency", state.target.name),
        })
        .width(Fill)
        .height(140);

        container(chart)
            .padding(8)
            .width(Fill)
            .style(container::bordered_box)
            .into()
    }

    fn history_panel(&self) -> Element<'_, Message> {
        let heading = row![
            text(format!("Outage log ({})", self.outages.len())).size(14),
            space::horizontal(),
            text(outage::log_path().display().to_string())
                .size(10)
                .style(text::secondary),
        ]
        .align_y(Center);

        let list: Element<'_, Message> = if self.outages.is_empty() {
            text("No outages recorded yet.")
                .size(12)
                .style(text::secondary)
                .into()
        } else {
            let entries = self.outages.iter().map(|outage| self.outage_row(outage));

            scrollable(column(entries).spacing(8)).height(Fill).into()
        };

        container(column![heading, rule::horizontal(1), list].spacing(8))
            .padding(12)
            .width(Fill)
            .height(Fill)
            .style(container::bordered_box)
            .into()
    }

    fn outage_row<'a>(&'a self, entry: &'a Outage) -> Element<'a, Message> {
        let expanded = self.expanded.as_deref() == Some(entry.id.as_str());

        let when = entry.started_at.format("%Y-%m-%d %H:%M:%S").to_string();
        let ended = match entry.ended_at {
            Some(at) => format!("recovered {}", at.format("%H:%M:%S")),
            None if self.current.as_ref().is_some_and(|open| open.id == entry.id) => {
                "ongoing".into()
            }
            // An open record from a previous run: the app stopped before the
            // outage was seen to end.
            None => "end unknown (app stopped)".into(),
        };

        let toggle = button(
            text(if expanded { "Hide path" } else { "Show path" }).size(11),
        )
        .on_press(Message::Expand(
            (!expanded).then(|| entry.id.clone()),
        ))
        .style(button::text);

        let summary = row![
            text(entry.status.label()).size(12).style(text::danger),
            column![
                text(when).size(12),
                text(ended).size(10).style(text::secondary),
            ]
            .spacing(2)
            .width(Length::FillPortion(2)),
            text(outage::format_duration(entry.duration()))
                .size(12)
                .width(Length::FillPortion(1)),
            space::horizontal(),
            toggle,
        ]
        .spacing(10)
        .align_y(Center);

        let mut block = column![summary].spacing(6);

        if expanded {
            block = block.push(details(entry));
        }

        container(block)
            .padding(8)
            .width(Fill)
            .style(container::rounded_box)
            .into()
    }
}

/// Owns its samples so the canvas program can outlive the borrow of `App`.
struct ChartOwner {
    samples: Vec<Option<f64>>,
    capacity: usize,
    label: String,
}

impl canvas::Program<Message> for ChartOwner {
    type State = ();

    fn draw(
        &self,
        state: &(),
        renderer: &iced::Renderer,
        theme: &Theme,
        bounds: iced::Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let program = chart::Latency {
            samples: &self.samples,
            capacity: self.capacity,
            label: &self.label,
        };

        canvas::Program::<Message>::draw(&program, state, renderer, theme, bounds, cursor)
    }
}

fn details(entry: &Outage) -> Element<'_, Message> {
    let failed = if entry.failed_targets.is_empty() {
        text("No failing targets were recorded.").size(11).style(text::secondary)
    } else {
        text(format!("Failed: {}", entry.failed_targets.join(", ")))
            .size(11)
            .style(text::secondary)
    };

    let path: Element<'_, Message> = if entry.traceroute.is_empty() {
        text("No traceroute was captured.").size(11).style(text::secondary).into()
    } else {
        let hops = entry.traceroute.iter().map(|hop| {
            let timing = match hop.latency_ms {
                Some(latency) => format!("{latency:.1} ms"),
                None => "no reply".into(),
            };

            let line = text(format!("{:>2}  {:<22} {timing}", hop.hop, hop.host))
                .size(11)
                .font(iced::Font::MONOSPACE);

            if hop.latency_ms.is_some() {
                line.into()
            } else {
                line.style(text::danger).into()
            }
        });

        column(hops).spacing(2).into()
    };

    let verdict = match entry.last_responding_hop {
        Some(hop) => text(format!(
            "Packets stopped after hop {hop} — that is the last device that answered."
        ))
        .size(11),
        None => text("Nothing along the path answered.").size(11),
    };

    column![failed, verdict, path].spacing(6).into()
}

fn banner<'a>(message: &str, tone: Tone) -> Element<'a, Message> {
    container(text(message.to_string()).size(12))
        .padding([8, 12])
        .width(Fill)
        .style(move |theme: &Theme| {
            let pair = tone.pair(theme);

            container::Style {
                background: Some(Color { a: 0.18, ..pair.color }.into()),
                border: iced::border::rounded(6),
                ..container::Style::default()
            }
        })
        .into()
}

/// Maps a status onto the theme's semantic colors.
#[derive(Debug, Clone, Copy)]
enum Tone {
    Good,
    Warning,
    Bad,
    Neutral,
}

impl Tone {
    fn of(status: Status) -> Self {
        match status {
            Status::Up => Self::Good,
            Status::Degraded => Self::Warning,
            Status::Unknown => Self::Neutral,
            _ => Self::Bad,
        }
    }

    fn pair(self, theme: &Theme) -> iced::theme::palette::Pair {
        let palette = theme.extended_palette();

        match self {
            Self::Good => palette.success.base,
            Self::Warning => palette.warning.base,
            Self::Bad => palette.danger.base,
            Self::Neutral => palette.secondary.base,
        }
    }
}

/// One line explaining what a target proves, so the window is readable
/// without consulting the config file.
fn describe(target: &Target) -> String {
    let role = match target.kind {
        Kind::Gateway => "local gateway",
        Kind::Internet => "Internet, no DNS",
        Kind::Dns => "needs DNS",
    };

    let method = match target.method {
        Method::Tcp => format!("tcp/{}", target.port),
        Method::Icmp => "icmp".into(),
    };

    format!("{role} · {method}")
}

/// Runs one probe round and prints it, then a traceroute if anything upstream
/// is failing. Used by `--probe`.
fn probe_once() {
    let (config, notice) = Config::load();

    if let Some(notice) = notice {
        eprintln!("note: {notice}");
    }

    let mut targets = Vec::new();

    match probe::detect_gateway() {
        Some(host) => targets.push(Target::gateway(host)),
        None => eprintln!("note: no default gateway found"),
    }

    targets.extend(config.targets.iter().cloned());

    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start a runtime: {error}");
            return;
        }
    };

    let outcomes = runtime.block_on(probe::probe_all(targets, config.clone()));

    for outcome in &outcomes {
        let reading = match outcome.latency_ms {
            Some(latency) => format!("{latency:8.1} ms"),
            None => "  no reply".into(),
        };

        println!(
            "{:<14} {:<18} {reading}  {}",
            outcome.target.name, outcome.target.host, outcome.detail
        );
    }

    let status = outage::classify(&outcomes);

    println!("\n{}: {}", status.label(), status.description());

    if status.is_outage() {
        let host = outcomes
            .iter()
            .find(|outcome| outcome.target.kind == Kind::Internet)
            .map(|outcome| outcome.target.host.clone());

        if let Some(host) = host {
            println!("\ntraceroute to {host}:");

            for hop in runtime.block_on(probe::traceroute(host, config.traceroute_max_hops)) {
                match hop.latency_ms {
                    Some(latency) => println!("{:>3}  {:<22} {latency:.1} ms", hop.hop, hop.host),
                    None => println!("{:>3}  {}", hop.hop, hop.host),
                }
            }
        }
    }
}
