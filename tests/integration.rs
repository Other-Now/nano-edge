//! End-to-end tests: real `origin` processes, the proxy in-process on an ephemeral port.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use nano_edge::config::{Config, LbPolicy};
use nano_edge::metrics::METRICS;
use nano_edge::{proxy::State, server};
use std::process::{Child, Command};
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Origin {
    child: Child,
    port: u16,
}
impl Drop for Origin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn origin(port: u16) -> Origin {
    let child = Command::new(env!("CARGO_BIN_EXE_origin"))
        .arg(port.to_string())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Owned by the guard from here, so it is killed and reaped on every path.
    let mut o = Origin { child, port };
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return o;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = o.child.kill();
    let _ = o.child.wait();
    panic!("origin {port} did not start");
}

fn cfg(origins: &[&Origin]) -> Config {
    let mut c = Config {
        listen: "127.0.0.1:0".into(),
        ..Default::default()
    };
    c.upstream.servers = origins
        .iter()
        .map(|o| format!("127.0.0.1:{}", o.port))
        .collect();
    c
}

async fn proxy(c: Config) -> String {
    let st = State::new(c);
    let addr = server::start(st).await.unwrap();
    format!("http://{addr}")
}

type Resp = (u16, http::HeaderMap, Bytes);

async fn req(method: &str, url: &str, headers: &[(&str, &str)]) -> Resp {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let mut b = http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let r = client
        .request(b.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap();
    let (p, body) = r.into_parts();
    (
        p.status.as_u16(),
        p.headers,
        body.collect().await.unwrap().to_bytes(),
    )
}

async fn get(url: &str) -> Resp {
    req("GET", url, &[]).await
}

async fn origin_hits(o: &Origin) -> u64 {
    let (_, _, b) = get(&format!("http://127.0.0.1:{}/stats", o.port)).await;
    std::str::from_utf8(&b).unwrap().parse().unwrap()
}

fn xcache(r: &Resp) -> &str {
    r.1.get("x-cache")
        .map(|v| v.to_str().unwrap())
        .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn miss_then_hit_with_age() {
    let o = origin(19101).await;
    let p = proxy(cfg(&[&o])).await;
    let a = get(&format!("{p}/obj/1?size=100")).await;
    assert_eq!((a.0, xcache(&a)), (200, "MISS"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let b = get(&format!("{p}/obj/1?size=100")).await;
    assert_eq!(xcache(&b), "HIT");
    assert_eq!(b.2.len(), 100);
    assert!(b.1["age"].to_str().unwrap().parse::<u64>().unwrap() >= 1);
    assert_eq!(origin_hits(&o).await, 1);
}

async fn stampede(port: u16, collapse: bool) -> u64 {
    let o = origin(port).await;
    let mut c = cfg(&[&o]);
    c.cache.collapse = collapse;
    let p = proxy(c).await;
    let url = format!("{p}/obj/hot?delay=300");
    let tasks: Vec<_> = (0..1000)
        .map(|_| {
            tokio::spawn({
                let u = url.clone();
                async move { get(&u).await.0 }
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    origin_hits(&o).await
}

#[tokio::test(flavor = "multi_thread")]
async fn stampede_collapses_to_one_origin_fetch() {
    assert_eq!(stampede(19102, true).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn stampede_without_collapsing_hits_origin_many_times() {
    let n = stampede(19103, false).await;
    assert!(n > 100, "only {n} origin fetches");
}

async fn concurrent_uncacheable(port: u16, hit_for_pass: bool) -> Duration {
    let o = origin(port).await;
    let mut c = cfg(&[&o]);
    c.cache.hit_for_pass = hit_for_pass;
    let p = proxy(c).await;
    let u = format!("{p}/nostore/hfp?delay=300");
    get(&u).await; // learn that the key is not storable
    let t = Instant::now();
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            tokio::spawn({
                let u = u.clone();
                async move { get(&u).await.0 }
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    t.elapsed()
}

#[tokio::test(flavor = "multi_thread")]
async fn hit_for_pass_stops_serialising_uncacheable_keys() {
    // Without it, followers wait for the leader's 300 ms fetch and then make their own.
    let with = concurrent_uncacheable(19115, true).await;
    let without = concurrent_uncacheable(19116, false).await;
    assert!(
        with < Duration::from_millis(500),
        "with hit-for-pass took {with:?}"
    );
    assert!(
        without >= Duration::from_millis(590),
        "without hit-for-pass took {without:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_entry_revalidates_with_304() {
    let o = origin(19104).await;
    let p = proxy(cfg(&[&o])).await;
    let u = format!("{p}/obj/rv?max_age=1&size=500");
    get(&u).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let r = get(&u).await;
    assert_eq!(xcache(&r), "REVALIDATED");
    assert_eq!(r.2.len(), 500, "body must come from the cache after a 304");
    assert_eq!(xcache(&get(&u).await), "HIT");
    assert_eq!(origin_hits(&o).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_while_revalidate_serves_instantly() {
    let o = origin(19105).await;
    let p = proxy(cfg(&[&o])).await;
    let u = format!("{p}/obj/swr?max_age=1&swr=30&delay=400");
    get(&u).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let t = Instant::now();
    let r = get(&u).await;
    assert_eq!(xcache(&r), "STALE");
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "stale response waited for origin"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(xcache(&get(&u).await), "HIT");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_conditional_gets_304_from_cache() {
    let o = origin(19106).await;
    let p = proxy(cfg(&[&o])).await;
    let u = format!("{p}/obj/cond");
    let a = get(&u).await;
    let etag = a.1["etag"].to_str().unwrap().to_string();
    let b = req("GET", &u, &[("if-none-match", &etag)]).await;
    assert_eq!(b.0, 304);
    assert_eq!(origin_hits(&o).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_store_is_never_cached() {
    let o = origin(19107).await;
    let p = proxy(cfg(&[&o])).await;
    for _ in 0..3 {
        get(&format!("{p}/nostore/1")).await;
    }
    assert_eq!(origin_hits(&o).await, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_invalidates_cached_uri() {
    let o = origin(19108).await;
    let p = proxy(cfg(&[&o])).await;
    let u = format!("{p}/obj/inv");
    get(&u).await;
    assert_eq!(xcache(&get(&u).await), "HIT");
    req("POST", &u, &[]).await;
    assert_eq!(xcache(&get(&u).await), "MISS");
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_origin_is_retried_then_ejected() {
    let a = origin(19109).await;
    let mut b = origin(19110).await;
    let mut c = cfg(&[&a, &b]);
    c.upstream.policy = LbPolicy::RoundRobin;
    let p = proxy(c).await;
    let _ = b.child.kill();
    let _ = b.child.wait();
    let before = METRICS.upstream_ejections.load(Relaxed);
    for i in 0..20 {
        let r = get(&format!("{p}/nostore/{i}")).await;
        assert_eq!(
            r.0, 200,
            "request {i} failed; retry should have covered the dead origin"
        );
        assert_eq!(r.1["x-origin"], "19109");
    }
    assert!(METRICS.upstream_ejections.load(Relaxed) > before);
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_returns_429() {
    let o = origin(19111).await;
    let mut c = cfg(&[&o]);
    c.limits.rate_per_sec = 5.0;
    c.limits.burst = 5.0;
    let p = proxy(c).await;
    let mut codes = vec![];
    for _ in 0..10 {
        codes.push(get(&format!("{p}/obj/rl")).await.0);
    }
    assert_eq!(codes.iter().filter(|&&c| c == 200).count(), 5);
    assert_eq!(codes.iter().filter(|&&c| c == 429).count(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn waf_blocks_sqli() {
    let o = origin(19112).await;
    let p = proxy(cfg(&[&o])).await;
    assert_eq!(get(&format!("{p}/obj/1?id=1%27%20OR%201=1")).await.0, 403);
    assert_eq!(origin_hits(&o).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn slowloris_connection_is_closed() {
    let o = origin(19113).await;
    let mut c = cfg(&[&o]);
    c.limits.header_read_timeout_ms = 500;
    let p = proxy(c).await;
    let addr = p.trim_start_matches("http://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let t = Instant::now();
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
    // Trickle a header byte every 100 ms, never finishing the request.
    let closed = async {
        let mut buf = [0u8; 256];
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if s.write_all(b"X").await.is_err() {
                return;
            }
            if let Ok(Ok(0)) | Ok(Err(_)) =
                tokio::time::timeout(Duration::from_millis(1), s.read(&mut buf)).await
            {
                return;
            }
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(1), s.read(&mut buf)).await
            {
                if n > 0 && buf.starts_with(b"HTTP/1.1 408") {
                    return;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .expect("slowloris connection stayed open");
    assert!(t.elapsed() >= Duration::from_millis(450));
}

mod tls {
    use super::*;
    use rustls::pki_types::{CertificateDer, ServerName};
    use std::sync::Arc;

    fn write_cert(dir: &std::path::Path, file: &str, name: &str) -> CertificateDer<'static> {
        let ck = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        std::fs::write(dir.join(format!("{file}.key")), ck.key_pair.serialize_pem()).unwrap();
        std::fs::write(dir.join(format!("{file}.crt")), ck.cert.pem()).unwrap();
        ck.cert.der().clone()
    }

    async fn served_cert(addr: &str, sni: &str) -> CertificateDer<'static> {
        #[derive(Debug)]
        struct AcceptAny;
        impl rustls::client::danger::ServerCertVerifier for AcceptAny {
            fn verify_server_cert(
                &self,
                _: &CertificateDer,
                _: &[CertificateDer],
                _: &ServerName,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &CertificateDer,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &CertificateDer,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                rustls::crypto::ring::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let s = tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(ServerName::try_from(sni.to_string()).unwrap(), tcp)
            .await
            .unwrap();
        s.get_ref().1.peer_certificates().unwrap()[0]
            .clone()
            .into_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sni_selects_cert_and_hot_reloads() {
        let dir = std::env::temp_dir().join(format!("nano-edge-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let def = write_cert(&dir, "default", "default.test");
        let a = write_cert(&dir, "a.test", "a.test");
        let wild = write_cert(&dir, "_wildcard.cdn.test", "*.cdn.test");

        let o = origin(19114).await;
        let mut c = cfg(&[&o]);
        c.tls_listen = Some("127.0.0.1:19443".into());
        c.tls_cert_dir = Some(dir.clone());
        proxy(c).await;
        let addr = "127.0.0.1:19443";
        assert_eq!(served_cert(addr, "a.test").await, a);
        assert_eq!(served_cert(addr, "img.cdn.test").await, wild);
        assert_eq!(served_cert(addr, "unknown.test").await, def);

        // Hot reload: add a new name and rotate a.test while the listener runs.
        let b = write_cert(&dir, "b.test", "b.test");
        let a2 = write_cert(&dir, "a.test", "a.test");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if served_cert(addr, "b.test").await == b && served_cert(addr, "a.test").await == a2 {
                break;
            }
            assert!(Instant::now() < deadline, "certificates were not reloaded");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
