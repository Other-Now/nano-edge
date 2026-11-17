//! Where does an idle connection's memory go? Accept connections and hold them in one of
//! several ways, so bench/idle_conns.py can measure each layer separately.
//!
//!   cargo run --release --example conn_mem -- <mode> <port>
//!   modes: hold      task owns the TcpStream and awaits readability (no buffer)
//!          buf8k     task owns the stream + an 8 KiB read buffer (what hyper allocates)
//!          hyper     bare hyper http1 hello-world, default settings
//!          hyper-hrt bare hyper + header_read_timeout (the timer nano-edge arms)

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::time::Duration;
use tokio::io::AsyncReadExt;

fn main() {
    let mut a = std::env::args().skip(1);
    let mode = a.next().unwrap_or_else(|| "hyper".into());
    let port: u16 = a.next().and_then(|p| p.parse().ok()).unwrap_or(8090);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let l = nano_edge::server::bind(&format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mode = mode.clone();
            tokio::spawn(async move {
                match mode.as_str() {
                    "hold" => {
                        let _ = s.readable().await;
                        // Answer one request so the client can proceed, then hold.
                        let mut b = [0u8; 512];
                        let _ = s.try_read(&mut b);
                        reply(&mut s).await;
                        std::future::pending::<()>().await
                    }
                    "buf8k" => {
                        let mut b = vec![0u8; 8192];
                        let _ = s.read(&mut b).await;
                        reply(&mut s).await;
                        let _ = s.read(&mut b).await;
                    }
                    _ => {
                        let svc = service_fn(|_req| async {
                            Ok::<_, hyper::Error>(hyper::Response::new(Full::new(Bytes::from(
                                vec![b'x'; 1024],
                            ))))
                        });
                        let mut b = http1::Builder::new();
                        if mode == "hyper-hrt" {
                            b.timer(TokioTimer::new())
                                .header_read_timeout(Duration::from_secs(120));
                        }
                        let _ = b.serve_connection(TokioIo::new(s), svc).await;
                    }
                }
            });
        }
    });
}

async fn reply(s: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    let body = vec![b'x'; 1024];
    let _ = s
        .write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len()).as_bytes())
        .await;
    let _ = s.write_all(&body).await;
}
