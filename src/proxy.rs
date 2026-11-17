//! The request path: WAF -> rate limit -> cache (lookup / collapse / revalidate) -> upstream.

use crate::cache::{self, Cache, CacheControl, Entry};
use crate::config::Config;
use crate::lb::Pool;
use crate::limits::{waf_check, ConnLimiter, RateLimiter};
use crate::metrics::METRICS;
use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;

pub type Body = BoxBody<Bytes, hyper::Error>;

/// Collapsed-fetch slot: `None` while the leader is fetching, `Some(result)` when done.
/// `Some(None)` means "the response was not storable; fetch it yourself".
type Slot = watch::Receiver<Option<Option<Arc<Entry>>>>;

pub struct State {
    pub cfg: Config,
    pub cache: Cache,
    pub pool: Arc<Pool>,
    client: Client<HttpConnector, Body>,
    pub ratelimit: RateLimiter,
    pub conns: Arc<ConnLimiter>,
    inflight: Mutex<HashMap<String, Slot>>,
    revalidating: Mutex<HashSet<String>>,
    /// Hit-for-pass (Varnish's term): keys whose last response was not storable, so
    /// collapsing them would only serialise requests. Value = when the marker expires.
    pass: Mutex<HashMap<String, Instant>>,
}

const HIT_FOR_PASS: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum UpstreamError {
    NoServer,
    Timeout,
    Connect,
}

impl State {
    pub fn new(cfg: Config) -> Arc<State> {
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        conn.set_connect_timeout(Some(Duration::from_secs(2)));
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(512)
            .build(conn);
        let pool = Arc::new(Pool::new(&cfg.upstream));
        if let Some(path) = &cfg.upstream.health_path {
            crate::lb::spawn_health_checks(
                pool.clone(),
                path.clone(),
                Duration::from_millis(cfg.upstream.health_interval_ms),
            );
        }
        Arc::new(State {
            cache: Cache::new(cfg.cache.max_bytes, cfg.cache.max_object_bytes),
            ratelimit: RateLimiter::new(cfg.limits.rate_per_sec, cfg.limits.burst),
            conns: Arc::new(ConnLimiter::new(cfg.limits.max_connections_per_ip)),
            pool,
            client,
            inflight: Mutex::new(HashMap::new()),
            revalidating: Mutex::new(HashSet::new()),
            pass: Mutex::new(HashMap::new()),
            cfg,
        })
    }
}

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|n| match n {}).boxed()
}

fn empty() -> Body {
    Empty::new().map_err(|n| match n {}).boxed()
}

fn simple(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(full(format!("{msg}\n")));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
];

fn strip_hop_by_hop(h: &mut HeaderMap) {
    let listed: Vec<String> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .collect();
    for n in HOP_BY_HOP.iter().map(|s| s.to_string()).chain(listed) {
        h.remove(n.as_str());
    }
}

/// Cache key: host + path + query. Scheme is folded in because TLS and plain share content.
fn cache_key(req: &Request<Incoming>) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or(req.uri().host())
        .unwrap_or("");
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    format!("{}{}", host.to_ascii_lowercase(), pq)
}

pub async fn handle(
    state: Arc<State>,
    req: Request<Incoming>,
    client_ip: IpAddr,
) -> Result<Response<Body>, hyper::Error> {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let (resp, outcome) = route(&state, req, client_ip).await;
    let status = resp.status().as_u16();
    let us = start.elapsed().as_micros() as u64;
    METRICS.observe(status, us);
    if state.cfg.access_log {
        tracing::info!(target: "access", ip = %client_ip, %method, path, status, cache = outcome, us);
    }
    Ok(resp)
}

async fn route(
    state: &Arc<State>,
    req: Request<Incoming>,
    ip: IpAddr,
) -> (Response<Body>, &'static str) {
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    if state.cfg.waf {
        if let Some(rule) = waf_check(pq) {
            METRICS.waf_blocked.fetch_add(1, Relaxed);
            tracing::debug!(%ip, rule, "waf block");
            return (
                simple(StatusCode::FORBIDDEN, &format!("blocked by rule {rule}")),
                "waf",
            );
        }
    }
    if let Err(wait) = state.ratelimit.check(ip) {
        METRICS.rate_limited.fetch_add(1, Relaxed);
        let mut r = simple(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        r.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from(wait.ceil().max(1.0) as u64),
        );
        return (r, "ratelimit");
    }
    let key = cache_key(&req);
    let cacheable_method = req.method() == Method::GET || req.method() == Method::HEAD;
    if !state.cfg.cache.enabled || !cacheable_method {
        let unsafe_method = !matches!(
            *req.method(),
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        );
        METRICS.cache_bypass.fetch_add(1, Relaxed);
        let r = bypass(state, req, ip, &key).await;
        // §4.4: a successful unsafe request invalidates the stored response for its URI.
        if unsafe_method && (r.status().is_success() || r.status().is_redirection()) {
            state.cache.remove(&key);
        }
        return (r, "bypass");
    }
    cached(state, req, ip, key).await
}

async fn cached(
    state: &Arc<State>,
    req: Request<Incoming>,
    ip: IpAddr,
    key: String,
) -> (Response<Body>, &'static str) {
    let req_cc = CacheControl::parse(req.headers());
    let head = req.method() == Method::HEAD;
    let (parts, _) = req.into_parts();
    let now = SystemTime::now();

    if let Some(e) = state.cache.get(&key, &parts.headers) {
        let within_req_max_age = req_cc.max_age.is_none_or(|m| e.age(now) <= m);
        if !req_cc.no_cache && within_req_max_age && e.is_fresh(now) {
            METRICS.cache_hit.fetch_add(1, Relaxed);
            return (serve_entry(&e, &parts.headers, head, "HIT"), "hit");
        }
        if !req_cc.no_cache && within_req_max_age && e.can_swr(now) {
            METRICS.cache_stale.fetch_add(1, Relaxed);
            spawn_revalidate(
                state,
                key,
                parts.uri.clone(),
                parts.headers.clone(),
                e.clone(),
                ip,
            );
            return (serve_entry(&e, &parts.headers, head, "STALE"), "stale");
        }
        // Synchronous revalidation.
        return match fetch_for_cache(state, &key, &parts.uri, &parts.headers, Some(&e), ip).await {
            Ok(Fetched::Stored(n, revalidated)) => {
                if revalidated {
                    METRICS.cache_revalidated.fetch_add(1, Relaxed);
                } else {
                    METRICS.cache_miss.fetch_add(1, Relaxed);
                }
                (
                    serve_entry(
                        &n,
                        &parts.headers,
                        head,
                        if revalidated { "REVALIDATED" } else { "MISS" },
                    ),
                    "revalidated",
                )
            }
            Ok(Fetched::Passthrough(r))
                if !r.status().is_server_error() || !e.can_sie(now, &req_cc) =>
            {
                METRICS.cache_miss.fetch_add(1, Relaxed);
                (r, "miss")
            }
            Ok(Fetched::Passthrough(_)) | Err(_) if e.can_sie(now, &req_cc) => {
                METRICS.cache_stale.fetch_add(1, Relaxed);
                (
                    serve_entry(&e, &parts.headers, head, "STALE-IF-ERROR"),
                    "stale",
                )
            }
            Ok(Fetched::Passthrough(r)) => (r, "miss"),
            Err(err) => (upstream_error(err), "error"),
        };
    }

    if req_cc.only_if_cached {
        return (
            simple(
                StatusCode::GATEWAY_TIMEOUT,
                "only-if-cached and not in cache",
            ),
            "miss",
        );
    }
    METRICS.cache_miss.fetch_add(1, Relaxed);
    let pass = state
        .pass
        .lock()
        .unwrap()
        .get(&key)
        .is_some_and(|t| *t > Instant::now());
    if pass {
        METRICS.cache_pass.fetch_add(1, Relaxed);
    }
    let r = if state.cfg.cache.collapse && !req_cc.no_store && !pass {
        collapsed_miss(state, key, &parts.uri, &parts.headers, head, ip).await
    } else {
        match fetch_for_cache(state, &key, &parts.uri, &parts.headers, None, ip).await {
            Ok(Fetched::Stored(e, _)) => {
                if pass {
                    state.pass.lock().unwrap().remove(&key); // cacheable again
                }
                serve_entry(&e, &parts.headers, head, "MISS")
            }
            Ok(Fetched::Passthrough(r)) => r,
            Err(err) => upstream_error(err),
        }
    };
    (r, if pass { "pass" } else { "miss" })
}

/// Request collapsing. The first miss on a key becomes the leader and fetches in a
/// detached task (so its client hanging up doesn't cancel the fill); concurrent misses
/// wait on a watch channel and are served the leader's stored entry.
async fn collapsed_miss(
    state: &Arc<State>,
    key: String,
    uri: &http::Uri,
    headers: &HeaderMap,
    head: bool,
    ip: IpAddr,
) -> Response<Body> {
    enum Role {
        Leader(watch::Sender<Option<Option<Arc<Entry>>>>),
        Follower(Slot),
    }
    let role = {
        let mut m = state.inflight.lock().unwrap();
        match m.get(&key) {
            Some(rx) => Role::Follower(rx.clone()),
            None => {
                let (tx, rx) = watch::channel(None);
                m.insert(key.clone(), rx);
                Role::Leader(tx)
            }
        }
    };
    match role {
        Role::Follower(mut rx) => {
            METRICS.cache_collapsed.fetch_add(1, Relaxed);
            let wait = Duration::from_millis(state.cfg.cache.collapse_timeout_ms);
            if let Ok(Ok(v)) = tokio::time::timeout(wait, rx.wait_for(|v| v.is_some())).await {
                if let Some(Some(e)) = v.clone() {
                    if e.matches_request(headers) {
                        return serve_entry(&e, headers, head, "COLLAPSED");
                    }
                }
            }
            // Leader timed out, failed, or got an unstorable response: go ourselves.
            match fetch_for_cache(state, &key, uri, headers, None, ip).await {
                Ok(Fetched::Stored(e, _)) => serve_entry(&e, headers, head, "MISS"),
                Ok(Fetched::Passthrough(r)) => r,
                Err(err) => upstream_error(err),
            }
        }
        Role::Leader(tx) => {
            let (otx, orx) = tokio::sync::oneshot::channel();
            let (st, k, u, h) = (state.clone(), key, uri.clone(), headers.clone());
            tokio::spawn(async move {
                let res = fetch_for_cache(&st, &k, &u, &h, None, ip).await;
                let shared = match &res {
                    Ok(Fetched::Stored(e, _)) => Some(e.clone()),
                    Ok(Fetched::Passthrough(r))
                        if st.cfg.cache.hit_for_pass && !r.status().is_server_error() =>
                    {
                        // Not storable: stop collapsing this key for a while.
                        let mut p = st.pass.lock().unwrap();
                        if p.len() > 100_000 {
                            let now = Instant::now();
                            p.retain(|_, t| *t > now);
                        }
                        p.insert(k.clone(), Instant::now() + HIT_FOR_PASS);
                        None
                    }
                    _ => None,
                };
                st.inflight.lock().unwrap().remove(&k);
                let _ = tx.send(Some(shared));
                let _ = otx.send(res);
            });
            match orx.await {
                Ok(Ok(Fetched::Stored(e, _))) => serve_entry(&e, headers, head, "MISS"),
                Ok(Ok(Fetched::Passthrough(r))) => r,
                Ok(Err(err)) => upstream_error(err),
                Err(_) => simple(StatusCode::BAD_GATEWAY, "fetch task failed"),
            }
        }
    }
}

fn spawn_revalidate(
    state: &Arc<State>,
    key: String,
    uri: http::Uri,
    headers: HeaderMap,
    e: Arc<Entry>,
    ip: IpAddr,
) {
    if !state.revalidating.lock().unwrap().insert(key.clone()) {
        return; // a refresh for this key is already running
    }
    let st = state.clone();
    tokio::spawn(async move {
        let _ = fetch_for_cache(&st, &key, &uri, &headers, Some(&e), ip).await;
        st.revalidating.lock().unwrap().remove(&key);
    });
}

enum Fetched {
    /// Stored (or freshened) entry; bool = it came from a 304 revalidation.
    Stored(Arc<Entry>, bool),
    /// Not storable: the origin's response, streamed straight through.
    Passthrough(Response<Body>),
}

/// GET from origin for the cache. With `old`, sends a conditional request and merges a 304.
async fn fetch_for_cache(
    state: &Arc<State>,
    key: &str,
    uri: &http::Uri,
    client_headers: &HeaderMap,
    old: Option<&Arc<Entry>>,
    ip: IpAddr,
) -> Result<Fetched, UpstreamError> {
    let mut h = client_headers.clone();
    // The cache, not the client, owns validators on a fill.
    for n in [
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_MATCH,
        header::IF_UNMODIFIED_SINCE,
        header::IF_RANGE,
        header::RANGE,
    ] {
        h.remove(n);
    }
    if let Some(e) = old {
        if let Some(t) = e.etag() {
            h.insert(header::IF_NONE_MATCH, t.clone());
        }
        if let Some(lm) = e.last_modified() {
            h.insert(header::IF_MODIFIED_SINCE, lm.clone());
        }
    }
    let req_cc = CacheControl::parse(client_headers);
    let request_time = SystemTime::now();
    let resp = forward(state, key, Method::GET, uri, h, None, ip).await?;
    let response_time = SystemTime::now();
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);

    if parts.status == StatusCode::NOT_MODIFIED {
        if let Some(e) = old {
            let n = cache::freshen(e, &parts.headers, request_time, response_time);
            let n = Arc::new(n);
            state.cache.put_arc(key, n.clone());
            return Ok(Fetched::Stored(n, true));
        }
    }
    let cc = CacheControl::parse_response(&parts.headers);
    let fits = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
        .is_none_or(|n| n <= state.cache.max_object_bytes);
    if fits
        && cache::storable(
            &Method::GET,
            client_headers,
            &req_cc,
            parts.status,
            &parts.headers,
            &cc,
        )
    {
        let bytes = match Limited::new(body, state.cache.max_object_bytes)
            .collect()
            .await
        {
            Ok(c) => c.to_bytes(),
            Err(_) => return Err(UpstreamError::Connect),
        };
        let e = Arc::new(cache::build_entry(
            parts.status,
            parts.headers,
            bytes,
            client_headers,
            request_time,
            response_time,
        ));
        state.cache.put_arc(key, e.clone());
        return Ok(Fetched::Stored(e, false));
    }
    // An origin can only invalidate by answering differently; a 2xx/3xx non-storable
    // response to a GET replaces nothing, but a stale entry should not outlive it.
    if old.is_some() && !parts.status.is_server_error() {
        state.cache.remove(key);
    }
    via(&mut parts.headers);
    parts
        .headers
        .insert("x-cache", HeaderValue::from_static("MISS"));
    Ok(Fetched::Passthrough(Response::from_parts(
        parts,
        body.boxed(),
    )))
}

/// Non-cacheable request: stream it through.
async fn bypass(
    state: &Arc<State>,
    req: Request<Incoming>,
    ip: IpAddr,
    key: &str,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let has_body = !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
    let body = has_body.then(|| body.boxed());
    match forward(
        state,
        key,
        parts.method,
        &parts.uri,
        parts.headers,
        body,
        ip,
    )
    .await
    {
        Ok(r) => {
            let (mut p, b) = r.into_parts();
            strip_hop_by_hop(&mut p.headers);
            via(&mut p.headers);
            p.headers
                .insert("x-cache", HeaderValue::from_static("BYPASS"));
            Response::from_parts(p, b.boxed())
        }
        Err(e) => upstream_error(e),
    }
}

/// One upstream exchange. Body-less requests get one retry on a different server
/// when the first fails to connect or times out; requests with a body do not.
async fn forward(
    state: &Arc<State>,
    key: &str,
    method: Method,
    uri: &http::Uri,
    mut headers: HeaderMap,
    mut body: Option<Body>,
    ip: IpAddr,
) -> Result<Response<Incoming>, UpstreamError> {
    strip_hop_by_hop(&mut headers);
    let xff = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {ip}"),
        None => ip.to_string(),
    };
    headers.insert(
        HeaderName::from_static("x-forwarded-for"),
        HeaderValue::from_str(&xff).unwrap(),
    );
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let attempts = if body.is_some() { 1 } else { 2 };
    let mut exclude = None;
    let mut last = UpstreamError::NoServer;
    for _ in 0..attempts {
        let Some((idx, _guard)) = state.pool.pick(key, exclude) else {
            break;
        };
        let addr = &state.pool.servers[idx].addr;
        let mut req = Request::builder()
            .method(method.clone())
            .uri(format!("http://{addr}{pq}"))
            .body(body.take().unwrap_or_else(empty))
            .unwrap();
        *req.headers_mut() = headers.clone();
        if !req.headers().contains_key(header::HOST) {
            req.headers_mut()
                .insert(header::HOST, HeaderValue::from_str(addr).unwrap());
        }
        METRICS.origin_requests.fetch_add(1, Relaxed);
        match tokio::time::timeout(state.cfg.upstream.timeout(), state.client.request(req)).await {
            Ok(Ok(r)) => {
                let bad = r.status().is_server_error();
                state.pool.report(idx, !bad);
                if bad {
                    METRICS.origin_errors.fetch_add(1, Relaxed);
                }
                return Ok(r);
            }
            Ok(Err(e)) => {
                tracing::debug!(upstream = %addr, err = %e, "upstream error");
                last = UpstreamError::Connect;
            }
            Err(_) => last = UpstreamError::Timeout,
        }
        METRICS.origin_errors.fetch_add(1, Relaxed);
        state.pool.report(idx, false);
        exclude = Some(idx);
    }
    Err(last)
}

fn upstream_error(e: UpstreamError) -> Response<Body> {
    match e {
        UpstreamError::NoServer => simple(StatusCode::SERVICE_UNAVAILABLE, "no healthy upstream"),
        UpstreamError::Timeout => simple(StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
        UpstreamError::Connect => simple(StatusCode::BAD_GATEWAY, "upstream connect error"),
    }
}

fn via(h: &mut HeaderMap) {
    h.append(header::VIA, HeaderValue::from_static("1.1 nano-edge"));
}

/// Serve a stored response, answering the client's own conditional request with 304.
fn serve_entry(e: &Entry, req: &HeaderMap, head: bool, tag: &'static str) -> Response<Body> {
    let now = SystemTime::now();
    let not_modified = match (req.get(header::IF_NONE_MATCH), e.etag()) {
        (Some(inm), Some(tag)) => {
            cache::etag_matches(inm.to_str().unwrap_or(""), tag.to_str().unwrap_or(""))
        }
        (Some(_), None) => false,
        (None, _) => match (req.get(header::IF_MODIFIED_SINCE), e.last_modified()) {
            (Some(ims), Some(lm)) => {
                let p = |v: &HeaderValue| {
                    v.to_str()
                        .ok()
                        .and_then(|s| httpdate::parse_http_date(s).ok())
                };
                matches!((p(ims), p(lm)), (Some(a), Some(b)) if b <= a)
            }
            _ => false,
        },
    } && e.status == StatusCode::OK;

    let mut r = if not_modified {
        let mut r = Response::new(empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        for n in [
            header::CACHE_CONTROL,
            header::CONTENT_LOCATION,
            header::DATE,
            header::ETAG,
            header::EXPIRES,
            header::VARY,
            header::LAST_MODIFIED,
        ] {
            if let Some(v) = e.headers.get(&n) {
                r.headers_mut().insert(n, v.clone());
            }
        }
        r
    } else {
        let mut r = Response::new(if head { empty() } else { full(e.body.clone()) });
        *r.status_mut() = e.status;
        *r.headers_mut() = e.headers.clone();
        if head {
            // Keep the GET's Content-Length on HEAD.
            r.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(e.body.len()));
        }
        r
    };
    r.headers_mut()
        .insert(header::AGE, HeaderValue::from(e.age(now)));
    via(r.headers_mut());
    r.headers_mut()
        .insert("x-cache", HeaderValue::from_static(tag));
    r
}
