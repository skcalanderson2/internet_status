//! Live throughput on the default-route interface, read passively from the
//! kernel's byte counters via `netstat -ib`. Nothing is sent: this shows how
//! much the machine is using right now, not what the line can do.

use std::collections::VecDeque;
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

const NETSTAT: &str = "/usr/sbin/netstat";

/// Samples kept for "peak in the last minute" at the default 5 s interval.
pub const RECENT: usize = 12;

/// Cumulative byte counters for one interface at one moment.
#[derive(Debug, Clone)]
pub struct Counters {
    pub interface: String,
    rx_bytes: u64,
    tx_bytes: u64,
    at: Instant,
}

/// Throughput between two samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    pub interface: String,
    pub down_mbps: f64,
    pub up_mbps: f64,
}

impl Rate {
    /// "↓ 12.4 Mbps  ↑ 820 kbps".
    pub fn headline(&self) -> String {
        format!(
            "↓ {}  ↑ {}",
            format_rate(self.down_mbps),
            format_rate(self.up_mbps)
        )
    }
}

pub fn format_rate(mbps: f64) -> String {
    match mbps {
        mbps if mbps >= 100.0 => format!("{mbps:.0} Mbps"),
        mbps if mbps >= 1.0 => format!("{mbps:.1} Mbps"),
        mbps => format!("{:.0} kbps", mbps * 1000.0),
    }
}

/// The busiest sample in `recent`, by combined throughput.
pub fn peak(recent: &VecDeque<Rate>) -> Option<Rate> {
    recent
        .iter()
        .max_by(|a, b| (a.down_mbps + a.up_mbps).total_cmp(&(b.down_mbps + b.up_mbps)))
        .cloned()
}

/// Rate between two samples, or `None` when they cannot be compared: a
/// different interface, no time elapsed, or counters that went backwards
/// because the interface was reset.
pub fn rate(previous: &Counters, next: &Counters) -> Option<Rate> {
    if previous.interface != next.interface {
        return None;
    }

    let seconds = next.at.checked_duration_since(previous.at)?.as_secs_f64();

    if seconds <= 0.0 {
        return None;
    }

    let rx = next.rx_bytes.checked_sub(previous.rx_bytes)?;
    let tx = next.tx_bytes.checked_sub(previous.tx_bytes)?;

    let mbps = |bytes: u64| bytes as f64 * 8.0 / seconds / 1e6;

    Some(Rate {
        interface: next.interface.clone(),
        down_mbps: mbps(rx),
        up_mbps: mbps(tx),
    })
}

/// Interface names are short alphanumerics (`en0`, `utun3`). Anything else
/// is refused before it reaches `netstat` as an argument.
fn valid_interface(name: &str) -> bool {
    !name.is_empty() && name.len() <= 16 && name.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Reads `Ibytes` and `Obytes` from the interface's link-layer row.
///
/// Columns are counted from the right: the `Address` column is blank for
/// interfaces without a MAC address, which shifts everything left of it.
fn parse_netstat(stdout: &str, interface: &str) -> Option<(u64, u64)> {
    stdout.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();

        if fields.first() != Some(&interface) || !line.contains("<Link#") {
            return None;
        }

        // ... Ipkts Ierrs Ibytes Opkts Oerrs Obytes Coll
        let n = fields.len();

        if n < 8 {
            return None;
        }

        let rx = fields[n - 5].parse().ok()?;
        let tx = fields[n - 2].parse().ok()?;

        Some((rx, tx))
    })
}

fn sample_blocking() -> Result<Counters, String> {
    let interface = crate::probe::detect_interface().ok_or("no default route")?;

    if !valid_interface(&interface) {
        return Err(format!("unexpected interface name {interface:?}"));
    }

    let output = Command::new(NETSTAT)
        .args(["-ib", "-n", "-I", &interface])
        .output()
        .map_err(|error| format!("could not run netstat: {error}"))?;

    let at = Instant::now();

    let (rx_bytes, tx_bytes) = parse_netstat(&String::from_utf8_lossy(&output.stdout), &interface)
        .ok_or_else(|| format!("no counters for {interface}"))?;

    Ok(Counters {
        interface,
        rx_bytes,
        tx_bytes,
        at,
    })
}

pub async fn sample() -> Result<Counters, String> {
    tokio::task::spawn_blocking(sample_blocking)
        .await
        .unwrap_or_else(|error| Err(format!("traffic sample failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const NETSTAT_EN0: &str = "\
Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
en0        1500  <Link#28>   9c:76:0e:7d:bf:a4  1781283     0 2227937324   512678     0  336042891     0
en0        1500  192.168.88    192.168.88.226   1781283     - 2227937324   512678     -  336042891     -
";

    fn counters(interface: &str, rx: u64, tx: u64, at: Instant) -> Counters {
        Counters {
            interface: interface.into(),
            rx_bytes: rx,
            tx_bytes: tx,
            at,
        }
    }

    #[test]
    fn reads_link_row_counters() {
        assert_eq!(
            parse_netstat(NETSTAT_EN0, "en0"),
            Some((2227937324, 336042891))
        );
        assert_eq!(parse_netstat(NETSTAT_EN0, "en1"), None);
    }

    #[test]
    fn reads_counters_without_a_mac_address() {
        let stdout = "\
Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
utun3      1380  <Link#31>                          120     0      45000      130     0      52000     0
";

        assert_eq!(parse_netstat(stdout, "utun3"), Some((45000, 52000)));
    }

    #[test]
    fn computes_megabits_per_second() {
        let start = Instant::now();
        let before = counters("en0", 0, 0, start);
        // 12.5 MB down and 1.25 MB up over 2 s.
        let after = counters("en0", 12_500_000, 1_250_000, start + Duration::from_secs(2));

        let rate = rate(&before, &after).expect("comparable samples");

        assert!((rate.down_mbps - 50.0).abs() < 1e-9);
        assert!((rate.up_mbps - 5.0).abs() < 1e-9);
    }

    #[test]
    fn incomparable_samples_give_no_rate() {
        let start = Instant::now();
        let later = start + Duration::from_secs(5);

        // Switched from Wi-Fi to Ethernet between samples.
        assert_eq!(
            rate(&counters("en0", 0, 0, start), &counters("en1", 9, 9, later)),
            None
        );
        // Counters reset.
        assert_eq!(
            rate(
                &counters("en0", 900, 900, start),
                &counters("en0", 10, 10, later)
            ),
            None
        );
        // No time passed.
        assert_eq!(
            rate(&counters("en0", 0, 0, start), &counters("en0", 9, 9, start)),
            None
        );
    }

    #[test]
    fn refuses_odd_interface_names() {
        assert!(valid_interface("en0"));
        assert!(valid_interface("utun12"));
        assert!(!valid_interface("-a"));
        assert!(!valid_interface(""));
        assert!(!valid_interface("en0 lo0"));
    }

    #[test]
    fn formats_small_rates_in_kilobits() {
        assert_eq!(format_rate(0.82), "820 kbps");
        assert_eq!(format_rate(12.44), "12.4 Mbps");
        assert_eq!(format_rate(456.2), "456 Mbps");
    }

    #[test]
    fn peak_is_the_busiest_sample() {
        let rate = |down: f64| Rate {
            interface: "en0".into(),
            down_mbps: down,
            up_mbps: 1.0,
        };

        let recent: VecDeque<Rate> = [rate(3.0), rate(40.0), rate(9.0)].into();

        assert_eq!(peak(&recent).map(|r| r.down_mbps), Some(40.0));
        assert_eq!(peak(&VecDeque::new()), None);
    }
}
