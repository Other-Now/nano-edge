//! Edge protection: per-IP token bucket, per-IP connection cap, and a tiny WAF.

use percent_encoding::percent_decode_str;
use regex::RegexSet;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

const SHARDS: usize = 32;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token bucket per client IP, sharded by IP hash to limit contention.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    shards: Vec<Mutex<HashMap<IpAddr, Bucket>>>,
}

impl RateLimiter {
    pub fn new(rate: f64, burst: f64) -> RateLimiter {
        RateLimiter {
            rate,
            burst: burst.max(1.0),
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, ip: &IpAddr) -> &Mutex<HashMap<IpAddr, Bucket>> {
        let h = match ip {
            IpAddr::V4(v) => u32::from(*v) as usize,
            IpAddr::V6(v) => u128::from(*v) as usize,
        };
        &self.shards[h.wrapping_mul(0x9E37_79B9) % SHARDS]
    }

    /// Ok(()) if allowed, Err(seconds until a token is available) if not.
    pub fn check(&self, ip: IpAddr) -> Result<(), f64> {
        if self.rate <= 0.0 {
            return Ok(());
        }
        let now = Instant::now();
        let mut m = self.shard(&ip).lock().unwrap();
        // Bound memory under a spoofed-source flood: drop full buckets when a shard grows large.
        if m.len() > 100_000 {
            let (rate, burst) = (self.rate, self.burst);
            m.retain(|_, b| b.tokens + now.duration_since(b.last).as_secs_f64() * rate < burst);
        }
        let b = m.entry(ip).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        b.tokens =
            (b.tokens + now.duration_since(b.last).as_secs_f64() * self.rate).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err((1.0 - b.tokens) / self.rate)
        }
    }
}

/// Open-connection count per IP; the guard decrements on drop.
pub struct ConnLimiter {
    max: usize,
    counts: Mutex<HashMap<IpAddr, usize>>,
}

pub struct ConnGuard {
    lim: Arc<ConnLimiter>,
    ip: IpAddr,
}

impl ConnLimiter {
    pub fn new(max: usize) -> ConnLimiter {
        ConnLimiter {
            max,
            counts: Mutex::new(HashMap::new()),
        }
    }

    pub fn acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnGuard> {
        if self.max == 0 {
            return Some(ConnGuard {
                lim: self.clone(),
                ip,
            });
        }
        let mut m = self.counts.lock().unwrap();
        let c = m.entry(ip).or_insert(0);
        if *c >= self.max {
            return None;
        }
        *c += 1;
        Some(ConnGuard {
            lim: self.clone(),
            ip,
        })
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if self.lim.max == 0 {
            return;
        }
        let mut m = self.lim.counts.lock().unwrap();
        if let Some(c) = m.get_mut(&self.ip) {
            *c -= 1;
            if *c == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

/// A handful of OWASP-CRS-style signatures. This is a demonstration of where a WAF
/// sits in the request path, not a replacement for the Core Rule Set.
static WAF: LazyLock<(RegexSet, Vec<&'static str>)> = LazyLock::new(|| {
    let rules = [
        ("sqli-union", r"(?i)\bunion\b[\s/*+]+(all[\s/*+]+)?select\b"),
        (
            "sqli-tautology",
            r#"(?i)['"]\s*or\s+['"]?\d+['"]?\s*=\s*['"]?\d+"#,
        ),
        ("sqli-comment", r"(?i)('|;)\s*(--|#|/\*)"),
        ("sqli-timing", r"(?i)\b(sleep|benchmark|pg_sleep)\s*\("),
        ("xss-script", r"(?i)<\s*script\b"),
        ("xss-handler", r"(?i)\bon(error|load|mouseover|focus)\s*="),
        ("xss-js-uri", r"(?i)javascript\s*:"),
        ("path-traversal", r"(\.\./|\.\.\\)"),
    ];
    (
        RegexSet::new(rules.iter().map(|r| r.1)).unwrap(),
        rules.iter().map(|r| r.0).collect(),
    )
});

/// Returns the name of the first matching rule. Decodes twice to catch double encoding.
pub fn waf_check(path_and_query: &str) -> Option<&'static str> {
    let once = percent_decode_str(path_and_query)
        .decode_utf8_lossy()
        .replace('+', " ");
    let twice = percent_decode_str(&once).decode_utf8_lossy().into_owned();
    for s in [&once, &twice] {
        if let Some(i) = WAF.0.matches(s).iter().next() {
            return Some(WAF.1[i]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_burst_then_limits() {
        let r = RateLimiter::new(10.0, 5.0);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..5 {
            assert!(r.check(ip).is_ok());
        }
        assert!(r.check(ip).is_err());
        assert!(r.check("5.6.7.8".parse().unwrap()).is_ok());
    }

    #[test]
    fn waf_rules() {
        assert_eq!(
            waf_check("/item?id=1%20UNION%20SELECT%20pw%20FROM%20users"),
            Some("sqli-union")
        );
        assert_eq!(
            waf_check("/login?u=admin'%20or%201=1"),
            Some("sqli-tautology")
        );
        assert_eq!(
            waf_check("/q?s=%253Cscript%253Ealert(1)"),
            Some("xss-script")
        );
        assert_eq!(
            waf_check("/static/../../etc/passwd"),
            Some("path-traversal")
        );
        assert_eq!(waf_check("/search?q=union+station+selection"), None);
        assert_eq!(waf_check("/obj/42?v=3"), None);
    }

    #[test]
    fn conn_limit() {
        let l = Arc::new(ConnLimiter::new(2));
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        let a = l.acquire(ip).unwrap();
        let _b = l.acquire(ip).unwrap();
        assert!(l.acquire(ip).is_none());
        drop(a);
        assert!(l.acquire(ip).is_some());
    }
}
