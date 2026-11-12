//! Outage classification and the append-only outage log.

use std::fmt;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::config::Kind;
use crate::probe::{Failure, Hop, Outcome};

/// Overall verdict for one probe round, ordered from healthy to broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// No probe round has completed yet.
    Unknown,
    /// Everything answered.
    Up,
    /// Some but not all Internet targets answered: packet loss, not an outage.
    Degraded,
    /// Public IPs answer but names do not resolve.
    DnsDown,
    /// The gateway answers and nothing beyond it does.
    InternetDown,
    /// Even the home router is unreachable, so the fault is local.
    LocalNetworkDown,
}

impl Status {
    pub fn is_outage(self) -> bool {
        matches!(self, Self::DnsDown | Self::InternetDown | Self::LocalNetworkDown)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "CHECKING",
            Self::Up => "ONLINE",
            Self::Degraded => "PACKET LOSS",
            Self::DnsDown => "DNS DOWN",
            Self::InternetDown => "INTERNET DOWN",
            Self::LocalNetworkDown => "LAN DOWN",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Unknown => "Waiting for the first probe round",
            Self::Up => "All targets responding",
            Self::Degraded => "Some targets missed replies; connection is usable",
            Self::DnsDown => "Public IPs reachable but name resolution is failing",
            Self::InternetDown => "Router reachable, nothing beyond it is — the fault is upstream",
            Self::LocalNetworkDown => "The router itself is not answering — check Wi-Fi or cabling",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Reduces a round of per-target results to a single verdict.
///
/// The gateway is checked first: if the router is unreachable there is no
/// point blaming the ISP. Public IPs are checked next because they need no
/// DNS, which isolates resolution failures from routing failures.
///
/// A failed gateway probe only means "LAN down" when nothing beyond the router
/// answered either. Every packet to the Internet crosses the router, so a
/// reply from any public target proves the router is forwarding; a missed
/// gateway echo in that round is a lost packet, not a local outage.
pub fn classify(outcomes: &[Outcome]) -> Status {
    if outcomes.is_empty() {
        return Status::Unknown;
    }

    let by_kind = |kind: Kind| {
        outcomes
            .iter()
            .filter(move |outcome| outcome.target.kind == kind)
    };

    let counts = |kind: Kind| {
        let total = by_kind(kind).count();
        let up = by_kind(kind).filter(|outcome| outcome.is_up()).count();

        (up, total)
    };

    let (gateway_up, gateway_total) = counts(Kind::Gateway);

    let beyond_router_answered = outcomes.iter().any(|outcome| {
        matches!(outcome.target.kind, Kind::Internet | Kind::Dns) && outcome.is_up()
    });

    if gateway_total > 0 && gateway_up == 0 && !beyond_router_answered {
        return Status::LocalNetworkDown;
    }

    let (internet_up, internet_total) = counts(Kind::Internet);

    if internet_total > 0 && internet_up == 0 {
        return Status::InternetDown;
    }

    let (dns_up, dns_total) = counts(Kind::Dns);

    // Only blame DNS when resolution itself failed. A hostname target that
    // resolved and then timed out is a connectivity failure that happens to
    // involve a name, and reporting it as "DNS down" sends you looking in the
    // wrong place.
    let resolution_broken = dns_total > 0
        && by_kind(Kind::Dns).all(|outcome| outcome.failure == Some(Failure::Resolution));

    if resolution_broken && internet_up > 0 {
        return Status::DnsDown;
    }

    if gateway_up < gateway_total || internet_up < internet_total || dns_up < dns_total {
        return Status::Degraded;
    }

    Status::Up
}

/// One logged outage. Written once when detected and rewritten with the same
/// `id` when it ends, so an unresolved record means the app stopped mid-outage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outage {
    pub id: String,
    pub status: Status,
    pub started_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
    pub duration_secs: Option<i64>,
    /// Targets that were failing when the outage was declared.
    pub failed_targets: Vec<String>,
    /// Path to the first Internet target, captured at detection time.
    pub traceroute: Vec<Hop>,
    /// Where the trace stopped making progress, if it did.
    pub last_responding_hop: Option<u32>,
    /// The busiest live-traffic sample in the minute before the outage was
    /// declared. Heavy traffic suggests a saturated line rather than a
    /// dead one.
    #[serde(default)]
    pub traffic_before: Option<crate::traffic::Rate>,
}

impl Outage {
    pub fn new(status: Status, outcomes: &[Outcome]) -> Self {
        let started_at = Local::now();

        Self {
            id: format!("{}", started_at.timestamp_millis()),
            status,
            started_at,
            ended_at: None,
            duration_secs: None,
            failed_targets: outcomes
                .iter()
                .filter(|outcome| !outcome.is_up())
                .map(|outcome| format!("{} ({})", outcome.target.name, outcome.detail))
                .collect(),
            traceroute: Vec::new(),
            last_responding_hop: None,
            traffic_before: None,
        }
    }

    /// Closes the outage at `ended_at`, which is the moment connectivity came
    /// back — not the later moment recovery was confirmed.
    pub fn close(&mut self, ended_at: DateTime<Local>) {
        self.duration_secs = Some((ended_at - self.started_at).num_seconds());
        self.ended_at = Some(ended_at);
    }

    pub fn set_traceroute(&mut self, hops: Vec<Hop>) {
        self.last_responding_hop = hops
            .iter()
            .filter(|hop| hop.latency_ms.is_some())
            .map(|hop| hop.hop)
            .max();
        self.traceroute = hops;
    }

    /// Elapsed time, using the current clock while the outage is ongoing.
    pub fn duration(&self) -> chrono::Duration {
        match self.ended_at {
            Some(ended_at) => ended_at - self.started_at,
            None => Local::now() - self.started_at,
        }
    }
}

pub fn format_duration(duration: chrono::Duration) -> String {
    let total = duration.num_seconds().max(0);

    match (total / 3600, (total % 3600) / 60, total % 60) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, seconds) => format!("{minutes}m {seconds}s"),
        (hours, minutes, _) => format!("{hours}h {minutes}m"),
    }
}

pub fn log_path() -> std::path::PathBuf {
    crate::config::data_dir().join("outages.jsonl")
}

/// Appends an outage record as one JSON line.
pub fn append(outage: &Outage) -> std::io::Result<()> {
    use std::io::Write;

    let path = log_path();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let line = serde_json::to_string(outage)
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;

    writeln!(file, "{line}")
}

/// Loads the log, newest first. Later lines win, so a closed outage replaces
/// the open record written when it was detected. Unparsable lines are skipped
/// rather than failing the load.
pub fn load() -> Vec<Outage> {
    let Ok(contents) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };

    let mut outages: Vec<Outage> = Vec::new();

    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Outage>(line) {
            Ok(outage) => match outages.iter_mut().find(|existing| existing.id == outage.id) {
                Some(existing) => *existing = outage,
                None => outages.push(outage),
            },
            Err(error) => eprintln!("skipping malformed log line: {error}"),
        }
    }

    outages.reverse();
    outages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;

    fn outcome(name: &str, kind: Kind, up: bool) -> Outcome {
        Outcome {
            target: Target {
                name: name.into(),
                host: name.into(),
                kind,
                method: crate::config::Method::Tcp,
                port: 443,
            },
            latency_ms: up.then_some(10.0),
            detail: String::new(),
            failure: (!up).then_some(Failure::Unreachable),
        }
    }

    /// A hostname target that resolved and then failed to connect.
    fn unreachable_after_resolving(name: &str) -> Outcome {
        outcome(name, Kind::Dns, false)
    }

    /// A hostname target whose name would not resolve at all.
    fn resolution_failed(name: &str) -> Outcome {
        Outcome {
            failure: Some(Failure::Resolution),
            ..outcome(name, Kind::Dns, false)
        }
    }

    #[test]
    fn all_up_is_up() {
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, true),
            outcome("d", Kind::Dns, true),
        ];

        assert_eq!(classify(&round), Status::Up);
    }

    #[test]
    fn dead_gateway_outranks_everything() {
        let round = [
            outcome("gw", Kind::Gateway, false),
            outcome("a", Kind::Internet, false),
        ];

        assert_eq!(classify(&round), Status::LocalNetworkDown);
    }

    #[test]
    fn missed_gateway_echo_with_internet_answering_is_not_a_lan_outage() {
        // Real log shape: the router ping failed with "No route to host" while
        // every public target answered through that same router.
        let round = [
            outcome("gw", Kind::Gateway, false),
            outcome("a", Kind::Internet, true),
            outcome("b", Kind::Internet, true),
            outcome("d", Kind::Dns, true),
        ];

        assert_eq!(classify(&round), Status::Degraded);
    }

    #[test]
    fn dead_gateway_with_only_a_resolved_hostname_is_still_a_lan_outage() {
        // A name can resolve from the router's or the OS's cache; only an
        // answer from the host itself proves the path beyond the router.
        let round = [
            outcome("gw", Kind::Gateway, false),
            outcome("a", Kind::Internet, false),
            unreachable_after_resolving("d"),
        ];

        assert_eq!(classify(&round), Status::LocalNetworkDown);
    }

    #[test]
    fn live_gateway_dead_internet_is_an_upstream_outage() {
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, false),
            outcome("b", Kind::Internet, false),
            outcome("d", Kind::Dns, false),
        ];

        assert_eq!(classify(&round), Status::InternetDown);
    }

    #[test]
    fn only_dns_failing_is_a_dns_outage() {
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, true),
            resolution_failed("d"),
        ];

        assert_eq!(classify(&round), Status::DnsDown);
    }

    #[test]
    fn a_hostname_that_resolved_then_timed_out_is_not_a_dns_fault() {
        // Four days of real logs labelled 34 outages "DNS down" on this shape.
        // Resolution had succeeded every time; the connection timed out.
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, true),
            unreachable_after_resolving("d"),
        ];

        assert_eq!(classify(&round), Status::Degraded);
    }

    #[test]
    fn dead_internet_outranks_a_failing_hostname() {
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, false),
            resolution_failed("d"),
        ];

        assert_eq!(classify(&round), Status::InternetDown);
    }

    #[test]
    fn partial_loss_is_degraded_not_an_outage() {
        let round = [
            outcome("gw", Kind::Gateway, true),
            outcome("a", Kind::Internet, true),
            outcome("b", Kind::Internet, false),
        ];

        assert_eq!(classify(&round), Status::Degraded);
        assert!(!Status::Degraded.is_outage());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(chrono::Duration::seconds(9)), "9s");
        assert_eq!(format_duration(chrono::Duration::seconds(125)), "2m 5s");
        assert_eq!(format_duration(chrono::Duration::seconds(7300)), "2h 1m");
    }

    #[test]
    fn closing_uses_the_moment_connectivity_returned() {
        let mut entry = Outage::new(Status::InternetDown, &[]);

        entry.started_at = Local::now() - chrono::Duration::seconds(30);
        // Recovery is confirmed later than it happened; the log must record
        // when the connection came back, not when we became sure of it.
        entry.close(entry.started_at + chrono::Duration::seconds(12));

        assert_eq!(entry.duration_secs, Some(12));
    }

    #[test]
    fn traceroute_records_the_last_hop_that_answered() {
        let mut outage = Outage::new(Status::InternetDown, &[]);

        outage.set_traceroute(vec![
            Hop { hop: 1, host: "192.168.1.1".into(), latency_ms: Some(1.0) },
            Hop { hop: 2, host: "10.0.0.1".into(), latency_ms: Some(9.0) },
            Hop { hop: 3, host: "* (no reply)".into(), latency_ms: None },
        ]);

        assert_eq!(outage.last_responding_hop, Some(2));
    }
}
