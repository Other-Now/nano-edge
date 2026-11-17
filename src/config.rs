//! TOML configuration. Every knob has a default so a config file can be three lines.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Plain-HTTP listener, e.g. "0.0.0.0:8080".
    pub listen: String,
    /// Optional TLS listener; certificates come from `tls_cert_dir`.
    pub tls_listen: Option<String>,
    /// Directory of `<name>.crt` + `<name>.key` PEM pairs. `default.crt` is used
    /// when the client sends no SNI or an unknown name.
    pub tls_cert_dir: Option<PathBuf>,
    /// Admin listener serving /metrics.
    pub admin_listen: Option<String>,
    /// Tokio worker threads (0 = one per core).
    pub workers: usize,
    pub upstream: UpstreamConfig,
    pub cache: CacheConfig,
    pub limits: LimitsConfig,
    pub waf: bool,
    pub access_log: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LbPolicy {
    RoundRobin,
    /// Power of two choices on in-flight request count.
    P2c,
    /// Consistent hashing on the cache key (origin cache affinity).
    ConsistentHash,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub servers: Vec<String>,
    pub policy: LbPolicy,
    /// Active health checks: GET this path on every server every `health_interval_ms`.
    pub health_path: Option<String>,
    pub health_interval_ms: u64,
    /// Passive ejection: this many consecutive failures ejects a server...
    pub eject_after_failures: u32,
    /// ...for this long.
    pub eject_ms: u64,
    /// Time allowed for the origin to return response headers.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_bytes: usize,
    pub max_object_bytes: usize,
    /// Request collapsing: concurrent misses on one key share one origin fetch.
    pub collapse: bool,
    /// How long a collapsed follower waits for the leader before going to origin itself.
    pub collapse_timeout_ms: u64,
    /// Remember for 10 s that a key's response was not storable, and stop collapsing it.
    pub hit_for_pass: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Per-client-IP token bucket. 0 disables.
    pub rate_per_sec: f64,
    pub burst: f64,
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    /// Slowloris defence: a request's headers must arrive within this time.
    pub header_read_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:8080".into(),
            tls_listen: None,
            tls_cert_dir: None,
            admin_listen: None,
            workers: 0,
            upstream: UpstreamConfig::default(),
            cache: CacheConfig::default(),
            limits: LimitsConfig::default(),
            waf: true,
            access_log: false,
        }
    }
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        UpstreamConfig {
            servers: vec![],
            policy: LbPolicy::P2c,
            health_path: None,
            health_interval_ms: 1000,
            eject_after_failures: 3,
            eject_ms: 10_000,
            timeout_ms: 10_000,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            enabled: true,
            max_bytes: 256 << 20,
            max_object_bytes: 8 << 20,
            collapse: true,
            collapse_timeout_ms: 5_000,
            hit_for_pass: true,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            rate_per_sec: 0.0,
            burst: 0.0,
            max_connections: 50_000,
            max_connections_per_ip: 0,
            header_read_timeout_ms: 5_000,
        }
    }
}

impl UpstreamConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

pub fn load(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    toml::from_str(&text).map_err(|e| format!("{path}: {e}"))
}
