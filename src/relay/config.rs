use std::collections::HashMap;

/// Configuration for the relay server.
pub struct RelayConfig {
    /// Port the relay server listens on
    pub port: u16,
    /// Base domain for tunnel subdomains (e.g. "tunnel.mcpr.app")
    pub relay_domain: String,
    /// Optional auth provider URL for token validation
    pub auth_provider: Option<String>,
    /// Shared secret between relay and auth provider
    pub auth_provider_secret: Option<String>,
    /// Static token → allowed subdomains map (alternative to auth provider)
    pub tokens: HashMap<String, Vec<String>>,
    pub max_body_size: Option<usize>,
}
