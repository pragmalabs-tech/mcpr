use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioTimer};
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

    // No `http2_only` — most MCP upstreams are HTTP/1.1; cleartext H2
    // (h2c) requires server opt-in, and HTTPS negotiates H2 via ALPN.
    // The H2 keep-alive settings only fire when the connection is H2.
    Ok(Client::builder(TokioExecutor::new())
        .timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(upstream.max_concurrent)
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

    // ── isolation across pools ────────────────────────────────

    /// Two pools built from two different `ProxyConfig`s must each carry
    /// their own upstream metadata — clients/configs don't leak across
    /// instances. Verified by config (cheap) and by an actual request
    /// hitting only the matching upstream.
    #[tokio::test]
    async fn multiple_pools__route_to_their_own_upstream() {
        use axum::{Router, routing::get};
        use std::sync::{Arc as StdArc, Mutex};

        async fn spawn(label: &'static str, hits: StdArc<Mutex<Vec<&'static str>>>) -> String {
            let app = Router::new().route(
                "/",
                get(move || {
                    let hits = hits.clone();
                    async move {
                        hits.lock().unwrap().push(label);
                        "ok"
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            format!("http://{addr}")
        }

        let hits = StdArc::new(Mutex::new(Vec::new()));
        let url_a = spawn("A", hits.clone()).await;
        let url_b = spawn("B", hits.clone()).await;

        let cfg_a = Arc::new(ProxyConfig {
            name: "a".into(),
            mcp: url_a.clone(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        });
        let cfg_b = Arc::new(ProxyConfig {
            name: "b".into(),
            mcp: url_b.clone(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        });

        let pool_a = UpstreamPool::new(cfg_a).unwrap();
        let pool_b = UpstreamPool::new(cfg_b).unwrap();

        // Each pool's UpstreamConfig captures its own URL.
        assert_eq!(pool_a.upstream().url.as_str(), format!("{url_a}/"));
        assert_eq!(pool_b.upstream().url.as_str(), format!("{url_b}/"));
        assert_eq!(pool_a.upstream().name, "a");
        assert_eq!(pool_b.upstream().name, "b");

        // A request through each pool hits only its own upstream.
        use http_body_util::{BodyExt, Full, combinators::BoxBody};
        let empty_body = || -> BoxBody<Bytes, hyper::Error> {
            Full::new(Bytes::new())
                .map_err(|never: std::convert::Infallible| match never {})
                .boxed()
        };
        let send = |pool: &UpstreamPool| {
            let uri = pool.upstream().url.as_str().to_string();
            let client = pool.pick("anything").clone();
            let body = empty_body();
            async move {
                let req = hyper::Request::builder()
                    .method(hyper::Method::GET)
                    .uri(uri)
                    .body(body)
                    .unwrap();
                client.request(req).await.unwrap();
            }
        };
        send(&pool_a).await;
        send(&pool_b).await;

        let log = hits.lock().unwrap();
        assert_eq!(*log, vec!["A", "B"]);
    }
}
