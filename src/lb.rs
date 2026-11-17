//! Upstream selection: round-robin, power-of-two-choices on in-flight requests,
//! and a consistent-hash ring. Unhealthy or ejected servers are skipped by all three.
//!
//! Two ways a server leaves rotation:
//! * active: a background task GETs `health_path`; 2 failures down, 2 successes up
//! * passive: `eject_after_failures` consecutive proxy failures eject it for `eject_ms`

use crate::config::{LbPolicy, UpstreamConfig};
use crate::metrics::METRICS;
use rand::Rng;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VNODES: usize = 160;

pub struct Upstream {
    pub addr: String,
    pub inflight: AtomicUsize,
    healthy: AtomicBool,
    consecutive_fail: AtomicU32,
    /// Ejected until this many ms after `Pool::epoch`.
    ejected_until_ms: AtomicU64,
    health_streak: AtomicU32,
}

pub struct Pool {
    pub servers: Vec<Arc<Upstream>>,
    policy: LbPolicy,
    rr: AtomicUsize,
    /// Sorted (hash, server index).
    ring: Vec<(u64, usize)>,
    epoch: Instant,
    eject_after: u32,
    eject_for: Duration,
}

/// RAII in-flight counter, so P2C sees the request until its response headers arrive.
pub struct InflightGuard(Arc<Upstream>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Relaxed);
    }
}

fn hash64<T: Hash + ?Sized>(t: &T) -> u64 {
    let mut h = DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

impl Pool {
    pub fn new(cfg: &UpstreamConfig) -> Pool {
        let servers: Vec<Arc<Upstream>> = cfg
            .servers
            .iter()
            .map(|a| {
                Arc::new(Upstream {
                    addr: a.clone(),
                    inflight: AtomicUsize::new(0),
                    healthy: AtomicBool::new(true),
                    consecutive_fail: AtomicU32::new(0),
                    ejected_until_ms: AtomicU64::new(0),
                    health_streak: AtomicU32::new(0),
                })
            })
            .collect();
        let mut ring: Vec<(u64, usize)> = (0..servers.len())
            .flat_map(|i| {
                let addr = servers[i].addr.clone();
                (0..VNODES).map(move |v| (hash64(&format!("{addr}#{v}")), i))
            })
            .collect();
        ring.sort_unstable();
        Pool {
            servers,
            policy: cfg.policy,
            rr: AtomicUsize::new(0),
            ring,
            epoch: Instant::now(),
            eject_after: cfg.eject_after_failures,
            eject_for: Duration::from_millis(cfg.eject_ms),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    pub fn available(&self, u: &Upstream) -> bool {
        u.healthy.load(Relaxed) && u.ejected_until_ms.load(Relaxed) <= self.now_ms()
    }

    /// Pick a server, skipping `exclude` (the one that just failed, on retry).
    ///
    /// Panic mode (Envoy's term): if health checks and ejections have removed every
    /// server, route as if all were healthy. A pool that is 100% "down" is far more
    /// often a bad signal (e.g. an origin legitimately returning 500s) than a real
    /// total outage, and failing open costs nothing when it really is down.
    pub fn pick(&self, key: &str, exclude: Option<usize>) -> Option<(usize, InflightGuard)> {
        let n = self.servers.len();
        let panic = !(0..n).any(|i| Some(i) != exclude && self.available(&self.servers[i]));
        if panic && n > 0 {
            METRICS.lb_panic.fetch_add(1, Relaxed);
        }
        let ok = |i: usize| Some(i) != exclude && (panic || self.available(&self.servers[i]));
        let idx = match self.policy {
            LbPolicy::RoundRobin => {
                let start = self.rr.fetch_add(1, Relaxed);
                (0..n).map(|k| (start + k) % n).find(|&i| ok(i))
            }
            LbPolicy::P2c => {
                let cands: Vec<usize> = (0..n).filter(|&i| ok(i)).collect();
                match cands.len() {
                    0 => None,
                    1 => Some(cands[0]),
                    m => {
                        let mut rng = rand::thread_rng();
                        let a = cands[rng.gen_range(0..m)];
                        let mut b = cands[rng.gen_range(0..m - 1)];
                        if b == a {
                            b = cands[m - 1];
                        }
                        let (la, lb) = (
                            self.servers[a].inflight.load(Relaxed),
                            self.servers[b].inflight.load(Relaxed),
                        );
                        Some(if la <= lb { a } else { b })
                    }
                }
            }
            LbPolicy::ConsistentHash => {
                if self.ring.is_empty() {
                    None
                } else {
                    let h = hash64(key);
                    let start = self.ring.partition_point(|&(rh, _)| rh < h);
                    (0..self.ring.len())
                        .map(|k| self.ring[(start + k) % self.ring.len()].1)
                        .find(|&i| ok(i))
                }
            }
        }?;
        let u = self.servers[idx].clone();
        u.inflight.fetch_add(1, Relaxed);
        Some((idx, InflightGuard(u)))
    }

    pub fn report(&self, idx: usize, success: bool) {
        let u = &self.servers[idx];
        if success {
            u.consecutive_fail.store(0, Relaxed);
            return;
        }
        let f = u.consecutive_fail.fetch_add(1, Relaxed) + 1;
        // Never eject more than half the pool (Envoy's max_ejection_percent).
        let now = self.now_ms();
        let ejected = self
            .servers
            .iter()
            .filter(|s| s.ejected_until_ms.load(Relaxed) > now)
            .count();
        let room = (ejected + 1) * 2 <= self.servers.len();
        if self.eject_after > 0 && f >= self.eject_after && room {
            u.consecutive_fail.store(0, Relaxed);
            u.ejected_until_ms
                .store(self.now_ms() + self.eject_for.as_millis() as u64, Relaxed);
            METRICS.upstream_ejections.fetch_add(1, Relaxed);
            tracing::warn!(upstream = %u.addr, "ejected after {f} consecutive failures");
        }
    }

    fn set_health(&self, u: &Upstream, ok: bool) {
        let was = u.healthy.load(Relaxed);
        if ok == was {
            u.health_streak.store(0, Relaxed);
            return;
        }
        if u.health_streak.fetch_add(1, Relaxed) + 1 >= 2 {
            u.healthy.store(ok, Relaxed);
            u.health_streak.store(0, Relaxed);
            tracing::warn!(upstream = %u.addr, healthy = ok, "health check state change");
        }
    }
}

/// Active health checker: a raw HTTP/1.1 GET with a short timeout, no pooling,
/// so a wedged server can't hold a pooled connection hostage.
pub fn spawn_health_checks(pool: Arc<Pool>, path: String, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            for u in pool.servers.clone() {
                let ok = tokio::time::timeout(
                    interval.min(Duration::from_secs(2)),
                    probe(&u.addr, &path),
                )
                .await
                .unwrap_or(false);
                pool.set_health(&u, ok);
            }
        }
    });
}

async fn probe(addr: &str, path: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut s) = tokio::net::TcpStream::connect(addr).await else {
        return false;
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf).await.is_ok() && buf.starts_with(b"HTTP/1.1 2")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(policy: LbPolicy, n: usize) -> Pool {
        let cfg = UpstreamConfig {
            servers: (0..n).map(|i| format!("10.0.0.{i}:80")).collect(),
            policy,
            ..Default::default()
        };
        Pool::new(&cfg)
    }

    #[test]
    fn p2c_avoids_loaded_server() {
        let p = pool(LbPolicy::P2c, 2);
        p.servers[0].inflight.store(100, Relaxed);
        for _ in 0..100 {
            assert_eq!(p.pick("k", None).unwrap().0, 1);
        }
    }

    #[test]
    fn consistent_hash_moves_about_one_nth_of_keys() {
        let a = pool(LbPolicy::ConsistentHash, 4);
        let before: Vec<usize> = (0..10_000)
            .map(|k| a.pick(&format!("/obj/{k}"), None).unwrap().0)
            .collect();
        // Take server 2 down: only its keys should move.
        a.servers[2].healthy.store(false, Relaxed);
        let mut moved = 0;
        for (k, &b) in before.iter().enumerate() {
            let now = a.pick(&format!("/obj/{k}"), None).unwrap().0;
            if now != b {
                assert_eq!(b, 2, "a key not owned by the dead server moved");
                moved += 1;
            }
        }
        let share = moved as f64 / 10_000.0;
        assert!((0.15..0.35).contains(&share), "moved {share}");
    }

    #[test]
    fn never_ejects_the_last_server() {
        let p = pool(LbPolicy::RoundRobin, 1);
        for _ in 0..10 {
            p.report(0, false);
        }
        assert!(p.available(&p.servers[0]));
        // And if health checks mark everything down, route anyway (panic mode).
        p.servers[0].healthy.store(false, Relaxed);
        assert_eq!(p.pick("k", None).unwrap().0, 0);
    }

    #[test]
    fn passive_ejection() {
        let p = pool(LbPolicy::RoundRobin, 2);
        for _ in 0..3 {
            p.report(0, false);
        }
        for _ in 0..10 {
            assert_eq!(p.pick("k", None).unwrap().0, 1);
        }
    }
}
