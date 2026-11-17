//! Shared HTTP cache following RFC 9111, kept deliberately small.
//!
//! What is implemented:
//! * storability for a *shared* cache (§3): no-store, private, Authorization, explicit
//!   freshness (s-maxage > max-age > Expires) or heuristic freshness from Last-Modified (§4.2.2)
//! * age calculation (§4.2.3) and the `Age` header on every hit
//! * revalidation with If-None-Match / If-Modified-Since and 304 header merging (§4.3)
//! * request directives no-cache, no-store, max-age, only-if-cached
//! * stale-while-revalidate and stale-if-error (RFC 5861)
//! * Vary (secondary keys), invalidation on unsafe methods (§4.4)
//! * byte-bounded LRU, sharded to keep lock hold times short
//!
//! What is not: Range/206 storage, max-stale/min-fresh, Content-Location invalidation.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const SHARDS: usize = 16;

/// Parsed Cache-Control. Only the directives we act on.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CacheControl {
    pub no_store: bool,
    pub no_cache: bool,
    pub private: bool,
    pub public: bool,
    pub must_revalidate: bool,
    pub proxy_revalidate: bool,
    pub only_if_cached: bool,
    pub max_age: Option<u64>,
    pub s_maxage: Option<u64>,
    pub stale_while_revalidate: Option<u64>,
    pub stale_if_error: Option<u64>,
}

impl CacheControl {
    pub fn parse(headers: &HeaderMap) -> CacheControl {
        Self::parse_field(headers, http::header::CACHE_CONTROL.as_str())
    }

    /// Response directives for this cache. RFC 9213: when `CDN-Cache-Control` is
    /// present, a CDN uses it *instead of* Cache-Control (which then governs only
    /// downstream caches and browsers).
    pub fn parse_response(headers: &HeaderMap) -> CacheControl {
        // CDN-Cache-Control is a Structured Field (RFC 8941 dictionary); an invalid one
        // is ignored as a whole and Cache-Control applies instead.
        let cdn: Vec<&str> = headers
            .get_all("cdn-cache-control")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        if !cdn.is_empty() && cdn.iter().all(|v| sf_dictionary_ok(v)) {
            Self::parse_field(headers, "cdn-cache-control")
        } else {
            Self::parse(headers)
        }
    }

    fn parse_field(headers: &HeaderMap, field: &str) -> CacheControl {
        let mut cc = CacheControl::default();
        for v in headers.get_all(field) {
            let Ok(s) = v.to_str() else { continue };
            for part in split_list(s) {
                let (name, val) = match part.split_once('=') {
                    Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"'))),
                    None => (part.trim(), None),
                };
                // §5.2: a malformed delta-seconds makes the directive fail safe.
                let secs = || val.and_then(|v| v.parse::<u64>().ok());
                match name.to_ascii_lowercase().as_str() {
                    "no-store" => cc.no_store = true,
                    // `no-cache="Set-Cookie"` (qualified form) is treated like plain no-cache.
                    "no-cache" => cc.no_cache = true,
                    "private" => cc.private = true,
                    "public" => cc.public = true,
                    "must-revalidate" => cc.must_revalidate = true,
                    "proxy-revalidate" => cc.proxy_revalidate = true,
                    "only-if-cached" => cc.only_if_cached = true,
                    // A duplicated or invalid max-age is treated as stale (max-age=0).
                    "max-age" => {
                        cc.max_age = Some(if cc.max_age.is_some() {
                            0
                        } else {
                            secs().unwrap_or(0)
                        })
                    }
                    "s-maxage" => {
                        cc.s_maxage = Some(if cc.s_maxage.is_some() {
                            0
                        } else {
                            secs().unwrap_or(0)
                        })
                    }
                    "stale-while-revalidate" => cc.stale_while_revalidate = secs(),
                    "stale-if-error" => cc.stale_if_error = secs(),
                    _ => {}
                }
            }
        }
        cc
    }
}

/// Minimal RFC 8941 dictionary check for CDN-Cache-Control: keys are lowercase tokens,
/// and the delta-seconds directives must be bare integers (not strings).
fn sf_dictionary_ok(v: &str) -> bool {
    split_list(v).iter().all(|m| {
        let (k, val) = match m.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (m.trim(), None),
        };
        let key_ok = k.starts_with(|c: char| c.is_ascii_lowercase() || c == '*')
            && k.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.*".contains(c));
        let int_key = matches!(
            k,
            "max-age" | "s-maxage" | "stale-while-revalidate" | "stale-if-error"
        );
        let val_ok = match val {
            None => true,
            Some(x) if int_key => {
                !x.is_empty()
                    && x.trim_start_matches('-')
                        .chars()
                        .all(|c| c.is_ascii_digit())
            }
            Some(x) => !x.is_empty(),
        };
        key_ok && val_ok
    })
}

/// Split a comma-separated header list, ignoring commas inside quoted strings.
fn split_list(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let (mut start, mut quoted) = (0, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

fn date_header(h: &HeaderMap, name: HeaderName) -> Option<SystemTime> {
    h.get(name)?
        .to_str()
        .ok()
        .and_then(|s| httpdate::parse_http_date(s).ok())
}

/// Status codes that are "heuristically cacheable" (RFC 9110 §15.1). 206 omitted: no range support.
fn heuristically_cacheable(s: StatusCode) -> bool {
    matches!(
        s.as_u16(),
        200 | 203 | 204 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

/// Statuses whose semantics we understand well enough to store at all.
fn understood(s: StatusCode) -> bool {
    matches!(
        s.as_u16(),
        200 | 203 | 204 | 300 | 301 | 302 | 303 | 307 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

pub struct Entry {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// Request header values the response varies on, as (name, value-or-empty).
    pub vary: Vec<(HeaderName, String)>,
    pub request_time: SystemTime,
    pub response_time: SystemTime,
    pub cc: CacheControl,
    /// §4.2.1 freshness lifetime in seconds.
    pub freshness: u64,
}

impl Entry {
    /// §4.2.3 current_age.
    pub fn age(&self, now: SystemTime) -> u64 {
        let date = date_header(&self.headers, http::header::DATE).unwrap_or(self.response_time);
        // §5.1: an Age that isn't a non-negative integer is ignored.
        let age_value = self
            .headers
            .get(http::header::AGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let secs = |d: Result<Duration, _>| d.map(|d: Duration| d.as_secs()).unwrap_or(0);
        let apparent_age = secs(self.response_time.duration_since(date));
        let response_delay = secs(self.response_time.duration_since(self.request_time));
        let corrected_initial_age = apparent_age.max(age_value + response_delay);
        let resident = secs(now.duration_since(self.response_time));
        corrected_initial_age + resident
    }

    pub fn is_fresh(&self, now: SystemTime) -> bool {
        !self.cc.no_cache && self.age(now) < self.freshness
    }

    /// Seconds past expiry (0 while fresh).
    pub fn staleness(&self, now: SystemTime) -> u64 {
        self.age(now).saturating_sub(self.freshness)
    }

    /// Stale content may be served while a background refresh runs.
    pub fn can_swr(&self, now: SystemTime) -> bool {
        !self.must_revalidate()
            && !self.cc.no_cache
            && self
                .cc
                .stale_while_revalidate
                .is_some_and(|w| self.staleness(now) <= w)
    }

    /// Stale content may be served when the origin fails (RFC 5861 §4).
    pub fn can_sie(&self, now: SystemTime, req: &CacheControl) -> bool {
        if self.must_revalidate() {
            return false;
        }
        let window = req.stale_if_error.or(self.cc.stale_if_error);
        window.is_some_and(|w| self.staleness(now) <= w)
    }

    fn must_revalidate(&self) -> bool {
        self.cc.must_revalidate || self.cc.proxy_revalidate || self.cc.s_maxage.is_some()
    }

    pub fn etag(&self) -> Option<&HeaderValue> {
        self.headers.get(http::header::ETAG)
    }

    pub fn last_modified(&self) -> Option<&HeaderValue> {
        self.headers.get(http::header::LAST_MODIFIED)
    }

    /// Does this stored variant answer a request with these headers (Vary match)?
    pub fn matches_request(&self, req: &HeaderMap) -> bool {
        self.matches_vary(req)
    }

    fn matches_vary(&self, req: &HeaderMap) -> bool {
        self.vary.iter().all(|(n, v)| vary_value(req, n) == *v)
    }

    fn size(&self) -> usize {
        self.body.len()
            + self
                .headers
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 32)
                .sum::<usize>()
            + 256
    }
}

fn vary_value(req: &HeaderMap, name: &HeaderName) -> String {
    // Normalise by joining all field lines; whitespace-insensitive comparison is good enough here.
    let vals: Vec<&str> = req
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    vals.join(",")
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(",")
}

/// §4.2.1: freshness lifetime for a shared cache.
pub fn freshness_lifetime(
    status: StatusCode,
    h: &HeaderMap,
    cc: &CacheControl,
    now: SystemTime,
) -> u64 {
    if let Some(s) = cc.s_maxage {
        return s;
    }
    if let Some(m) = cc.max_age {
        return m;
    }
    if let Some(exp) = h.get(http::header::EXPIRES) {
        // An invalid Expires (e.g. "0") means already expired.
        let Some(exp) = exp
            .to_str()
            .ok()
            .and_then(|s| httpdate::parse_http_date(s).ok())
        else {
            return 0;
        };
        let date = date_header(h, http::header::DATE).unwrap_or(now);
        return exp.duration_since(date).map(|d| d.as_secs()).unwrap_or(0);
    }
    // §4.2.2 heuristic: 10% of the time since Last-Modified, capped at one day.
    if heuristically_cacheable(status) || cc.public {
        if let Some(lm) = date_header(h, http::header::LAST_MODIFIED) {
            let date = date_header(h, http::header::DATE).unwrap_or(now);
            let since = date.duration_since(lm).map(|d| d.as_secs()).unwrap_or(0);
            return (since / 10).min(86_400);
        }
    }
    0
}

/// §3: may a shared cache store this response?
pub fn storable(
    method: &Method,
    req: &HeaderMap,
    req_cc: &CacheControl,
    status: StatusCode,
    resp: &HeaderMap,
    cc: &CacheControl,
) -> bool {
    if *method != Method::GET || req_cc.no_store || cc.no_store || cc.private || !understood(status)
    {
        return false;
    }
    if resp
        .get_all(http::header::VARY)
        .iter()
        .any(|v| v.to_str().map(|s| s.contains('*')).unwrap_or(true))
    {
        return false;
    }
    // §3.5: responses to authenticated requests are only stored when explicitly allowed.
    if req.contains_key(http::header::AUTHORIZATION)
        && !(cc.public || cc.s_maxage.is_some() || cc.must_revalidate)
    {
        return false;
    }
    cc.public
        || cc.max_age.is_some()
        || cc.s_maxage.is_some()
        || cc.no_cache // stored, but always revalidated
        || resp.contains_key(http::header::EXPIRES)
        || (heuristically_cacheable(status) && resp.contains_key(http::header::LAST_MODIFIED))
}

pub fn build_entry(
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    req: &HeaderMap,
    request_time: SystemTime,
    response_time: SystemTime,
) -> Entry {
    let cc = CacheControl::parse_response(&headers);
    let freshness = freshness_lifetime(status, &headers, &cc, response_time);
    let mut vary = vec![];
    for v in headers.get_all(http::header::VARY) {
        if let Ok(s) = v.to_str() {
            for name in split_list(s) {
                if let Ok(n) = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()) {
                    let val = vary_value(req, &n);
                    vary.push((n, val));
                }
            }
        }
    }
    Entry {
        status,
        headers,
        body,
        vary,
        request_time,
        response_time,
        cc,
        freshness,
    }
}

/// §4.3.4: freshen a stored response with the headers of a 304.
pub fn freshen(
    old: &Entry,
    not_modified: &HeaderMap,
    request_time: SystemTime,
    response_time: SystemTime,
) -> Entry {
    let mut headers = old.headers.clone();
    for name in not_modified.keys() {
        if matches!(
            name.as_str(),
            "content-length" | "content-encoding" | "transfer-encoding" | "content-range"
        ) {
            continue;
        }
        headers.remove(name);
        for v in not_modified.get_all(name) {
            headers.append(name.clone(), v.clone());
        }
    }
    // A 304 without an Age header means the stored Age no longer applies.
    if !not_modified.contains_key(http::header::AGE) {
        headers.remove(http::header::AGE);
    }
    let cc = CacheControl::parse_response(&headers);
    let freshness = freshness_lifetime(old.status, &headers, &cc, response_time);
    Entry {
        status: old.status,
        headers,
        body: old.body.clone(),
        vary: old.vary.clone(),
        request_time,
        response_time,
        cc,
        freshness,
    }
}

struct Shard {
    lru: LruCache<String, Vec<Arc<Entry>>>,
    bytes: usize,
}

pub struct Cache {
    shards: Vec<Mutex<Shard>>,
    max_bytes_per_shard: usize,
    pub max_object_bytes: usize,
}

impl Cache {
    pub fn new(max_bytes: usize, max_object_bytes: usize) -> Cache {
        Cache {
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(Shard {
                        lru: LruCache::unbounded(),
                        bytes: 0,
                    })
                })
                .collect(),
            max_bytes_per_shard: (max_bytes / SHARDS).max(1),
            max_object_bytes,
        }
    }

    fn shard(&self, key: &str) -> &Mutex<Shard> {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        &self.shards[(h.finish() as usize) % SHARDS]
    }

    pub fn get(&self, key: &str, req: &HeaderMap) -> Option<Arc<Entry>> {
        let mut s = self.shard(key).lock().unwrap();
        s.lru
            .get(key)?
            .iter()
            .find(|e| e.matches_vary(req))
            .cloned()
    }

    pub fn put(&self, key: &str, entry: Entry) {
        self.put_arc(key, Arc::new(entry))
    }

    pub fn put_arc(&self, key: &str, entry: Arc<Entry>) {
        let size = entry.size();
        if size > self.max_object_bytes {
            return;
        }
        let mut s = self.shard(key).lock().unwrap();
        let mut freed = 0;
        if let Some(variants) = s.lru.get_mut(key) {
            // Replace the variant with the same Vary values (or the only one when not varying).
            variants.retain(|e| {
                let same = e.vary == entry.vary || e.vary.is_empty() || entry.vary.is_empty();
                if same {
                    freed += e.size();
                }
                !same
            });
            variants.push(entry);
        } else {
            s.lru.put(key.to_string(), vec![entry]);
        }
        s.bytes = s.bytes + size - freed;
        while s.bytes > self.max_bytes_per_shard {
            match s.lru.pop_lru() {
                Some((_, v)) => s.bytes -= v.iter().map(|e| e.size()).sum::<usize>(),
                None => break,
            }
        }
    }

    pub fn remove(&self, key: &str) {
        let mut s = self.shard(key).lock().unwrap();
        if let Some(v) = s.lru.pop(key) {
            s.bytes -= v.iter().map(|e| e.size()).sum::<usize>();
        }
    }

    pub fn bytes(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().bytes).sum()
    }
}

/// RFC 9110 §13.1.2 weak comparison for If-None-Match.
pub fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let e = strip(etag);
    split_list(if_none_match)
        .iter()
        .any(|t| t.trim() == "*" || strip(t) == e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_directives() {
        let cc = CacheControl::parse(&hm(&[
            (
                "cache-control",
                "public, max-age=60, stale-while-revalidate=30",
            ),
            ("cache-control", "s-maxage=\"10\""),
        ]));
        assert!(cc.public);
        assert_eq!(cc.max_age, Some(60));
        assert_eq!(cc.s_maxage, Some(10));
        assert_eq!(cc.stale_while_revalidate, Some(30));
    }

    #[test]
    fn duplicate_max_age_is_stale() {
        let cc = CacheControl::parse(&hm(&[("cache-control", "max-age=60, max-age=3600")]));
        assert_eq!(cc.max_age, Some(0));
    }

    #[test]
    fn s_maxage_beats_max_age() {
        let h = hm(&[("cache-control", "max-age=10, s-maxage=99")]);
        let cc = CacheControl::parse(&h);
        assert_eq!(
            freshness_lifetime(StatusCode::OK, &h, &cc, SystemTime::now()),
            99
        );
    }

    #[test]
    fn heuristic_freshness_is_ten_percent() {
        let now = SystemTime::now();
        let lm = now - Duration::from_secs(1000);
        let h = hm(&[
            ("date", &httpdate::fmt_http_date(now)),
            ("last-modified", &httpdate::fmt_http_date(lm)),
        ]);
        let cc = CacheControl::parse(&h);
        assert_eq!(freshness_lifetime(StatusCode::OK, &h, &cc, now), 100);
        assert!(storable(
            &Method::GET,
            &HeaderMap::new(),
            &CacheControl::default(),
            StatusCode::OK,
            &h,
            &cc
        ));
    }

    #[test]
    fn age_includes_upstream_age_and_residence() {
        let t0 = SystemTime::now() - Duration::from_secs(20);
        let h = hm(&[("age", "30"), ("date", &httpdate::fmt_http_date(t0))]);
        let e = build_entry(StatusCode::OK, h, Bytes::new(), &HeaderMap::new(), t0, t0);
        let a = e.age(t0 + Duration::from_secs(20));
        assert_eq!(a, 50);
    }

    #[test]
    fn private_and_no_store_not_stored() {
        for v in ["private, max-age=60", "no-store, max-age=60"] {
            let h = hm(&[("cache-control", v)]);
            let cc = CacheControl::parse(&h);
            assert!(!storable(
                &Method::GET,
                &HeaderMap::new(),
                &CacheControl::default(),
                StatusCode::OK,
                &h,
                &cc
            ));
        }
    }

    #[test]
    fn vary_selects_variant() {
        let c = Cache::new(1 << 20, 1 << 20);
        let now = SystemTime::now();
        let resp = hm(&[("cache-control", "max-age=60"), ("vary", "Accept-Encoding")]);
        let gz = hm(&[("accept-encoding", "gzip")]);
        let br = hm(&[("accept-encoding", "br")]);
        c.put(
            "k",
            build_entry(
                StatusCode::OK,
                resp.clone(),
                Bytes::from_static(b"gz"),
                &gz,
                now,
                now,
            ),
        );
        c.put(
            "k",
            build_entry(
                StatusCode::OK,
                resp,
                Bytes::from_static(b"br"),
                &br,
                now,
                now,
            ),
        );
        assert_eq!(&c.get("k", &gz).unwrap().body[..], b"gz");
        assert_eq!(&c.get("k", &br).unwrap().body[..], b"br");
        assert!(c.get("k", &HeaderMap::new()).is_none());
    }

    #[test]
    fn lru_evicts_by_bytes() {
        let c = Cache::new(SHARDS * 4096, 4096);
        let now = SystemTime::now();
        let h = hm(&[("cache-control", "max-age=60")]);
        for i in 0..200 {
            c.put(
                &format!("k{i}"),
                build_entry(
                    StatusCode::OK,
                    h.clone(),
                    Bytes::from(vec![0u8; 1000]),
                    &HeaderMap::new(),
                    now,
                    now,
                ),
            );
        }
        assert!(c.bytes() <= SHARDS * 4096);
        assert!(c.get("k199", &HeaderMap::new()).is_some());
    }

    #[test]
    fn cdn_cache_control_overrides_cache_control() {
        let h = hm(&[
            ("cache-control", "no-store"),
            ("cdn-cache-control", "max-age=30"),
        ]);
        let cc = CacheControl::parse_response(&h);
        assert!(!cc.no_store);
        assert_eq!(
            freshness_lifetime(StatusCode::OK, &h, &cc, SystemTime::now()),
            30
        );
        assert!(storable(
            &Method::GET,
            &HeaderMap::new(),
            &CacheControl::default(),
            StatusCode::OK,
            &h,
            &cc
        ));
    }

    #[test]
    fn invalid_cdn_cache_control_is_ignored() {
        for bad in ["max-age=\"10000\"", "max-age=10000, &&&&&"] {
            let h = hm(&[("cdn-cache-control", bad)]);
            assert_eq!(
                CacheControl::parse_response(&h),
                CacheControl::default(),
                "{bad}"
            );
        }
    }

    #[test]
    fn weak_etag_compare() {
        assert!(etag_matches("W/\"a\", \"b\"", "\"a\""));
        assert!(!etag_matches("\"c\"", "\"a\""));
    }
}
