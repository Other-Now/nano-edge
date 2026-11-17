use nano_edge::{config, proxy::State, server};
use tracing_subscriber::EnvFilter;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "edge.toml".into());
    let cfg = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    if cfg.workers > 0 {
        rt.worker_threads(cfg.workers);
    }
    rt.enable_all().build().unwrap().block_on(async move {
        let state = State::new(cfg);
        let addr = server::start(state.clone()).await.expect("bind");
        tracing::info!(%addr, upstreams = state.pool.servers.len(), "nano-edge listening");
        tokio::signal::ctrl_c().await.ok();
    });
}
