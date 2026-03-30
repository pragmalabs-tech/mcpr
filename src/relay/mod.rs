mod auth;
pub mod config;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use base64::Engine;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, oneshot};

use auth::{AuthError, AuthProviderConfig, subdomain_matches, verify_token};
use config::RelayConfig;

// ── Protocol messages (shared with tunnel client) ──────────────────────

#[derive(Serialize, Deserialize)]
pub struct TunnelRequest {
    pub id: String,
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>, // base64
}

#[derive(Serialize, Deserialize)]
pub struct TunnelResponse {
    pub id: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Option<String>, // base64
}

#[derive(Serialize, Deserialize)]
pub struct RegisterRequest {
    pub token: String,
    #[serde(default)]
    pub subdomain: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RegisterAck {
    pub subdomain: String,
    pub url: String,
}

/// Sent by relay when client didn't specify a subdomain and auth returned allowed list.
#[derive(Serialize, Deserialize)]
pub struct SubdomainOffer {
    pub subdomains: Vec<String>,
}

/// Sent by client to pick a subdomain from the offered list.
#[derive(Serialize, Deserialize)]
pub struct SubdomainPick {
    pub subdomain: String,
}

// ── Relay server ────────────────────────────────────────────────────────

type PendingRequests = Arc<RwLock<HashMap<String, oneshot::Sender<TunnelResponse>>>>;
type TunnelSender = tokio::sync::mpsc::UnboundedSender<String>;

struct TunnelConnection {
    sender: TunnelSender,
    pending: PendingRequests,
    /// Signal to notify the tunnel handler it has been evicted.
    evict: tokio::sync::Notify,
}

struct RelayState {
    /// subdomain → active tunnel connection
    tunnels: DashMap<String, Arc<TunnelConnection>>,
    /// Base domain for tunnel URLs
    base_domain: String,
    /// Auth mode for token validation
    auth: AuthMode,
}

/// How the relay validates tunnel registration tokens.
enum AuthMode {
    /// No authentication — anyone can tunnel
    Open,
    /// Static tokens from config file (token → allowed subdomain patterns)
    Static(HashMap<String, Vec<String>>),
    /// External auth provider API
    Provider(AuthProviderConfig),
}

/// Derive a consistent subdomain from a token.
fn token_to_subdomain(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let hash = hasher.finalize();
    hex::encode(&hash[..6])
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Start the relay server.
pub async fn start_relay(cfg: RelayConfig) {
    let auth = if !cfg.tokens.is_empty() {
        let count = cfg.tokens.len();
        println!(
            "  {} static tokens: {} token(s) configured",
            colored::Colorize::green("✓"),
            count,
        );
        AuthMode::Static(cfg.tokens)
    } else if let Some(url) = cfg.auth_provider {
        let secret = cfg
            .auth_provider_secret
            .expect("auth_provider_secret is required when auth_provider is set");
        println!("  {} auth provider enabled", colored::Colorize::green("✓"));
        AuthMode::Provider(AuthProviderConfig {
            url: url.trim_end_matches('/').to_string(),
            secret,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
        })
    } else {
        println!(
            "  {} open mode (anyone can tunnel)",
            colored::Colorize::yellow("!"),
        );
        AuthMode::Open
    };

    let state = Arc::new(RelayState {
        tunnels: DashMap::new(),
        base_domain: cfg.relay_domain,
        auth,
    });

    const DEFAULT_MAX_BODY_SIZE: usize = 5 * 1024 * 1024;
    let max_body = cfg.max_body_size.unwrap_or(DEFAULT_MAX_BODY_SIZE);

    let app = Router::new()
        .route("/_tunnel/register", any(handle_register))
        .fallback(any(handle_tunnel_request))
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_body));

    let port = cfg.port;
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind relay");

    println!(
        "  {} relay listening on :{port}",
        colored::Colorize::green("mcpr")
    );

    axum::serve(listener, app).await.expect("Relay failed");
}

/// WebSocket registration endpoint.
/// Token is sent as the first message after upgrade (not in query string).
async fn handle_register(ws: WebSocketUpgrade, State(state): State<Arc<RelayState>>) -> Response {
    ws.on_upgrade(move |socket| handle_tunnel_ws(socket, state))
}

async fn handle_tunnel_ws(socket: WebSocket, state: Arc<RelayState>) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Read RegisterRequest as the first message (contains token + optional subdomain)
    let reg: RegisterRequest = loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                Ok(req) => break req,
                Err(_) => continue,
            },
            Some(Err(_)) | None => return,
            _ => continue,
        }
    };

    let token = reg.token;
    let requested_subdomain = reg.subdomain;

    // Helper to close with error
    async fn close_with_error(
        ws_sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        reason: &str,
    ) {
        let _ = ws_sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4001,
                reason: reason.into(),
            })))
            .await;
    }

    // Helper to offer subdomains and wait for client pick
    async fn offer_and_pick(
        ws_sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        ws_stream: &mut futures_util::stream::SplitStream<WebSocket>,
        allowed: &[String],
    ) -> Option<String> {
        let offer = SubdomainOffer {
            subdomains: allowed.to_vec(),
        };
        if ws_sink
            .send(Message::Text(serde_json::to_string(&offer).unwrap().into()))
            .await
            .is_err()
        {
            return None;
        }
        // Wait for SubdomainPick
        loop {
            match ws_stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(pick) = serde_json::from_str::<SubdomainPick>(&text) {
                        return Some(pick.subdomain);
                    }
                    continue;
                }
                _ => return None,
            }
        }
    }

    // Resolve subdomain: if requested and allowed → use it; if not allowed or missing → offer pick
    async fn resolve_subdomain(
        requested: Option<String>,
        allowed: &[String],
        ws_sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        ws_stream: &mut futures_util::stream::SplitStream<WebSocket>,
    ) -> Option<String> {
        // If requested and it matches, use it directly
        if let Some(ref sub) = requested
            && subdomain_matches(allowed, sub)
        {
            return Some(sub.clone());
        }
        // Auto-assign if exactly one concrete subdomain
        if allowed.len() == 1 && !allowed[0].contains('*') {
            return Some(allowed[0].clone());
        }
        // Otherwise offer the list and let the client pick
        let picked = offer_and_pick(ws_sink, ws_stream, allowed).await?;
        if subdomain_matches(allowed, &picked) {
            Some(picked)
        } else {
            close_with_error(
                ws_sink,
                &format!("subdomain '{}' not authorized for this token", picked),
            )
            .await;
            None
        }
    }

    // Validate token and resolve subdomain based on auth mode
    let subdomain = match &state.auth {
        AuthMode::Open => Some(requested_subdomain.unwrap_or_else(|| token_to_subdomain(&token))),
        AuthMode::Static(tokens) => match tokens.get(&token) {
            Some(allowed) => {
                resolve_subdomain(requested_subdomain, allowed, &mut ws_sink, &mut ws_stream).await
            }
            None => {
                close_with_error(&mut ws_sink, "invalid token").await;
                return;
            }
        },
        AuthMode::Provider(auth) => {
            let sub_for_verify = requested_subdomain.as_deref().unwrap_or("");
            match verify_token(auth, &token, sub_for_verify).await {
                Ok(allowed) => {
                    resolve_subdomain(requested_subdomain, &allowed, &mut ws_sink, &mut ws_stream)
                        .await
                }
                Err(AuthError::InvalidToken(msg)) => {
                    close_with_error(&mut ws_sink, &msg).await;
                    return;
                }
                Err(AuthError::ProviderUnavailable(msg)) => {
                    println!(
                        "  {} auth provider error: {}",
                        colored::Colorize::red("✗"),
                        msg
                    );
                    close_with_error(&mut ws_sink, "auth provider unavailable").await;
                    return;
                }
            }
        }
    };

    let subdomain = match subdomain {
        Some(s) => s,
        None => return,
    };

    let url = format!("https://{}.{}", subdomain, state.base_domain);

    // Send registration ack
    let ack = RegisterAck {
        subdomain: subdomain.clone(),
        url: url.clone(),
    };
    if ws_sink
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await
        .is_err()
    {
        return;
    }

    // Create channel for sending requests to this tunnel client
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let pending: PendingRequests = Arc::new(RwLock::new(HashMap::new()));

    let conn = Arc::new(TunnelConnection {
        sender: tx,
        pending: pending.clone(),
        evict: tokio::sync::Notify::new(),
    });

    // Evict existing tunnel on same subdomain (if any) before registering
    if let Some((_, old)) = state.tunnels.remove(&subdomain) {
        old.evict.notify_one();
        println!(
            "  {} evicted old tunnel: {}",
            colored::Colorize::yellow("⇄"),
            subdomain
        );
    }

    // Register tunnel
    let conn_for_evict = conn.clone();
    state.tunnels.insert(subdomain.clone(), conn);

    println!(
        "  {} tunnel registered: {}",
        colored::Colorize::green("↑"),
        colored::Colorize::cyan(url.as_str())
    );

    // Spawn task to forward outbound messages (relay → client) from channel to WS.
    // Also listens for eviction signal to send a close frame with reason.
    let send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                () = conn_for_evict.evict.notified() => {
                    let _ = ws_sink
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 4002,
                            reason: "evicted: another client registered with the same tunnel".into(),
                        })))
                        .await;
                    break;
                }
            }
        }
    });

    // Read responses from WS (client → relay) and resolve pending requests
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg
            && let Ok(resp) = serde_json::from_str::<TunnelResponse>(&text)
        {
            let mut p = pending.write().await;
            if let Some(sender) = p.remove(&resp.id) {
                let _ = sender.send(resp);
            }
        }
    }

    // Client disconnected — clean up
    send_task.abort();
    state.tunnels.remove(&subdomain);
    println!(
        "  {} tunnel disconnected: {}",
        colored::Colorize::red("↓"),
        subdomain
    );
}

/// Catch-all handler: extract subdomain from Host header, forward request through tunnel.
async fn handle_tunnel_request(
    State(state): State<Arc<RelayState>>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    // Extract subdomain from Host header: "abc123.tunnel.mcpr.app" → "abc123"
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let subdomain = host.split('.').next().unwrap_or("").to_string();
    let path_str = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".into());

    if subdomain.is_empty() {
        relay_log(
            "-",
            method.as_str(),
            &path_str,
            400,
            0,
            std::time::Duration::ZERO,
        );
        return (StatusCode::BAD_REQUEST, "missing host header").into_response();
    }

    // Find tunnel connection
    let conn = match state.tunnels.get(&subdomain) {
        Some(c) => c.clone(),
        None => {
            relay_log(
                &subdomain,
                method.as_str(),
                &path_str,
                502,
                0,
                std::time::Duration::ZERO,
            );
            return (StatusCode::BAD_GATEWAY, "tunnel not found").into_response();
        }
    };

    // Build tunnel request
    let req_id = uuid::Uuid::new_v4().to_string();
    let mut req_headers = HashMap::new();
    for (key, val) in headers.iter() {
        if let Ok(v) = val.to_str() {
            req_headers.insert(key.to_string(), v.to_string());
        }
    }

    let body_b64 = if body.is_empty() {
        None
    } else {
        Some(base64::engine::general_purpose::STANDARD.encode(&body))
    };

    let tunnel_req = TunnelRequest {
        id: req_id.clone(),
        method: method.to_string(),
        path: uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "/".into()),
        headers: req_headers,
        body: body_b64,
    };

    // Register pending response
    let (resp_tx, resp_rx) = oneshot::channel();
    conn.pending.write().await.insert(req_id.clone(), resp_tx);

    // Send request to tunnel client
    let msg = serde_json::to_string(&tunnel_req).unwrap();
    if conn.sender.send(msg).is_err() {
        conn.pending.write().await.remove(&req_id);
        relay_log(
            &subdomain,
            method.as_str(),
            &path_str,
            502,
            0,
            std::time::Duration::ZERO,
        );
        return (StatusCode::BAD_GATEWAY, "tunnel disconnected").into_response();
    }

    // Wait for response with timeout
    let path = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".into());
    let start = std::time::Instant::now();

    match tokio::time::timeout(std::time::Duration::from_secs(30), resp_rx).await {
        Ok(Ok(resp)) => {
            let status_code = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let body_len = resp.body.as_ref().map(|b| b.len()).unwrap_or(0);
            relay_log(
                &subdomain,
                method.as_ref(),
                &path,
                resp.status,
                body_len,
                start.elapsed(),
            );

            let mut builder = Response::builder().status(status_code);

            for (k, v) in &resp.headers {
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(v),
                ) {
                    builder = builder.header(name, val);
                }
            }

            let body_bytes = resp
                .body
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .unwrap_or_default();

            builder
                .body(axum::body::Body::from(body_bytes))
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "response build error").into_response()
                })
        }
        Ok(Err(_)) => {
            relay_log(&subdomain, method.as_ref(), &path, 502, 0, start.elapsed());
            (StatusCode::BAD_GATEWAY, "tunnel dropped request").into_response()
        }
        Err(_) => {
            conn.pending.write().await.remove(&req_id);
            relay_log(&subdomain, method.as_ref(), &path, 504, 0, start.elapsed());
            (StatusCode::GATEWAY_TIMEOUT, "tunnel timeout").into_response()
        }
    }
}

/// nginx-style access log for relay mode.
fn relay_log(
    subdomain: &str,
    method: &str,
    path: &str,
    status: u16,
    body_len: usize,
    duration: std::time::Duration,
) {
    use colored::Colorize;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let status_str = if status < 300 {
        format!("{status}").green().to_string()
    } else if status < 400 {
        format!("{status}").yellow().to_string()
    } else {
        format!("{status}").red().to_string()
    };
    let ms = duration.as_millis();
    println!(
        "  {now}  {sub}  {method} {path}  → {status}  {body_len}b  {ms}ms",
        sub = subdomain.dimmed(),
        status = status_str,
    );
}

// ── Tests ──────────��──────────────────────────���─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Subdomain derivation ──

    #[test]
    fn token_to_subdomain_deterministic() {
        let a = token_to_subdomain("my-token");
        let b = token_to_subdomain("my-token");
        assert_eq!(a, b, "same token must produce same subdomain");
    }

    #[test]
    fn token_to_subdomain_different_tokens_differ() {
        let a = token_to_subdomain("token-a");
        let b = token_to_subdomain("token-b");
        assert_ne!(a, b);
    }

    #[test]
    fn token_to_subdomain_is_12_hex_chars() {
        let sub = token_to_subdomain("test");
        assert_eq!(sub.len(), 12);
        assert!(sub.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── Hex encoding ──

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex::encode(&[]), "");
    }

    #[test]
    fn hex_encode_known_values() {
        assert_eq!(hex::encode(&[0x00]), "00");
        assert_eq!(hex::encode(&[0xff]), "ff");
        assert_eq!(hex::encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    // ── Tunnel protocol serialization ──

    #[test]
    fn tunnel_request_roundtrip() {
        let req = TunnelRequest {
            id: "req-1".into(),
            method: "POST".into(),
            path: "/mcp".into(),
            headers: HashMap::from([("content-type".into(), "application/json".into())]),
            body: Some(base64::engine::general_purpose::STANDARD.encode(b"{\"test\":true}")),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: TunnelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-1");
        assert_eq!(decoded.method, "POST");
        assert_eq!(decoded.path, "/mcp");
        let body = base64::engine::general_purpose::STANDARD
            .decode(decoded.body.unwrap())
            .unwrap();
        assert_eq!(body, b"{\"test\":true}");
    }

    #[test]
    fn tunnel_response_roundtrip() {
        let resp = TunnelResponse {
            id: "req-1".into(),
            status: 200,
            headers: HashMap::from([("content-type".into(), "application/json".into())]),
            body: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: TunnelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-1");
        assert_eq!(decoded.status, 200);
        assert!(decoded.body.is_none());
    }

    #[test]
    fn register_ack_roundtrip() {
        let ack = RegisterAck {
            subdomain: "abc123".into(),
            url: "https://abc123.tunnel.example.com".into(),
        };
        let json = serde_json::to_string(&ack).unwrap();
        let decoded: RegisterAck = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.subdomain, "abc123");
        assert_eq!(decoded.url, "https://abc123.tunnel.example.com");
    }

    #[test]
    fn tunnel_request_no_body() {
        let req = TunnelRequest {
            id: "req-2".into(),
            method: "GET".into(),
            path: "/mcp".into(),
            headers: HashMap::new(),
            body: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: TunnelRequest = serde_json::from_str(&json).unwrap();
        assert!(decoded.body.is_none());
    }
}
