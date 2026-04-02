/// Check if the response body is SSE-formatted and extract the JSON data.
/// Returns the extracted JSON bytes if exactly one `data:` event is found.
pub fn extract_json_from_sse(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.trim_start().starts_with("data:") && !text.contains("\ndata:") {
        return None;
    }
    let mut json_parts = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            if !data.is_empty() {
                json_parts.push(data);
            }
        }
    }
    if json_parts.len() == 1 {
        Some(json_parts[0].as_bytes().to_vec())
    } else {
        None
    }
}

/// Re-wrap JSON bytes into SSE format.
pub fn wrap_as_sse(json_bytes: &[u8]) -> Vec<u8> {
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(json_bytes);
    out.extend_from_slice(b"\n\n");
    out
}

/// Split a full upstream URL into (base, path).
/// e.g. "http://localhost:9000/mcp" → ("http://localhost:9000", "/mcp")
/// e.g. "http://localhost:9000" → ("http://localhost:9000", "")
pub fn split_upstream(url: &str) -> (&str, &str) {
    let after_scheme = if let Some(pos) = url.find("://") {
        pos + 3
    } else {
        0
    };
    match url[after_scheme..].find('/') {
        Some(pos) => url.split_at(after_scheme + pos),
        None => (url, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SSE extraction ──

    #[test]
    fn extract_json_from_sse_single_event() {
        let input = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let result = extract_json_from_sse(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn extract_json_from_sse_with_leading_whitespace_returns_none() {
        let input = b"  data: {\"id\":1}\n\n";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_not_sse() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1}";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_multiple_events_returns_none() {
        let input = b"data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_empty_data_skipped() {
        let input = b"data: \ndata: {\"id\":1}\n\n";
        let result = extract_json_from_sse(input);
        assert!(result.is_some());
    }

    // ── SSE wrapping ──

    #[test]
    fn wrap_as_sse_format() {
        let json = b"{\"id\":1}";
        let wrapped = wrap_as_sse(json);
        assert_eq!(wrapped, b"data: {\"id\":1}\n\n");
    }

    #[test]
    fn sse_roundtrip() {
        let original = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"content\":[]}}";
        let wrapped = wrap_as_sse(original);
        let extracted = extract_json_from_sse(&wrapped).unwrap();
        assert_eq!(extracted, original);
    }

    // ── split_upstream ──

    #[test]
    fn split_upstream_with_path() {
        let (base, path) = split_upstream("http://localhost:9000/mcp");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "/mcp");
    }

    #[test]
    fn split_upstream_no_path() {
        let (base, path) = split_upstream("http://localhost:9000");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "");
    }

    #[test]
    fn split_upstream_deep_path() {
        let (base, path) = split_upstream("https://api.example.com/v1/mcp");
        assert_eq!(base, "https://api.example.com");
        assert_eq!(path, "/v1/mcp");
    }

    #[test]
    fn split_upstream_trailing_slash() {
        let (base, path) = split_upstream("http://localhost:9000/");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "/");
    }
}
