use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// RFC 7230 hop-by-hop headers plus `content-length`.
///
/// These must not be forwarded across a proxy: they describe the
/// connection between the current pair of peers, not the end-to-end
/// message. Forwarding `transfer-encoding` or a stale `content-length`
/// through the tunnel confuses hyper's body framing on the other side
/// and causes the TCP connection to be dropped before the response is
/// serialized — which surfaces to the client as a 502 with
/// "upstream prematurely closed connection".
pub fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn is_hop_by_hop__flags_all_rfc7230_headers() {
        for h in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(is_hop_by_hop(h), "{h} should be hop-by-hop");
        }
    }

    #[test]
    fn is_hop_by_hop__flags_content_length() {
        assert!(is_hop_by_hop("content-length"));
    }

    #[test]
    fn is_hop_by_hop__is_case_insensitive() {
        assert!(is_hop_by_hop("Transfer-Encoding"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(is_hop_by_hop("Content-Length"));
    }

    #[test]
    fn is_hop_by_hop__allows_end_to_end_headers() {
        for h in [
            "content-type",
            "content-encoding",
            "cache-control",
            "mcp-session-id",
            "authorization",
            "host",
            "accept",
            "user-agent",
            "set-cookie",
        ] {
            assert!(!is_hop_by_hop(h), "{h} should NOT be hop-by-hop");
        }
    }

    #[test]
    fn is_hop_by_hop__rejects_empty() {
        assert!(!is_hop_by_hop(""));
    }
}
