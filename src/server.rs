//! Accept loops. Connection-level defences live here:
//! * global connection cap (a semaphore; over the cap the socket is closed at once)
//! * per-IP connection cap
//! * header read timeout, which is the slowloris defence: hyper closes a connection
//!   whose request headers have not fully arrived in time
//! * TLS handshake timeout, the same defence one layer down

use crate::metrics::METRICS;
use crate::proxy::{self, State};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;

struct ActiveConn;
impl ActiveConn {
    fn new() -> ActiveConn {
        METRICS.conn_active.fetch_add(1, Relaxed);
        ActiveConn
    }
}
impl Drop for ActiveConn {
    fn drop(&mut self) {
        METRICS.conn_active.fetch_sub(1, Relaxed);
    }
}

pub async fn serve(state: Arc<State>, listener: TcpListener, tls: Option<TlsAcceptor>) {
    let sem = Arc::new(Semaphore::new(state.cfg.limits.max_connections));
    let hrt = Duration::from_millis(state.cfg.limits.header_read_timeout_ms);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // EMFILE and friends: back off rather than spin.
                tracing::warn!(err = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = sem.clone().try_acquire_owned() else {
            METRICS.conn_rejected.fetch_add(1, Relaxed);
            continue; // dropping the stream closes it
        };
        let Some(ip_guard) = state.conns.acquire(peer.ip()) else {
            METRICS.conn_rejected.fetch_add(1, Relaxed);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let (state, tls) = (state.clone(), tls.clone());
        let fut = async move {
            let _active = ActiveConn::new();
            let _permit = permit;
            let _ip = ip_guard;
            match tls {
                // Plaintext is HTTP/1.1 only: no h2c sniffing, so no rewind buffer per connection.
                None => serve_conn(state, stream, peer.ip(), hrt, false).await,
                // Boxed: the handshake future and rustls connection state are several KiB,
                // and an unboxed branch would make *every* connection task that large.
                Some(acc) => {
                    Box::pin(async move {
                        match tokio::time::timeout(hrt, acc.accept(stream)).await {
                            Ok(Ok(s)) => {
                                let h2 = s.get_ref().1.alpn_protocol() == Some(b"h2");
                                serve_conn(state, s, peer.ip(), hrt, h2).await
                            }
                            _ => tracing::debug!(%peer, "tls handshake failed or timed out"),
                        }
                    })
                    .await
                }
            }
        };
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::debug!(
                bytes = std::mem::size_of_val(&fut),
                "connection task future size"
            )
        });
        tokio::spawn(fut);
    }
}

/// The protocol is decided by TLS ALPN, never by sniffing the first bytes.
async fn serve_conn<S>(state: Arc<State>, io: S, ip: std::net::IpAddr, hrt: Duration, h2: bool)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req| proxy::handle(state.clone(), req, ip));
    let res = if h2 {
        // Boxed for the same reason as the TLS branch: keep the HTTP/1 task small.
        Box::pin(
            http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .keep_alive_interval(Some(Duration::from_secs(20)))
                .max_concurrent_streams(256)
                .serve_connection(TokioIo::new(io), svc),
        )
        .await
    } else {
        http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(hrt)
            .keep_alive(true)
            .serve_connection(TokioIo::new(io), svc)
            .await
    };
    if let Err(e) = res {
        tracing::debug!(err = %e, "connection closed with error");
    }
}

/// Bind with a deep accept backlog. Tokio's default (1024) drops SYNs when a thousand
/// clients connect at once, and each dropped SYN costs the client a 1 s retransmit.
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    let sa = tokio::net::lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| std::io::Error::other("bad address"))?;
    let sock = if sa.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    sock.set_reuseaddr(true)?;
    sock.bind(sa)?;
    sock.listen(4096)
}

/// Admin listener: /metrics (Prometheus) and /healthz.
pub async fn serve_admin(state: Arc<State>, listener: TcpListener) {
    use bytes::Bytes;
    use http_body_util::Full;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let st = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: http::Request<hyper::body::Incoming>| {
                let st = st.clone();
                async move {
                    let body = match req.uri().path() {
                        "/metrics" => METRICS.render(st.cache.bytes()),
                        "/healthz" => "ok\n".to_string(),
                        _ => "try /metrics\n".to_string(),
                    };
                    Ok::<_, hyper::Error>(http::Response::new(Full::new(Bytes::from(body))))
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

/// Bind and run every configured listener. Returns the bound plain-HTTP address.
pub async fn start(state: Arc<State>) -> std::io::Result<std::net::SocketAddr> {
    let l = bind(&state.cfg.listen).await?;
    let addr = l.local_addr()?;
    tokio::spawn(serve(state.clone(), l, None));
    if let (Some(tl), Some(dir)) = (&state.cfg.tls_listen, &state.cfg.tls_cert_dir) {
        let (cfg, _) = crate::tls::server_config(dir.clone(), Duration::from_secs(2));
        let l = bind(tl).await?;
        tracing::info!(addr = %l.local_addr()?, "tls listener");
        tokio::spawn(serve(state.clone(), l, Some(TlsAcceptor::from(cfg))));
    }
    if let Some(al) = &state.cfg.admin_listen {
        let l = TcpListener::bind(al).await?;
        tokio::spawn(serve_admin(state.clone(), l));
    }
    Ok(addr)
}
