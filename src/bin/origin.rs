//! Test origin for benchmarks and integration tests.
//!
//! `origin <port> [delay_ms]`
//!   GET /obj/<n>?size=B&max_age=S&swr=S   cacheable object with ETag; honours If-None-Match
//!   GET /nostore/<n>?size=B                 Cache-Control: no-store
//!   GET /health                             200
//!   GET /stats                              requests served since start (or last reset)
//!   GET /admin/delay?ms=N                   change the artificial delay at runtime
//!   GET /admin/reset                        zero the counter
//! Every response carries `x-origin: <port>`.

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

struct St {
    port: u16,
    hits: AtomicU64,
    delay_ms: AtomicU64,
}

fn q(uri: &http::Uri, k: &str) -> Option<u64> {
    uri.query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse().ok())
}

async fn handle(
    st: Arc<St>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();
    let resp = |status: StatusCode, body: Bytes| {
        let mut r = Response::new(Full::new(body));
        *r.status_mut() = status;
        r.headers_mut().insert("x-origin", st.port.into());
        r
    };
    match path.as_str() {
        "/health" => return Ok(resp(StatusCode::OK, Bytes::from_static(b"ok"))),
        "/stats" => {
            return Ok(resp(
                StatusCode::OK,
                Bytes::from(st.hits.load(Relaxed).to_string()),
            ))
        }
        "/admin/reset" => {
            st.hits.store(0, Relaxed);
            return Ok(resp(StatusCode::OK, Bytes::new()));
        }
        "/admin/delay" => {
            st.delay_ms.store(q(req.uri(), "ms").unwrap_or(0), Relaxed);
            return Ok(resp(StatusCode::OK, Bytes::new()));
        }
        _ => {}
    }
    st.hits.fetch_add(1, Relaxed);
    let d = q(req.uri(), "delay").unwrap_or(st.delay_ms.load(Relaxed));
    if d > 0 {
        tokio::time::sleep(Duration::from_millis(d)).await;
    }
    let size = q(req.uri(), "size").unwrap_or(1024) as usize;
    let etag = format!("\"{}-{size}\"", path.len());
    let mut r = resp(StatusCode::OK, Bytes::from(vec![b'x'; size]));
    let h = r.headers_mut();
    h.insert("content-type", "application/octet-stream".parse().unwrap());
    h.insert(
        "date",
        httpdate::fmt_http_date(std::time::SystemTime::now())
            .parse()
            .unwrap(),
    );
    if path.starts_with("/nostore/") {
        h.insert("cache-control", "no-store".parse().unwrap());
    } else {
        let max_age = q(req.uri(), "max_age").unwrap_or(60);
        let cc = match q(req.uri(), "swr") {
            Some(s) => format!("public, max-age={max_age}, stale-while-revalidate={s}"),
            None => format!("public, max-age={max_age}"),
        };
        h.insert("cache-control", cc.parse().unwrap());
        h.insert("etag", etag.parse().unwrap());
        if req
            .headers()
            .get("if-none-match")
            .and_then(|v| v.to_str().ok())
            == Some(etag.as_str())
        {
            let mut nm = resp(StatusCode::NOT_MODIFIED, Bytes::new());
            nm.headers_mut().insert("etag", etag.parse().unwrap());
            nm.headers_mut()
                .insert("cache-control", r.headers()["cache-control"].clone());
            return Ok(nm);
        }
    }
    Ok(r)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(9001);
    let delay: u64 = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let st = Arc::new(St {
        port,
        hits: AtomicU64::new(0),
        delay_ms: AtomicU64::new(delay),
    });
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind");
    eprintln!("origin listening on 127.0.0.1:{port} delay={delay}ms");
    loop {
        let Ok((s, _)) = l.accept().await else {
            continue;
        };
        let _ = s.set_nodelay(true);
        let st = st.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |r| handle(st.clone(), r));
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(s), svc)
                .await;
        });
    }
}
