//! Network probing. Everything here shells out to the system `ping` and
//! `traceroute`, which keeps the app free of raw-socket privileges.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Method, Target};

const PING: &str = "/sbin/ping";
const TRACEROUTE: &str = "/usr/sbin/traceroute";
const ROUTE: &str = "/sbin/route";

/// Why a probe failed. Kept as a type rather than parsed back out of
/// `detail`, because the verdict depends on the distinction: a name that will
/// not resolve is a DNS fault, while a host that never answers is a
/// connectivity fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The name could not be resolved.
    Resolution,
    /// Nothing came back from the host.
    Unreachable,
}

/// One target's result for one probe round.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub target: Target,
    /// Round-trip time, or `None` when the probe failed.
    pub latency_ms: Option<f64>,
    /// Human-readable note: the resolved address, or why it failed.
    pub detail: String,
    /// Set when the probe failed.
    pub failure: Option<Failure>,
}

impl Outcome {
    pub fn is_up(&self) -> bool {
        self.latency_ms.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hop {
    pub hop: u32,
    pub host: String,
    pub latency_ms: Option<f64>,
}

/// Asks the routing table for the default gateway.
pub fn detect_gateway() -> Option<String> {
    let output = Command::new(ROUTE).args(["-n", "get", "default"]).output().ok()?;

    parse_gateway(&String::from_utf8_lossy(&output.stdout))
}

fn parse_gateway(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;

        (key.trim() == "gateway").then(|| value.trim().to_string())
    })
}

/// Echoes sent per probe. One lost echo must not read as an outage: on macOS
/// a LAN host whose ARP entry is being refreshed can fail a lone echo with
/// `recvmsg: No route to host` while answering the very next one.
const ECHOES: &str = "3";

/// Sends up to `ECHOES` ICMP echoes, stopping at the first reply, and returns
/// its round-trip time.
fn ping_once(host: &str, timeout: Duration) -> Result<f64, String> {
    // `-o` exits on the first reply and `-i 0.2` spaces the retries closely,
    // so a healthy host costs one echo. `-W` bounds the wait for a reply,
    // `-t` bounds the whole run so a wedged ping can never stall a probe round.
    let deadline = timeout.as_secs().max(1) + 1;

    let output = Command::new(PING)
        .args([
            "-n",
            "-o",
            "-i",
            "0.2",
            "-c",
            ECHOES,
            "-W",
            &timeout.as_millis().to_string(),
            "-t",
            &deadline.to_string(),
            host,
        ])
        .output()
        .map_err(|error| format!("could not run ping: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if let Some(latency) = parse_ping_time(&stdout) {
        return Ok(latency);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    Err(first_meaningful_line(&stderr)
        .or_else(|| first_meaningful_line(&stdout))
        .unwrap_or_else(|| "no reply".into()))
}

fn parse_ping_time(stdout: &str) -> Option<f64> {
    let (_, rest) = stdout.split_once("time=")?;
    let value = rest.split_whitespace().next()?;

    value.parse().ok()
}

fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with("PING")
                && !line.starts_with("---")
                && !line.contains("packets transmitted")
        })
        .map(ToString::to_string)
}

/// Resolves a host to its first address, timing the lookup. A literal IP
/// address short-circuits, so `Internet` targets never depend on DNS.
fn resolve(host: &str, port: u16) -> Result<(SocketAddr, Option<f64>), String> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok((SocketAddr::new(address, port), None));
    }

    let started = Instant::now();

    let mut addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("DNS lookup failed: {error}"))?;

    let address = addresses.next().ok_or("DNS returned no addresses")?;

    Ok((address, Some(started.elapsed().as_secs_f64() * 1000.0)))
}

/// A successful probe: the round-trip time, plus a note when the peer was
/// reachable in a way worth mentioning.
type Reached = (f64, Option<&'static str>);

/// Completes a TCP handshake and returns how long it took.
///
/// A refused connection still counts as reached. Something sent a packet back,
/// which is exactly what an outage does not do, and treating a closed port as
/// an outage would log false positives every time a middlebox resets a
/// connection.
fn connect_once(address: SocketAddr, timeout: Duration) -> Result<Reached, String> {
    let started = Instant::now();
    let result = TcpStream::connect_timeout(&address, timeout);
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

    match result {
        Ok(stream) => {
            // Nothing is sent; the handshake alone proves reachability.
            let _ = stream.shutdown(std::net::Shutdown::Both);

            Ok((latency_ms, None))
        }
        Err(error) => match error.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                Ok((latency_ms, Some("refused, but the host answered")))
            }
            std::io::ErrorKind::TimedOut => Err("timed out".into()),
            _ => Err(error.to_string()),
        },
    }
}

/// Probes one target. Hostnames are resolved first so a resolution failure is
/// reported as such instead of looking like a routing failure.
fn probe_target(target: &Target, timeout: Duration) -> Outcome {
    let failed = |detail: String, failure: Failure| Outcome {
        target: target.clone(),
        latency_ms: None,
        detail,
        failure: Some(failure),
    };

    let (address, lookup_ms) = match resolve(&target.host, target.port) {
        Ok(resolved) => resolved,
        Err(error) => return failed(error, Failure::Resolution),
    };

    let mut notes = Vec::new();

    if let Some(lookup_ms) = lookup_ms {
        notes.push(format!("{} · lookup {lookup_ms:.0} ms", address.ip()));
    }

    let (attempt, method) = match target.method {
        Method::Tcp => (
            connect_once(address, timeout),
            format!("tcp/{}", target.port),
        ),
        Method::Icmp => (
            ping_once(&address.ip().to_string(), timeout).map(|latency| (latency, None)),
            "icmp".to_string(),
        ),
    };

    notes.push(method);

    match attempt {
        Ok((latency_ms, note)) => {
            notes.extend(note.map(str::to_string));

            Outcome {
                target: target.clone(),
                latency_ms: Some(latency_ms),
                detail: notes.join(" · "),
                failure: None,
            }
        }
        Err(error) => {
            notes.push(error);

            // Resolution already succeeded above, so anything failing here is
            // the host not answering.
            failed(notes.join(" · "), Failure::Unreachable)
        }
    }
}

/// Probes every target concurrently and returns the results in target order.
pub async fn probe_all(targets: Vec<Target>, config: Config) -> Vec<Outcome> {
    let timeout = Duration::from_millis(config.timeout_ms.max(100));

    let handles: Vec<_> = targets
        .into_iter()
        .map(|target| tokio::task::spawn_blocking(move || probe_target(&target, timeout)))
        .collect();

    let mut outcomes = Vec::with_capacity(handles.len());

    for handle in handles {
        match handle.await {
            Ok(outcome) => outcomes.push(outcome),
            // A panicking probe should degrade the round, not kill the app.
            Err(error) => eprintln!("probe task failed: {error}"),
        }
    }

    outcomes
}

fn traceroute_blocking(host: &str, max_hops: u32) -> Vec<Hop> {
    let output = Command::new(TRACEROUTE)
        .args([
            "-n",
            "-q",
            "1",
            "-w",
            "1",
            "-m",
            &max_hops.max(1).to_string(),
            host,
        ])
        .output();

    match output {
        Ok(output) => {
            let mut hops = parse_traceroute(&String::from_utf8_lossy(&output.stdout));

            if hops.is_empty() {
                let stderr = String::from_utf8_lossy(&output.stderr);

                if let Some(line) = first_meaningful_line(&stderr) {
                    hops.push(Hop {
                        hop: 0,
                        host: line,
                        latency_ms: None,
                    });
                }
            }

            hops
        }
        Err(error) => vec![Hop {
            hop: 0,
            host: format!("could not run traceroute: {error}"),
            latency_ms: None,
        }],
    }
}

fn parse_traceroute(stdout: &str) -> Vec<Hop> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let hop = fields.next()?.parse::<u32>().ok()?;
            let host = fields.next()?;

            if host == "*" {
                return Some(Hop {
                    hop,
                    host: "* (no reply)".into(),
                    latency_ms: None,
                });
            }

            // Remaining fields look like `1.234 ms`; take the first number.
            let latency_ms = fields.next().and_then(|value| value.parse::<f64>().ok());

            Some(Hop {
                hop,
                host: host.to_string(),
                latency_ms,
            })
        })
        .collect()
}

/// Traces the path to `host`. Run when an outage is declared, to capture where
/// packets stop making progress.
pub async fn traceroute(host: String, max_hops: u32) -> Vec<Hop> {
    tokio::task::spawn_blocking(move || traceroute_blocking(&host, max_hops))
        .await
        .unwrap_or_else(|error| {
            vec![Hop {
                hop: 0,
                host: format!("traceroute task failed: {error}"),
                latency_ms: None,
            }]
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_addresses_skip_dns() {
        let (address, lookup_ms) = resolve("1.1.1.1", 443).expect("a literal IP always resolves");

        assert_eq!(address.to_string(), "1.1.1.1:443");
        // No lookup happened, so an Internet target cannot be tripped by DNS.
        assert_eq!(lookup_ms, None);
    }

    #[test]
    fn unresolvable_hostnames_report_dns() {
        let error = resolve("no-such-host.invalid", 443).expect_err("must not resolve");

        assert!(error.contains("DNS lookup failed"), "{error}");
    }

    #[test]
    fn reads_gateway_from_route_output() {
        let stdout = "   route to: default\ndestination: default\n       gateway: 192.168.1.1\n";

        assert_eq!(parse_gateway(stdout).as_deref(), Some("192.168.1.1"));
        assert_eq!(parse_gateway("no gateway here"), None);
    }

    #[test]
    fn reads_round_trip_time() {
        let stdout = "64 bytes from 1.1.1.1: icmp_seq=0 ttl=57 time=12.345 ms";

        assert_eq!(parse_ping_time(stdout), Some(12.345));
    }

    #[test]
    fn no_time_when_request_times_out() {
        let stdout = "PING 1.1.1.1 (1.1.1.1): 56 data bytes\nRequest timeout for icmp_seq 0\n";

        assert_eq!(parse_ping_time(stdout), None);
        assert_eq!(
            first_meaningful_line(stdout).as_deref(),
            Some("Request timeout for icmp_seq 0")
        );
    }

    #[test]
    fn reads_hops_including_silent_ones() {
        let stdout = "traceroute to 1.1.1.1 (1.1.1.1), 15 hops max, 40 byte packets\n \
             1  192.168.1.1  2.163 ms\n 2  *\n 3  96.120.1.1  12.345 ms\n";

        let hops = parse_traceroute(stdout);

        assert_eq!(hops.len(), 3);
        assert_eq!(hops[0].host, "192.168.1.1");
        assert_eq!(hops[0].latency_ms, Some(2.163));
        assert_eq!(hops[1].latency_ms, None);
        assert_eq!(hops[2].hop, 3);
    }
}
