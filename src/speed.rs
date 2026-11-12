//! Throughput tests, run through Apple's `networkQuality` (macOS 12+).
//!
//! A test saturates the link in both directions for up to
//! `speedtest_max_secs` and moves hundreds of megabytes, so it only runs on
//! request or on a slow, opt-in schedule, never once per probe round.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

const NETWORK_QUALITY: &str = "/usr/bin/networkQuality";

/// Extra time allowed past `-M` before the process is killed. `-M` bounds
/// the measurement phases, but setup and teardown come on top of it.
const GRACE: Duration = Duration::from_secs(30);

/// One finished speed test, as logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedTest {
    pub at: DateTime<Local>,
    pub down_mbps: Option<f64>,
    pub up_mbps: Option<f64>,
    /// Round trips per minute while the link is loaded. Higher is better;
    /// a low figure with high throughput is bufferbloat.
    pub responsiveness_rpm: Option<f64>,
    /// Latency to the test server before the link was loaded.
    pub base_rtt_ms: Option<f64>,
    pub interface: Option<String>,
    pub endpoint: Option<String>,
    /// Bytes moved in both directions, so the data cost is visible.
    pub bytes_transferred: Option<u64>,
    /// Set when the test could not run or produced nothing usable.
    pub error: Option<String>,
}

impl SpeedTest {
    fn failed(error: String) -> Self {
        Self {
            at: Local::now(),
            down_mbps: None,
            up_mbps: None,
            responsiveness_rpm: None,
            base_rtt_ms: None,
            interface: None,
            endpoint: None,
            bytes_transferred: None,
            error: Some(error),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none() && (self.down_mbps.is_some() || self.up_mbps.is_some())
    }

    /// "↓ 444 Mbps  ↑ 50 Mbps", or the error.
    pub fn headline(&self) -> String {
        match &self.error {
            Some(error) => format!("failed: {error}"),
            None => format!(
                "↓ {}  ↑ {}",
                format_mbps(self.down_mbps),
                format_mbps(self.up_mbps)
            ),
        }
    }

    /// Responsiveness, idle latency, interface and data used, for a second line.
    pub fn details(&self) -> String {
        let mut parts = Vec::new();

        if let Some(rpm) = self.responsiveness_rpm {
            parts.push(format!("{rpm:.0} RPM under load"));
        }

        if let Some(rtt) = self.base_rtt_ms {
            parts.push(format!("idle {rtt:.0} ms"));
        }

        if let Some(interface) = &self.interface {
            parts.push(interface.clone());
        }

        if let Some(bytes) = self.bytes_transferred {
            parts.push(format!("{:.0} MB used", bytes as f64 / 1_000_000.0));
        }

        parts.join(" · ")
    }
}

pub fn format_mbps(value: Option<f64>) -> String {
    match value {
        Some(mbps) if mbps >= 100.0 => format!("{mbps:.0} Mbps"),
        Some(mbps) => format!("{mbps:.1} Mbps"),
        None => "—".into(),
    }
}

/// Whether this macOS ships `networkQuality` (12 and later).
pub fn available() -> bool {
    Path::new(NETWORK_QUALITY).exists()
}

/// The subset of `networkQuality -c` output the app uses. Every field is
/// optional because the format is undocumented and has changed between
/// macOS releases; a missing field shows as "—" rather than failing the test.
#[derive(Debug, Deserialize)]
struct Report {
    /// Bits per second.
    dl_throughput: Option<f64>,
    /// Bits per second.
    ul_throughput: Option<f64>,
    responsiveness: Option<f64>,
    base_rtt: Option<f64>,
    interface_name: Option<String>,
    test_endpoint: Option<String>,
    dl_bytes_transferred: Option<u64>,
    ul_bytes_transferred: Option<u64>,
    error_code: Option<i64>,
    error_domain: Option<String>,
}

fn parse(stdout: &str) -> Result<SpeedTest, String> {
    let report: Report = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("unreadable networkQuality output ({error})"))?;

    if let Some(code) = report.error_code {
        return Err(match report.error_domain {
            Some(domain) => format!("networkQuality error {domain} {code}"),
            None => format!("networkQuality error {code}"),
        });
    }

    let to_mbps = |bits: Option<f64>| bits.filter(|bits| *bits > 0.0).map(|bits| bits / 1e6);

    let bytes = match (report.dl_bytes_transferred, report.ul_bytes_transferred) {
        (None, None) => None,
        (down, up) => Some(down.unwrap_or(0) + up.unwrap_or(0)),
    };

    let test = SpeedTest {
        at: Local::now(),
        down_mbps: to_mbps(report.dl_throughput),
        up_mbps: to_mbps(report.ul_throughput),
        responsiveness_rpm: report.responsiveness,
        base_rtt_ms: report.base_rtt,
        interface: report.interface_name,
        endpoint: report.test_endpoint,
        bytes_transferred: bytes,
        error: None,
    };

    if test.down_mbps.is_none() && test.up_mbps.is_none() {
        return Err("networkQuality reported no throughput".into());
    }

    Ok(test)
}

/// Runs one test, killing it if it overruns `max_secs` plus a grace period.
fn run_blocking(max_secs: u64) -> SpeedTest {
    let max_secs = max_secs.clamp(5, 120);

    let child = Command::new(NETWORK_QUALITY)
        .args(["-c", "-M", &max_secs.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(error) => return SpeedTest::failed(format!("could not run networkQuality: {error}")),
    };

    // Drain stdout on its own thread. The JSON carries per-request latency
    // arrays and can outgrow a pipe buffer, which would otherwise stall the
    // child while this thread waits for it to exit.
    let mut stdout_pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();

        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buffer);
        }

        buffer
    });

    let deadline = Instant::now() + Duration::from_secs(max_secs) + GRACE;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(error) => break Err(error.to_string()),
        }
    };

    let stdout = reader.join().unwrap_or_default();

    let mut stderr = String::new();

    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }

    if let Err(error) = status {
        return SpeedTest::failed(error);
    }

    match parse(&stdout) {
        Ok(test) => test,
        Err(error) => {
            let reason = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(ToString::to_string)
                .unwrap_or(error);

            SpeedTest::failed(reason)
        }
    }
}

pub async fn run(max_secs: u64) -> SpeedTest {
    tokio::task::spawn_blocking(move || run_blocking(max_secs))
        .await
        .unwrap_or_else(|error| SpeedTest::failed(format!("speed test task failed: {error}")))
}

pub fn log_path() -> std::path::PathBuf {
    crate::config::data_dir().join("speedtests.jsonl")
}

/// Appends a result as one JSON line.
pub fn append(test: &SpeedTest) -> std::io::Result<()> {
    use std::io::Write;

    let path = log_path();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let line =
        serde_json::to_string(test).map_err(|error| std::io::Error::other(error.to_string()))?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    writeln!(file, "{line}")
}

/// Loads up to `keep` of the most recent results, newest first. Unparsable
/// lines are skipped.
pub fn load(keep: usize) -> Vec<SpeedTest> {
    let Ok(contents) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };

    let mut tests: Vec<SpeedTest> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    tests.reverse();
    tests.truncate(keep);
    tests
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `networkQuality -c` run on macOS 27.2.
    const SAMPLE: &str = r#"
    {
      "base_rtt" : 27.09608268737793,
      "dl_bytes_transferred" : 579530551,
      "dl_throughput" : 444377632,
      "il_h2_req_resp" : [23.27, 22.65],
      "interface_name" : "en0",
      "other" : {"interface-type": {"wired": 337}},
      "responsiveness" : 171.42611694335938,
      "test_endpoint" : "ussea4-edge-fx-019.aaplimg.com",
      "ul_bytes_transferred" : 96534527,
      "ul_throughput" : 49632612
    }"#;

    #[test]
    fn reads_a_real_report() {
        let test = parse(SAMPLE).expect("sample parses");

        assert!((test.down_mbps.unwrap() - 444.377632).abs() < 1e-6);
        assert!((test.up_mbps.unwrap() - 49.632612).abs() < 1e-6);
        assert_eq!(test.interface.as_deref(), Some("en0"));
        assert_eq!(test.bytes_transferred, Some(579530551 + 96534527));
        assert!(test.succeeded());
        assert_eq!(test.headline(), "↓ 444 Mbps  ↑ 49.6 Mbps");
    }

    #[test]
    fn missing_fields_do_not_fail_the_test() {
        let test = parse(r#"{"dl_throughput": 1000000}"#).expect("download alone is enough");

        assert_eq!(test.down_mbps, Some(1.0));
        assert_eq!(test.up_mbps, None);
        assert_eq!(format_mbps(test.up_mbps), "—");
    }

    #[test]
    fn no_throughput_is_an_error() {
        assert!(parse(r#"{"base_rtt": 20.0}"#).is_err());
        assert!(parse(r#"{"dl_throughput": 0, "ul_throughput": 0}"#).is_err());
    }

    #[test]
    fn reported_errors_surface() {
        let error = parse(r#"{"error_code": -1009, "error_domain": "NSURLErrorDomain"}"#)
            .expect_err("an error report is a failure");

        assert!(error.contains("-1009"), "{error}");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse("networkQuality: something went wrong").is_err());
    }
}
