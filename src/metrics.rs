//! Prometheus text-format metrics from plain atomics. No registry, no labels-by-map:
//! every series is a static field, which is all a single binary needs.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const BUCKETS_US: [u64; 12] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 1_000_000,
];

#[derive(Default)]
pub struct Metrics {
    pub cache_hit: AtomicU64,
    pub cache_miss: AtomicU64,
    pub cache_stale: AtomicU64,
    pub cache_revalidated: AtomicU64,
    pub cache_bypass: AtomicU64,
    pub cache_collapsed: AtomicU64,
    pub cache_pass: AtomicU64,
    pub origin_requests: AtomicU64,
    pub origin_errors: AtomicU64,
    pub upstream_ejections: AtomicU64,
    pub lb_panic: AtomicU64,
    pub rate_limited: AtomicU64,
    pub waf_blocked: AtomicU64,
    pub conn_rejected: AtomicU64,
    pub conn_active: AtomicU64,
    pub status_2xx: AtomicU64,
    pub status_3xx: AtomicU64,
    pub status_4xx: AtomicU64,
    pub status_5xx: AtomicU64,
    latency_buckets: [AtomicU64; 12],
    latency_inf: AtomicU64,
    latency_sum_us: AtomicU64,
}

pub static METRICS: std::sync::LazyLock<Metrics> = std::sync::LazyLock::new(Metrics::default);

impl Metrics {
    pub fn observe(&self, status: u16, us: u64) {
        match status / 100 {
            2 => &self.status_2xx,
            3 => &self.status_3xx,
            4 => &self.status_4xx,
            _ => &self.status_5xx,
        }
        .fetch_add(1, Relaxed);
        self.latency_sum_us.fetch_add(us, Relaxed);
        match BUCKETS_US.iter().position(|&b| us <= b) {
            Some(i) => self.latency_buckets[i].fetch_add(1, Relaxed),
            None => self.latency_inf.fetch_add(1, Relaxed),
        };
    }

    pub fn render(&self, cache_bytes: usize) -> String {
        let mut s = String::new();
        let mut c = |name: &str, help: &str, kind: &str, lines: &[(&str, u64)]| {
            let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (labels, v) in lines {
                let _ = writeln!(s, "{name}{labels} {v}");
            }
        };
        let l = |a: &AtomicU64| a.load(Relaxed);
        c(
            "edge_cache_requests_total",
            "Requests by cache outcome",
            "counter",
            &[
                ("{result=\"hit\"}", l(&self.cache_hit)),
                ("{result=\"miss\"}", l(&self.cache_miss)),
                ("{result=\"stale\"}", l(&self.cache_stale)),
                ("{result=\"revalidated\"}", l(&self.cache_revalidated)),
                ("{result=\"bypass\"}", l(&self.cache_bypass)),
                ("{result=\"collapsed\"}", l(&self.cache_collapsed)),
                ("{result=\"pass\"}", l(&self.cache_pass)),
            ],
        );
        c(
            "edge_responses_total",
            "Responses by status class",
            "counter",
            &[
                ("{class=\"2xx\"}", l(&self.status_2xx)),
                ("{class=\"3xx\"}", l(&self.status_3xx)),
                ("{class=\"4xx\"}", l(&self.status_4xx)),
                ("{class=\"5xx\"}", l(&self.status_5xx)),
            ],
        );
        c(
            "edge_origin_requests_total",
            "Requests sent to origin",
            "counter",
            &[("", l(&self.origin_requests))],
        );
        c(
            "edge_origin_errors_total",
            "Origin connect/timeout/5xx failures",
            "counter",
            &[("", l(&self.origin_errors))],
        );
        c(
            "edge_upstream_ejections_total",
            "Passive ejections",
            "counter",
            &[("", l(&self.upstream_ejections))],
        );
        c(
            "edge_lb_panic_total",
            "Picks made in panic mode (no available upstream)",
            "counter",
            &[("", l(&self.lb_panic))],
        );
        c(
            "edge_rate_limited_total",
            "Requests rejected with 429",
            "counter",
            &[("", l(&self.rate_limited))],
        );
        c(
            "edge_waf_blocked_total",
            "Requests rejected by WAF rules",
            "counter",
            &[("", l(&self.waf_blocked))],
        );
        c(
            "edge_connections_rejected_total",
            "Connections refused by limits",
            "counter",
            &[("", l(&self.conn_rejected))],
        );
        c(
            "edge_connections_active",
            "Open client connections",
            "gauge",
            &[("", l(&self.conn_active))],
        );
        c(
            "edge_cache_bytes",
            "Bytes held in cache",
            "gauge",
            &[("", cache_bytes as u64)],
        );
        let _ = writeln!(s, "# HELP edge_request_duration_seconds Request latency\n# TYPE edge_request_duration_seconds histogram");
        let mut cum = 0;
        for (i, b) in BUCKETS_US.iter().enumerate() {
            cum += l(&self.latency_buckets[i]);
            let _ = writeln!(
                s,
                "edge_request_duration_seconds_bucket{{le=\"{}\"}} {cum}",
                *b as f64 / 1e6
            );
        }
        cum += l(&self.latency_inf);
        let _ = writeln!(
            s,
            "edge_request_duration_seconds_bucket{{le=\"+Inf\"}} {cum}"
        );
        let _ = writeln!(
            s,
            "edge_request_duration_seconds_sum {}",
            l(&self.latency_sum_us) as f64 / 1e6
        );
        let _ = writeln!(s, "edge_request_duration_seconds_count {cum}");
        s
    }
}
