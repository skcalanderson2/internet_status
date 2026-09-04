//! Configuration loading. A TOML file is created on first run and can be
//! edited by hand; the app reads it at startup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What role a probe target plays in the diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The home router. If this fails, the problem is local.
    Gateway,
    /// A public IP reached by ICMP, so no DNS is involved.
    Internet,
    /// A hostname, so success also proves DNS resolution works.
    Dns,
}

/// How a target is tested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// A TCP handshake against `port`. This is the default because it proves
    /// an end-to-end connection actually completes, and because it survives
    /// the ICMP filtering many ISPs apply to public addresses.
    #[default]
    Tcp,
    /// An ICMP echo. Cheaper and gives a cleaner round-trip time, but many
    /// upstream devices answer echoes with "communication prohibited by
    /// filter" or rate-limit them, which looks identical to an outage.
    Icmp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub kind: Kind,
    #[serde(default)]
    pub method: Method,
    /// TCP port, ignored when `method` is `icmp`.
    ///
    /// Prefer 443. Port 53 is a trap: home routers commonly intercept DNS, so
    /// a connection to any address on port 53 succeeds against the router in
    /// about a millisecond even with the Internet down.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    443
}

impl Target {
    /// The home router, probed with ICMP: routers answer echoes reliably and
    /// expose no port we can count on being open.
    pub fn gateway(host: String) -> Self {
        Self {
            name: "Home router".into(),
            host,
            kind: Kind::Gateway,
            method: Method::Icmp,
            port: default_port(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds between probe rounds.
    pub interval_secs: u64,
    /// Consecutive bad rounds required before an outage is declared. Keeps a
    /// single dropped packet from being logged as an outage.
    pub fail_threshold: u32,
    /// How long a single probe may take before it counts as a failure.
    pub timeout_ms: u64,
    /// Hop limit handed to `traceroute -m`.
    pub traceroute_max_hops: u32,
    /// How many probe rounds the latency chart keeps.
    pub history_len: usize,
    /// Gateway IP. Empty means auto-detect via `route get default`.
    pub gateway: String,
    pub targets: Vec<Target>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_secs: 5,
            fail_threshold: 2,
            timeout_ms: 2000,
            traceroute_max_hops: 15,
            history_len: 240,
            gateway: String::new(),
            targets: vec![
                Target {
                    name: "Cloudflare".into(),
                    host: "1.1.1.1".into(),
                    kind: Kind::Internet,
                    method: Method::Tcp,
                    port: 443,
                },
                Target {
                    name: "Google DNS".into(),
                    host: "8.8.8.8".into(),
                    kind: Kind::Internet,
                    method: Method::Tcp,
                    port: 443,
                },
                Target {
                    name: "DNS lookup".into(),
                    host: "www.google.com".into(),
                    kind: Kind::Dns,
                    method: Method::Tcp,
                    port: 443,
                },
            ],
        }
    }
}

/// `~/Library/Application Support/internet_status`
pub fn data_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));

    base.join("internet_status")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

impl Config {
    /// Reads the config file, writing out the defaults first if it is missing.
    /// A malformed file is reported and the defaults are used, so a typo never
    /// stops the app from monitoring.
    pub fn load() -> (Self, Option<String>) {
        let path = config_path();

        if !path.exists() {
            let config = Self::default();

            if let Err(error) = config.save() {
                return (config, Some(format!("could not write {}: {error}", path.display())));
            }

            return (config, None);
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str::<Self>(&contents) {
                Ok(config) if config.targets.is_empty() => (
                    Self::default(),
                    Some("config has no targets; using defaults".into()),
                ),
                Ok(config) => (config, None),
                Err(error) => (
                    Self::default(),
                    Some(format!("{} is invalid ({error}); using defaults", path.display())),
                ),
            },
            Err(error) => (
                Self::default(),
                Some(format!("could not read {}: {error}", path.display())),
            ),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;

        std::fs::write(path, contents)
    }

    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.interval_secs.max(1))
    }
}
