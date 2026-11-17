//! nano-edge: a small CDN edge reverse proxy.
//!
//! Request path, in order: connection limits (server) -> WAF -> per-IP rate limit ->
//! cache lookup -> request collapsing -> load-balanced upstream fetch -> store.

pub mod cache;
pub mod config;
pub mod lb;
pub mod limits;
pub mod metrics;
pub mod proxy;
pub mod server;
pub mod tls;
