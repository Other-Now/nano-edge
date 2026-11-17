# nano-edge

A small CDN edge reverse proxy in Rust (Tokio, hyper 1.x, rustls), benchmarked against
NGINX 1.24 on the same machine. It has about 2,300 lines of Rust, plus a test origin and integration tests.

```
client ──► conn limits ─► WAF ─► per-IP rate limit ─► cache ──hit──► response
           (slowloris)                                  │ miss/stale
                                                        ▼
                                      request collapsing (1 fetch per key)
                                                        ▼
                               load balancer (RR / P2C / consistent hash)
                               health checks + passive ejection + retry
                                                        ▼
                                                     origins
```

| Area | What is implemented | Where |
|---|---|---|
| Caching (RFC 9111) | shared-cache storability, s-maxage > max-age > Expires > heuristic freshness, `Age` calculation, ETag / Last-Modified revalidation with 304 header merging, Vary, invalidation on POST/PUT/DELETE, `only-if-cached`, byte-bounded sharded LRU | `src/cache.rs` |
| CDN-Cache-Control (RFC 9213) | used *instead of* Cache-Control when present; an invalid Structured Field is ignored | `src/cache.rs` |
| RFC 5861 | stale-while-revalidate (background refresh, deduplicated) and stale-if-error | `src/proxy.rs` |
| Request collapsing | concurrent misses on one key share one origin fetch; the fill runs in a detached task, so a client hanging up can't cancel it; **hit-for-pass** stops collapsing keys that turned out to be uncacheable | `src/proxy.rs` |
| Load balancing | round-robin, power-of-two-choices on in-flight count, consistent hashing (160 virtual nodes); active health checks; passive ejection; retry on a different origin for body-less requests; **panic mode** and a 50% ejection cap | `src/lb.rs` |
| TLS | rustls; SNI chooses the certificate (exact name, then wildcard, then default); hot reload when the cert directory changes; ALPN picks h2 or HTTP/1.1 | `src/tls.rs` |
| Edge protection | per-IP token bucket (429 + Retry-After), global and per-IP connection caps, header-read timeout and TLS handshake timeout against slowloris, 8 OWASP-style WAF regexes (SQLi, XSS, path traversal; double-decoded) | `src/limits.rs`, `src/server.rs` |
| Observability | Prometheus `/metrics` (cache outcomes, origin errors, ejections, latency histogram), `tracing` access log | `src/metrics.rs` |

## Results

All figures come from one laptop: an i5-9300H with 4 vCPUs under WSL2. **This is not a real CDN deployment.**
The two proxies are pinned to the same 2 CPUs with 2 workers each. The origins and the load generator
([oha](https://github.com/hatoo/oha)) share the other 2 CPUs. Raw output is in `results/`, and
`bench/perf.sh` and `bench/cache-tests.sh` reproduce it.

### HTTP cache conformance — [cache-tests.fyi](https://cache-tests.fyi) suite

| | required tests passed (of 160) | optimal | check |
|---|---|---|---|
| **nano-edge** | **156** | 76 | 49 |
| NGINX 1.24 (the suite's own nginx config) | 115 | 66 | 22 |

My local NGINX run gives the same 115 as the suite's published nginx result, which confirms the setup.
For context, the suite's published results for Varnish, Squid and ATS score 138.
Of nano-edge's 4 misses, 2 need Range/206 support (not implemented),
1 is `age-parse-suffix` (it conflicts with the sibling tests that say to ignore an invalid Age), and
1 is a TypeError inside the test harness. The suite tests only spec conformance. Some of NGINX's misses are deliberate
design choices rather than bugs, so this is **not** a
claim that nano-edge is a better cache than NGINX.

### Throughput and latency (1 KiB object, 64 connections, 10 s, median of 3)

| | nano-edge | NGINX 1.24 |
|---|---|---|
| cache hit, closed loop | **83.2k req/s**, p99 1.79 ms | 50.7k req/s, p99 3.13 ms |
| pass-through (`no-store`), closed loop | 23.8k req/s, p99 10.6 ms | **37.2k req/s**, p99 5.6 ms |
| cache hit, open loop 5k req/s (latency-corrected) | p50 0.45 / p99 0.81 ms | p50 0.42 / p99 0.79 ms |
| cache hit, open loop 20k req/s | p50 0.52 / p99 1.68 ms | p50 0.51 / p99 3.55 ms |

- Cache hits are faster because nano-edge serves them from memory, while NGINX serves hits from its file cache (page cache + file I/O).
- Pass-through is slower. NGINX's upstream path is the more efficient one here, and nano-edge's numbers vary more between runs (22k–32k).
- The open-loop p99 at 20k/s was **not stable across runs**: an earlier full run had nano-edge 6.1 ms vs NGINX 3.6 ms. Treat the two as comparable at that rate.

### Behaviour tests

| Scenario | nano-edge | NGINX |
|---|---|---|
| **Stampede**: 1,000 concurrent requests for one cold object, origin takes 200 ms | **1 origin fetch** (1,000 with collapsing off); p50 **243 ms** | 1 fetch with `proxy_cache_lock`; p50 **526 ms** |
| One of 3 origins +50 ms, pass-through at 2,000 req/s | P2C sends **2.3%** of traffic to it (RR: 33%); mean latency 17.3 → **1.8 ms** | `least_conn`: 2.4%, mean 1.7 ms |
| Origin SIGKILLed 3 s into a 2,000 req/s run | 0 failed requests (retry + 1 passive ejection) | 0 failed requests (`proxy_next_upstream`) |
| Memory per idle keep-alive connection (5,000 conns) | 14.9 KiB | **0.64 KiB** |

- The collapsed followers in the stampede beat `proxy_cache_lock` on latency because they are woken by a `watch` channel the moment the fill lands. NGINX's lock waiters re-check every 500 ms.
- In the slow-origin test, **p99 did not improve** with P2C: 2.3% of requests still go to the slow origin, and 2.3% is more than 1%. P2C fixes the mean and p95 (51.9 → 1.1 ms), not the p99.

## Findings

Each of these was measured, then fixed or explained.

1. **Passive ejection turned an origin's 500s into a full outage.** The conformance suite's origin sends
   500s on purpose. After 3 of them, the only upstream was ejected for 10 s, and about 200 later tests got a 503 *from the proxy*.
   The first run passed 54/160 required tests. The fix is Envoy's approach: a pool that is 100% "down"
   routes to everyone (panic mode), and at most 50% of servers can be ejected. Required tests: 54 → 150.
2. **NGINX `proxy_cache_lock` serialises uncacheable URLs.** Pass-through to a `no-store` URL ran at
   4.7k req/s, and p99.9 sat at exactly **5,003 ms**, which is `proxy_cache_lock_timeout`. Concurrent misses on one key
   queue behind the lock even though the response will never be stored, because NGINX has no
   "hit-for-pass". With the lock off: 37k req/s. nano-edge had a milder form of the same flaw:
   followers waited for the leader and then fetched on their own, doubling latency. I added Varnish-style
   **hit-for-pass**, which remembers an uncacheable key for 10 s and doesn't collapse it. A test shows 20 concurrent
   requests through a 300 ms origin finishing in < 500 ms with it and ≥ 600 ms without.
   (In the closed-loop throughput test, against a 0.3 ms origin, the difference was within run-to-run noise.)
3. **The 1,000-request stampede had p99 = 1,261 ms with collapsing on.** That is a 1 s SYN retransmit:
   Tokio's default listen backlog is 1,024, and 1,000 simultaneous connects overflowed it. Binding with
   a 4,096 backlog (`server::bind`) brought p99 down to 263 ms. The no-collapse control also changed:
   it had shown "only 385 origin fetches" because late connections were arriving after the cache had filled.
   It now shows the true 1,000.
4. **Memory per idle connection was 23.5 KiB vs NGINX's 0.64 KiB.** `examples/conn_mem.rs` splits it into layers:
   a bare Tokio task + socket is 2.1 KiB; adding an 8 KiB read buffer gives 10.1 KiB; bare hyper http1 is 10.6 KiB.
   The rest was mine. The connection task's future was **6,728 bytes**, because Rust lays out the
   TLS-handshake, rustls and HTTP/2 branches inline even for plaintext HTTP/1 connections. Boxing the
   cold branches brought it to 1,048 bytes and the RSS to 14.9 KiB/conn. The remaining gap is hyper's fixed
   8 KiB read buffer (`INIT_BUFFER_SIZE`), which stays allocated while the connection is idle. NGINX frees
   its header buffer when a connection goes idle in keep-alive, and hyper 1.12 has no setting for that. At
   100k idle connections that is about 1.4 GiB vs about 63 MiB.

## Not done

- No Range/206 caching, no disk tier, no purge API, no HTTP/3/QUIC.
- The WAF is 8 regexes that show where a WAF sits in the request path. It is not the OWASP Core Rule Set.
- No real network or multi-machine run: everything is loopback on one WSL2 VM, with noisy neighbours on the same 4 CPUs.
- TLS is covered by tests (SNI selection and hot reload) but is **not benchmarked**.

## Run it

```bash
cargo test                                   # 18 unit + 14 integration tests
cargo run --release --bin origin 9001 &      # test origin
cargo run --release -- edge.toml             # proxy on :8080, metrics on :9100
curl -i localhost:8080/obj/1                 # x-cache: MISS, then HIT
bash scripts/setup-deps.sh                   # nginx (extracted .deb), oha, cache-tests
bash bench/cache-tests.sh && bash bench/perf.sh
```

Study notes: [docs/NOTES.md](docs/NOTES.md).
