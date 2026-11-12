//! Menu-bar presence: a coloured status dot with a menu, built on
//! [`tray_icon`].
//!
//! Everything here must run on the main thread, which on macOS is where the
//! status item lives. That is why the tray is created from the first `update`
//! call rather than at boot: by then the application is running and the status
//! bar will accept the item.

use chrono::{DateTime, Local};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::outage::Status;

/// What the user picked from the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ShowWindow,
    ProbeNow,
    SpeedTest,
    TogglePause,
    OpenLog,
    Quit,
}

const ID_SHOW: &str = "show";
const ID_PROBE: &str = "probe";
const ID_SPEED: &str = "speed";
const ID_PAUSE: &str = "pause";
const ID_LOG: &str = "log";
const ID_QUIT: &str = "quit";

/// Icon side in pixels. macOS scales whatever it is given to 18 points tall,
/// so drawing larger keeps it crisp on a Retina display.
const SIZE: u32 = 36;

pub struct Tray {
    icon: TrayIcon,
    status_item: MenuItem,
    today_item: MenuItem,
    speed_item: MenuItem,
    traffic_item: MenuItem,
    speed_run_item: MenuItem,
    pause_item: MenuItem,
    /// Last drawn appearance, so the icon is only rebuilt when it changes.
    drawn: Option<(Status, bool)>,
}

impl Tray {
    /// Creates the status item. Must be called on the main thread.
    pub fn new() -> Result<Self, String> {
        let status_item = MenuItem::new("Starting…", false, None);
        let today_item = MenuItem::new("No outages today", false, None);
        let speed_item = MenuItem::new("No speed test yet", false, None);
        let traffic_item = MenuItem::new("Live traffic: measuring…", false, None);
        let speed_run_item = MenuItem::with_id(ID_SPEED, "Run speed test", true, None);
        let pause_item = MenuItem::with_id(ID_PAUSE, "Pause monitoring", true, None);

        let menu = Menu::new();

        let items: [&dyn tray_icon::menu::IsMenuItem; 12] = [
            &status_item,
            &today_item,
            &traffic_item,
            &speed_item,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_PROBE, "Check now", true, None),
            &speed_run_item,
            &pause_item,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_SHOW, "Show window", true, None),
            &MenuItem::with_id(ID_LOG, "Open log folder", true, None),
            &MenuItem::with_id(ID_QUIT, "Quit Internet Status", true, None),
        ];

        menu.append_items(&items)
            .map_err(|error| format!("could not build the menu: {error}"))?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Internet Status")
            .with_icon(dot(Status::Unknown, false))
            .with_icon_as_template(false)
            .build()
            .map_err(|error| format!("could not create the status item: {error}"))?;

        Ok(Self {
            icon,
            status_item,
            today_item,
            speed_item,
            traffic_item,
            speed_run_item,
            pause_item,
            drawn: None,
        })
    }

    /// Reflects the current state in the menu bar.
    pub fn refresh(&mut self, state: State<'_>) {
        let appearance = (state.status, state.paused);

        if self.drawn != Some(appearance) {
            // `set_icon` only fails on a malformed image, and `dot` always
            // produces a well-formed one.
            let _ = self.icon.set_icon(Some(dot(state.status, state.paused)));
            self.drawn = Some(appearance);
        }

        let headline = if state.paused {
            "Paused".to_string()
        } else {
            match state.latency_ms {
                Some(latency) => format!("{} · {latency:.0} ms", state.status.label()),
                None => state.status.label().to_string(),
            }
        };

        self.status_item.set_text(&headline);
        self.today_item.set_text(state.today);
        self.speed_item.set_text(state.speed);
        self.traffic_item.set_text(state.traffic);
        self.speed_run_item
            .set_enabled(state.speed_available && !state.speed_testing);
        self.pause_item.set_text(if state.paused {
            "Resume monitoring"
        } else {
            "Pause monitoring"
        });

        let checked = match state.last_probe {
            Some(at) => format!("checked {}", at.format("%H:%M:%S")),
            None => "no probe yet".to_string(),
        };

        let _ = self
            .icon
            .set_tooltip(Some(format!("Internet Status — {headline} ({checked})")));
    }
}

/// What the menu bar needs to know about the app.
pub struct State<'a> {
    pub status: Status,
    pub paused: bool,
    pub latency_ms: Option<f64>,
    pub last_probe: Option<DateTime<Local>>,
    /// Pre-formatted summary line for today.
    pub today: &'a str,
    /// Pre-formatted latest speed-test line.
    pub speed: &'a str,
    /// Pre-formatted live-throughput line.
    pub traffic: &'a str,
    pub speed_testing: bool,
    /// `networkQuality` exists; without it the speed items are inert.
    pub speed_available: bool,
}

/// Drains any menu clicks that have happened since the last call.
///
/// `tray_icon` delivers events through a process-wide channel, so this is a
/// poll rather than a stream. Reading it from the app's own tick keeps every
/// tray interaction on the main thread.
pub fn drain() -> Vec<Action> {
    let receiver = MenuEvent::receiver();
    let mut actions = Vec::new();

    while let Ok(event) = receiver.try_recv() {
        let action = match event.id.0.as_str() {
            ID_SHOW => Action::ShowWindow,
            ID_PROBE => Action::ProbeNow,
            ID_SPEED => Action::SpeedTest,
            ID_PAUSE => Action::TogglePause,
            ID_LOG => Action::OpenLog,
            ID_QUIT => Action::Quit,
            _ => continue,
        };

        actions.push(action);
    }

    actions
}

/// Draws the status dot: a filled circle, or a ring when paused.
fn dot(status: Status, paused: bool) -> Icon {
    let (red, green, blue) = colour(status, paused);

    let centre = (SIZE as f32 - 1.0) / 2.0;
    let outer = SIZE as f32 / 2.0 - 2.0;
    // A ring reads as "not watching" at a glance, without another colour.
    // Negative means "no hole", so the disc fills all the way to the centre.
    let inner = if paused { outer - 5.0 } else { -1.0 };

    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);

    for y in 0..SIZE {
        for x in 0..SIZE {
            let distance = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();

            // One pixel of feathering on each edge, so the circle is not
            // visibly stepped at 18 points.
            let alpha = coverage(outer - distance) * coverage(distance - inner);

            rgba.extend_from_slice(&[red, green, blue, (alpha * 255.0) as u8]);
        }
    }

    // `SIZE * SIZE` pixels of 4 bytes is always a valid icon.
    Icon::from_rgba(rgba, SIZE, SIZE).expect("the generated icon is well-formed")
}

/// Antialiasing ramp across one pixel either side of an edge.
fn coverage(distance: f32) -> f32 {
    (distance + 0.5).clamp(0.0, 1.0)
}

fn colour(status: Status, paused: bool) -> (u8, u8, u8) {
    if paused {
        return (142, 142, 147);
    }

    match status {
        Status::Up => (52, 199, 89),
        Status::Degraded => (255, 179, 64),
        Status::Unknown => (142, 142, 147),
        Status::DnsDown | Status::InternetDown | Status::LocalNetworkDown => (255, 69, 58),
    }
}
