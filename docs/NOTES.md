# nano-edge: study notes (1–2 days)

Read in this order: the pitch, one request through the code, each subsystem,
the four stories, then the interview questions. Every number is in `results/`.

## 1. The 60-second pitch

> I built a small CDN edge proxy in Rust on Tokio, hyper and rustls. It has
> an RFC 9111 shared cache with revalidation, stale-while-revalidate and
> CDN-Cache-Control. It collapses requests, so a 1,000-request stampede on a
> cold object costs one origin fetch. It load-balances with P2C or consistent
> hashing and uses health checks and passive ejection. It picks TLS certs by
> SNI and hot-reloads them. It also has rate limiting, slowloris defences and
> a small WAF. I benchmarked it against NGINX on the same cores. On the public
> HTTP cache conformance suite it passes 156 of 160 required tests to NGINX's
> 115. It serves cache hits at 83k vs 51k req/s. NGINX is faster on
> pass-through, at 37k vs 24k req/s, and uses 0.64 KiB per idle connection to
> my 14.9. I traced that memory gap down to hyper's 8 KiB read buffer. The
> benchmarks also found two bugs: an ejection bug in my proxy, and a
> cache-lock pathology in NGINX.

Be ready to say: everything ran on one laptop under WSL2 with loopback
networking. It is not a deployed CDN.

## 2. One request through the code

1. `server.rs::serve`: accept → global semaphore (`max_connections`) → per-IP
   cap → spawn a task. Plaintext is HTTP/1.1 only; for TLS, ALPN decides
   between h2 and HTTP/1.1.
2. hyper parses the request. `header_read_timeout` closes slowloris
   connections (the test trickles a byte every 100 ms and gets cut at 500 ms).
3. `proxy.rs::route`: WAF regexes on the decoded path → token bucket per IP
   → non-GET/HEAD bypasses the cache (and a 2xx/3xx to POST/PUT/DELETE
   invalidates the URI).
4. `proxy.rs::cached`: look up `host + path + query`, then pick the variant
   by Vary.
   - fresh → **HIT**, with `Age` set
   - stale but within stale-while-revalidate → **STALE**, plus a
     deduplicated background refresh
   - otherwise revalidate synchronously with If-None-Match /
     If-Modified-Since. A 304 → **REVALIDATED** (headers merged, body kept).
     Origin down and stale-if-error allows it → serve stale.
5. Miss → `collapsed_miss`. The first request becomes the leader and fetches
   in a `tokio::spawn`ed task. Followers wait on a `watch` channel and get
   the leader's entry. If the response is not storable, the key gets a
   hit-for-pass marker for 10 s.
6. `forward`: pick an upstream (`lb.rs`), keep an in-flight guard so P2C
   sees it, apply a timeout, report success or failure for passive ejection,
   and retry once on another server if the request had no body.

## 3. HTTP caching in 15 minutes (RFC 9111)

- **Shared vs private cache.** A CDN is shared. It must not store `private`,
  must honour `s-maxage`, and must not store responses to requests carrying
  `Authorization` unless they are `public`, `s-maxage` or `must-revalidate`.
- **Freshness lifetime**, in order: `s-maxage` → `max-age` → `Expires − Date`
  → heuristic (10% of `Date − Last-Modified`, capped at 1 day here).
- **Age** (§4.2.3):
  `corrected_initial_age = max(response_time − Date, Age_header + (response_time − request_time))`,
  and `current_age = corrected_initial_age + time resident in cache`.
  Fresh means `current_age < freshness_lifetime`.
- **no-cache ≠ no-store.** no-cache: store, but revalidate before every use.
  no-store: never store.
- **Revalidation.** Send the stored ETag as If-None-Match. A 304 means
  "unchanged": update the stored headers from the 304, keep the body, reset
  the age.
- **Vary.** The response is keyed on the named request headers too. Here,
  each key holds a list of variants. `Vary: *` is never stored.
- **stale-while-revalidate=N**: serve stale for N s while refreshing in the
  background. **stale-if-error=N**: serve stale if the origin fails.
  `must-revalidate` and `s-maxage` forbid both.
- **CDN-Cache-Control (RFC 9213):** if present, the CDN uses it *instead of*
  Cache-Control. Cache-Control then only governs browsers. Example:
  `CDN-Cache-Control: max-age=3600` with `Cache-Control: no-store` → the CDN
  caches it and browsers don't.
- **Invalidation:** a successful POST/PUT/DELETE removes the stored response
  for that URI.

## 4. Request collapsing and hit-for-pass

- Without collapsing, a popular object expiring sends every concurrent miss
  to the origin. That is the "thundering herd" or stampede, and it can take
  an origin down at exactly the moment traffic peaks.
- The leader's fetch runs in a **detached task**. In hyper, the service
  future is dropped when the client disconnects. If the fill ran inside it,
  one impatient client would cancel the fetch for 999 waiters.
- **Followers wait on a `tokio::sync::watch`**, so they wake as soon as the
  leader finishes. NGINX's `proxy_cache_lock` waiters re-check every 500 ms,
  which is why its stampede p50 was 526 ms against a 200 ms origin, vs 243 ms
  here.
- **Hit-for-pass** (Varnish's term). If the response turns out unstorable
  (`no-store`, `private`), collapsing that key only serialises requests. So
  the key is remembered for 10 s and later misses go straight to the origin.
  NGINX lacks this; see story 2.
- Follower timeout: `collapse_timeout_ms`, 5 s. After that, a follower goes
  to the origin itself.

## 5. Load balancing

- **Round-robin**: ignores load, so a slow server keeps its 1/N share (33%
  in the test).
- **P2C (power of two choices)**: pick 2 random healthy servers and send to
  the one with fewer requests in flight. This is the Mitzenmacher result:
  max load drops from O(log n / log log n) to O(log log n), with no global
  state. Measured: the slow origin's share fell from 33% to 2.3%, mean
  latency from 17.3 to 1.8 ms, and p95 from 51.9 to 1.1 ms. **p99 stayed at
  ~52 ms** because 2.3% > 1%. NGINX `least_conn` got the same numbers.
- **Consistent hashing**: 160 virtual nodes per server on a sorted ring;
  binary-search the key's hash. When a server dies, only its keys move (the
  test checks that 15–35% of keys move with 4 servers and that no other key
  moves). Why a CDN wants it: the same URL always goes to the same origin or
  mid-tier, so that tier's cache stays hot.
- **Health**:
  - active: GET `/health`; 2 failures mark a server down, 2 successes bring
    it back
  - passive: 3 consecutive failures eject it for 10 s
  - **panic mode**: if nothing is available, route to all servers
  - **ejection cap**: never eject more than 50% of the pool
- **Retry**: only for body-less requests (GET/HEAD), because the body stream
  has already been consumed. Use a different server than the one that failed.

## 6. TLS

- `ResolvesServerCert` is called for each ClientHello with the SNI. Lookup
  order: exact name → `*.parent` → `default`.
- Hot reload:
  - every 2 s, compare the cert directory's (path, mtime) list
  - on a change, re-read the whole directory and swap the
    `RwLock<Arc<HashMap>>`
  - handshakes in flight keep the Arc they already cloned
  - a half-written pair fails to parse and is skipped, with a warning
- ALPN offers `h2` and `http/1.1`. The connection is served by
  `http2::Builder` or `http1::Builder` depending on the result, with no
  byte-sniffing.
- The TLS handshake has its own timeout (the same value as the header
  timeout), so a slow-handshake attack can't hold a connection.

## 7. Edge protection

- **Token bucket** per IP:
  - `tokens = min(burst, tokens + elapsed × rate)`; a request costs 1
  - the 429 carries `Retry-After`
  - sharded into 32 mutexes
  - when a shard passes 100k IPs, full buckets are pruned, which bounds
    memory under a spoofed-source flood
- **Slowloris**: the defence is a deadline on the *whole* header, not an idle
  timeout between bytes. A client trickling 1 byte every 100 ms never trips
  an idle timeout.
- **Connection caps**: global (a semaphore; over the cap the socket is closed
  at once) and per IP.
- **WAF**:
  - 8 regexes for union-select, tautologies, comment injection, timing
    functions, `<script`, `on*=` handlers, `javascript:` and `../`
  - the input is percent-decoded twice, to catch `%253C`
  - a false-positive test makes sure `union station selection` is allowed
  - Say plainly that real WAFs use the OWASP CRS with anomaly scoring, not
    first-match.

## 8. The stories (measured, then fixed or explained)

**Story 1: my health checking caused an outage.**
- The conformance suite's origin returns 500s on purpose (stale-if-error
  tests).
- Passive ejection counted 3 of them and ejected the *only* origin for 10 s.
  About 200 later tests got 503 "no healthy upstream" from my proxy. The
  first conformance run scored 54/160.
- Fix: Envoy's panic threshold. If everything is out, route to everything,
  since a whole pool "down" is more likely a bad signal than a real outage.
  Plus max-ejection at 50%. Score: 54 → 150.
- Lesson: outlier detection needs a floor. NGINX never marks a single-server
  upstream as failed for the same reason.

**Story 2: NGINX's cache lock and a 5-second plateau.**
- Pass-through benchmark: NGINX at 4.2k req/s with p99.9 = 5,003 ms on every
  run.
- First hypothesis: TIME_WAIT reuse (there were 10k sockets in TIME_WAIT).
  Wrong: they were left over from earlier tests.
- Second hypothesis: `keepalive_requests 1000` recycling client connections.
  Wrong: setting it to 1M changed nothing.
- Third: 5,003 ms = `proxy_cache_lock_timeout` (5 s). All 64 connections
  requested the same `no-store` URL; each miss took the lock, and the others
  queued even though nothing would be stored. Lock off → 37–41k req/s.
- My proxy had a milder version of this. I added hit-for-pass.
- Lesson: request collapsing needs to remember what is uncacheable.

**Story 3: the stampede p99 was a TCP problem.**
- With collapsing on, p99 was still 1,261 ms. 1,261 ≈ 1 s (the initial SYN
  retransmit timeout) + 200 ms (the origin).
- Tokio's default `listen()` backlog is 1,024. 1,000 simultaneous connects
  plus the in-progress accepts overflowed it, and the kernel dropped SYNs.
- Fix: `TcpSocket::listen(4096)`. p99 → 263 ms.
- Side effect: the no-collapse control had shown 385 origin fetches, not
  1,000, because delayed connections arrived after the cache had filled. The
  bad listener had been hiding the herd.

**Story 4: where 23.5 KiB per connection went.**
- NGINX: 0.64 KiB per idle keep-alive connection. Mine: 23.5.
- Measured layer by layer with `examples/conn_mem.rs`:
  - task + socket: 2.1 KiB
  - + an 8 KiB buffer: 10.1
  - bare hyper: 10.6
- Mine was 20.8 after the first fix (HTTP/1-only on plaintext, no h2 sniff
  buffer).
- I logged `size_of_val` of the connection future: **6,728 bytes**. An async
  fn's state machine is as big as its largest await point, and mine inlined
  the TLS accept future, the rustls connection and the HTTP/2 server even for
  plaintext connections.
- `Box::pin` on the cold branches → 1,048 bytes and 14.9 KiB/conn.
- The rest is hyper's `INIT_BUFFER_SIZE = 8192` read buffer, kept while
  idle. NGINX frees its header buffer when a connection goes idle in
  keep-alive (`ngx_http_set_keepalive`). hyper 1.12 has no option for this.
- At 100k idle connections that is ~1.4 GiB vs ~63 MiB. That is a real
  reason a CDN might choose a different HTTP stack (Pingora wrote its own).

## 9. Numbers to quote (exactly)

- Conformance (required): **156/160** vs NGINX 1.24 **115/160**. The local
  NGINX run matches the published nginx result. Varnish/Squid/ATS published:
  138.
- Cache hit, 1 KiB, 64 conns: **83.2k vs 50.7k req/s** (median of 3), p99
  1.79 vs 3.13 ms.
- Pass-through: **23.8k vs 37.2k req/s**. NGINX wins.
- Stampede 1,000 → **1** origin fetch; p50 243 ms vs NGINX 526 ms.
- Slow origin: P2C 2.3% share (RR 33%), mean 17.3 → 1.8 ms, p99 unchanged.
- Kill an origin at 2k req/s: 0 failed requests for both.
- Idle connection: 14.9 KiB vs 0.64 KiB.
- Caveats: one laptop, WSL2, loopback; the open-loop p99 at 20k req/s
  flipped between runs; TLS not benchmarked.

## 10. Likely interview questions

- *Why does a CDN care about Age?* Downstream caches subtract it from
  max-age. If you drop it, a browser caches for longer than the origin
  intended.
- *How do you purge?* Not built. You'd index by URL (and by surrogate keys or
  tags), propagate the purge to all PoPs, and accept a short window of
  inconsistency, or use soft purge (mark stale) so origins aren't stampeded.
- *Why not one global mutex for the cache?* 16 shards keyed by hash. Lock
  hold time is a HashMap lookup and an `Arc` clone. Bodies are `Bytes`, so a
  hit never copies the body under the lock.
- *Why `Bytes`?* It is refcounted. One stored body is shared by every
  concurrent response, which gives zero-copy on hits.
- *P2C vs least-connections?* Least-conn needs global state and herds onto
  the momentarily emptiest server when the counts are stale. P2C gets nearly
  the same balance from two random reads.
- *Why consistent hashing at the edge?* Origin or mid-tier cache affinity.
  Without it, each tier caches everything and hit rates drop.
- *How would you scale this past one box?* Anycast to PoPs, ECMP to many edge
  nodes, consistent hashing between edge and mid-tier (shield), and the
  origin behind the shield.
- *Rust vs C for this?* Rust gave memory safety in parsing and async with no
  data races (the cache shards are `Mutex`, the counters `Atomic`). It cost me
  hyper's fixed buffer model (story 4) and future-size pitfalls invisible in
  the source. C (NGINX) controls every byte, which is why it is at 0.64 KiB.
- *What breaks first under a real DDoS?* Here: per-IP buckets (useless
  against a botnet), loopback-only tests, and no SYN cookies tuning or
  eBPF/XDP drop in front. Pair it with the nano-dpi / nano-dns work for the
  L3/L4 side.
