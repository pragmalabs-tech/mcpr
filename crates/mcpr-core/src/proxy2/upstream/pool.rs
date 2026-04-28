use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use url::Url;

use crate::proxy2::proxy_config::ProxyConfig;

#[derive(Clone, Debug)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: Url,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_concurrent: usize,
    pub max_response_body: Option<usize>,
}

impl UpstreamConfig {
    pub fn from_proxy_config(cfg: Arc<ProxyConfig>) -> anyhow::Result<Self> {
        let url: Url = cfg.mcp.parse()?;

        Ok(Self {
            name: cfg.name.clone(),
            url,
            connect_timeout: Duration::from_secs(cfg.connect_timeout.unwrap_or(10)),
            request_timeout: Duration::from_secs(cfg.request_timeout.unwrap_or(60)),
            max_concurrent: cfg.max_concurrent_upstream.unwrap_or(100),
            max_response_body: cfg.max_response_body_size,
        })
    }
}

type ProxyBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;
type ProxyClient = Client<HttpsConnector<HttpConnector>, ProxyBody>;

/// Number of HTTP/2 client shards. Each shard is its own pooled
/// connection to the upstream — sharing a single client funnels every
/// in-flight request through one HTTP/2 connection's stream-concurrency
/// budget, which becomes the bottleneck once a proxy is hot.
const POOL_SHARDS: usize = 4;

#[derive(Clone)]
pub struct UpstreamPool {
    clients: Arc<Vec<ProxyClient>>,
    upstream: Arc<UpstreamConfig>,
}

impl UpstreamPool {
    pub fn new(cfg: Arc<ProxyConfig>) -> anyhow::Result<Self> {
        let upstream = UpstreamConfig::from_proxy_config(cfg)?;

        let clients = (0..POOL_SHARDS)
            .map(|_| build_client(&upstream))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            clients: Arc::new(clients),
            upstream: Arc::new(upstream),
        })
    }

    /// Pick a client by hashing `id`. Same id always lands on the same
    /// shard, so all requests for a given session ride the same HTTP/2
    /// connection — preserving in-order delivery and reusing the
    /// connection's keep-alive state.
    pub fn pick<H: Hash + ?Sized>(&self, id: &H) -> &ProxyClient {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % self.clients.len();
        &self.clients[idx]
    }

    pub fn upstream(&self) -> &UpstreamConfig {
        &self.upstream
    }
}

fn build_client(upstream: &UpstreamConfig) -> anyhow::Result<ProxyClient> {
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(upstream.connect_timeout));
    http.set_nodelay(true);
    http.enforce_http(false);

    let https = HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http2()
        .wrap_connector(http);

    Ok(Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(upstream.max_concurrent)
        .http2_only(true)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .build(https))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::csp::CspConfig;

    fn proxy_cfg() -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            name: "test".into(),
            mcp: "http://localhost:9000".into(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        })
    }

    // ── new ───────────────────────────────────────────────────

    #[test]
    fn new__builds_pool_shards_clients() {
        let pool = UpstreamPool::new(proxy_cfg()).unwrap();
        assert_eq!(pool.clients.len(), POOL_SHARDS);
    }

    // ── pick ──────────────────────────────────────────────────

    #[test]
    fn pick__same_id_returns_same_shard() {
        let pool = UpstreamPool::new(proxy_cfg()).unwrap();
        let a = pool.pick("session-abc") as *const _;
        let b = pool.pick("session-abc") as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn pick__distributes_across_shards() {
        let pool = UpstreamPool::new(proxy_cfg()).unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 0..256 {
            let client = pool.pick(&format!("session-{i}")) as *const _;
            seen.insert(client);
        }
        assert_eq!(seen.len(), POOL_SHARDS);
    }
}
