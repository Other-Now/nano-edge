//! SNI certificate selection with hot reload.
//!
//! `cert_dir` holds `<server-name>.crt` / `<server-name>.key` pairs (PEM). A ClientHello's
//! SNI picks the pair with that name; wildcard `_wildcard.example.com.crt` serves
//! `*.example.com`; `default.crt` serves everything else. A background task re-reads the
//! directory when any file's mtime changes and swaps the whole map atomically, so
//! in-flight handshakes keep the map they started with.

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

type CertMap = HashMap<String, Arc<CertifiedKey>>;

#[derive(Debug)]
pub struct SniResolver {
    certs: RwLock<Arc<CertMap>>,
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let map = self.certs.read().unwrap().clone();
        if let Some(name) = hello.server_name() {
            let name = name.to_ascii_lowercase();
            if let Some(c) = map.get(&name) {
                return Some(c.clone());
            }
            if let Some((_, parent)) = name.split_once('.') {
                if let Some(c) = map.get(&format!("*.{parent}")) {
                    return Some(c.clone());
                }
            }
        }
        map.get("default").cloned()
    }
}

fn load_pair(crt: &Path, key: &Path) -> Result<Arc<CertifiedKey>, String> {
    let chain = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(crt).map_err(|e| e.to_string())?,
    ))
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| e.to_string())?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key).map_err(|e| e.to_string())?,
    ))
    .map_err(|e| e.to_string())?
    .ok_or("no private key")?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| e.to_string())?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

fn load_dir(dir: &Path) -> CertMap {
    let mut map = CertMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return map;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("crt") {
            continue;
        }
        let stem = p
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        let name = stem
            .strip_prefix("_wildcard.")
            .map(|r| format!("*.{r}"))
            .unwrap_or(stem);
        match load_pair(&p, &p.with_extension("key")) {
            Ok(ck) => {
                map.insert(name, ck);
            }
            // A half-written pair during a deploy is skipped; the old map stays in force.
            Err(err) => tracing::warn!(file = %p.display(), %err, "skipping certificate"),
        }
    }
    map
}

fn dir_stamp(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl SniResolver {
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.certs.read().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

pub fn server_config(
    dir: PathBuf,
    poll: Duration,
) -> (Arc<rustls::ServerConfig>, Arc<SniResolver>) {
    let resolver = Arc::new(SniResolver {
        certs: RwLock::new(Arc::new(load_dir(&dir))),
    });
    let r2 = resolver.clone();
    tokio::spawn(async move {
        let mut stamp = dir_stamp(&dir);
        loop {
            tokio::time::sleep(poll).await;
            let now = dir_stamp(&dir);
            if now != stamp {
                stamp = now;
                let map = load_dir(&dir);
                tracing::info!(certs = map.len(), "reloaded TLS certificates");
                *r2.certs.write().unwrap() = Arc::new(map);
            }
        }
    });
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    (Arc::new(cfg), resolver)
}
