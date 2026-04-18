//! `SchemaScanner` — active discovery of an upstream MCP server's schema.
//!
//! This module defines the trait and its supporting types. No concrete
//! implementation is provided here; the HTTP-backed scanner lands in a
//! later step once the `SchemaManager` has consumers.

use std::future::Future;

use serde_json::Value;

/// How the scanner acquires an MCP session for discovery calls.
#[derive(Debug, Clone)]
pub enum ScanMode {
    /// Scanner opens its own MCP session to the upstream.
    ///
    /// Used when the upstream accepts anonymous sessions or the proxy
    /// holds credentials (e.g. a static bearer token in `mcpr.toml`).
    Standalone,
    /// Scanner injects discovery requests into an existing client session.
    ///
    /// Used when the upstream requires auth that only clients hold.
    /// The scanner uses reserved JSON-RPC ids (e.g. `__mcpr_scan_<n>`)
    /// to multiplex without colliding with the client's own requests.
    Attached { session_id: String },
}

/// One method's merged `result` payload from a scan.
///
/// The scanner is responsible for merging paginated responses before
/// returning. Consumers pass each `ScanResult` into
/// `SchemaManager::ingest` as if it were a single-page response.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub method: String,
    pub result: Value,
}

/// Errors that can arise during a scan.
#[derive(Debug)]
pub enum ScanError {
    Transport(String),
    UnsupportedMode(ScanMode),
    Aborted(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "upstream transport error: {m}"),
            Self::UnsupportedMode(mode) => write!(f, "scan mode {mode:?} not supported"),
            Self::Aborted(m) => write!(f, "scan aborted: {m}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// Drives active schema discovery against an upstream.
///
/// Implementations run the discovery handshake (`initialize` →
/// `tools/list` / `resources/list` / `resources/templates/list` /
/// `prompts/list`) and return the merged result of each call.
pub trait SchemaScanner: Send + Sync + 'static {
    fn scan(
        &self,
        upstream_id: &str,
        mode: ScanMode,
    ) -> impl Future<Output = Result<Vec<ScanResult>, ScanError>> + Send;
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    struct MockScanner;

    impl SchemaScanner for MockScanner {
        async fn scan(
            &self,
            _upstream_id: &str,
            _mode: ScanMode,
        ) -> Result<Vec<ScanResult>, ScanError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn scanner_trait__has_at_least_one_impl() {
        let s = MockScanner;
        let out = s.scan("my-proxy", ScanMode::Standalone).await.unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_error__display_covers_all_variants() {
        assert_eq!(
            ScanError::Transport("boom".into()).to_string(),
            "upstream transport error: boom"
        );
        assert!(
            ScanError::UnsupportedMode(ScanMode::Standalone)
                .to_string()
                .contains("not supported")
        );
        assert_eq!(
            ScanError::Aborted("cancelled".into()).to_string(),
            "scan aborted: cancelled"
        );
    }
}
