//! Minimal Server-Sent Events codec for MCP `text/event-stream` responses.
//!
//! Step 1 scope: decode whole-body SSE into a sequence of `data:` payloads
//! and encode `JsonRpcResult`s as `event: message` frames. Buffered, not
//! streamed — true byte-for-byte passthrough is step 2.
//!
//! Decoder follows the EventSource spec (HTML living standard) for the
//! parts MCP uses: `\n` / `\r\n` / `\r` line terminators, `field: value`
//! lines, blank line dispatches the buffered event, multiple `data:` lines
//! join with `\n`, comment lines (`:` prefix) ignored. Only frames whose
//! `event` is `message` (or absent — the default) are returned. `id` and
//! `retry` are recognized but ignored.

use axum::body::Bytes;

use crate::protocol::mcp::JsonRpcResult;

/// Decode an SSE body into the ordered `data` payloads of every dispatched
/// `message` event. Frames with a non-`message` `event:` field are dropped.
/// A trailing frame missing its blank-line dispatch is also dropped.
pub fn decode_frames(bytes: &[u8]) -> Vec<Bytes> {
    let mut frames = Vec::new();
    let mut event = String::new();
    let mut data = String::new();
    let mut data_seen = false;

    for line in split_lines(bytes) {
        if line.is_empty() {
            if data_seen && (event.is_empty() || event == "message") {
                frames.push(Bytes::copy_from_slice(data.as_bytes()));
            }
            event.clear();
            data.clear();
            data_seen = false;
            continue;
        }
        if line.starts_with(b":") {
            continue;
        }
        let (field, value) = split_field(line);
        match field {
            b"event" => {
                event.clear();
                event.push_str(std::str::from_utf8(value).unwrap_or(""));
            }
            b"data" => {
                if data_seen {
                    data.push('\n');
                }
                data.push_str(std::str::from_utf8(value).unwrap_or(""));
                data_seen = true;
            }
            _ => {}
        }
    }

    frames
}

/// Encode JSON-RPC results as one `event: message` frame each.
pub fn encode_results(results: &[JsonRpcResult]) -> Bytes {
    let mut out = Vec::with_capacity(results.len() * 64);
    for r in results {
        out.extend_from_slice(b"event: message\ndata: ");
        let payload = serde_json::to_vec(r).expect("JsonRpcResult serializes");
        out.extend_from_slice(&payload);
        out.extend_from_slice(b"\n\n");
    }
    Bytes::from(out)
}

/// Split SSE body on `\n`, `\r\n`, or `\r`. Returns line slices without
/// terminators.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&bytes[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(&bytes[start..i]);
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

/// Split a field line into `(name, value)`. Per spec: name is up to the
/// first colon; value is everything after, with one optional leading space
/// stripped. Lines with no colon are treated as field-only with empty value.
fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        Some(pos) => {
            let name = &line[..pos];
            let mut value = &line[pos + 1..];
            if let Some((&b' ', rest)) = value.split_first() {
                value = rest;
            }
            (name, value)
        }
        None => (line, b""),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::protocol::mcp::{JsonRpcResponse, JsonRpcVersion, RequestId};

    fn rpc(id: i64) -> JsonRpcResult {
        JsonRpcResult::Response(JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            result: Some(serde_json::json!({"ok": true})),
        })
    }

    fn data_str(b: &Bytes) -> &str {
        std::str::from_utf8(b).unwrap()
    }

    // ── decode_frames ─────────────────────────────────────────

    #[test]
    fn decode_frames__single_message_frame() {
        let body = b"event: message\ndata: {\"id\":1}\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "{\"id\":1}");
    }

    #[test]
    fn decode_frames__no_event_field_treated_as_message() {
        let body = b"data: {\"id\":1}\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "{\"id\":1}");
    }

    #[test]
    fn decode_frames__multiple_frames_in_order() {
        let body = b"data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 2);
        assert_eq!(data_str(&frames[0]), "{\"id\":1}");
        assert_eq!(data_str(&frames[1]), "{\"id\":2}");
    }

    #[test]
    fn decode_frames__non_message_event_dropped() {
        let body = b"event: ping\ndata: ignored\n\nevent: message\ndata: kept\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "kept");
    }

    #[test]
    fn decode_frames__multiline_data_joined_with_newline() {
        let body = b"data: line1\ndata: line2\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "line1\nline2");
    }

    #[test]
    fn decode_frames__crlf_line_endings() {
        let body = b"event: message\r\ndata: {\"id\":1}\r\n\r\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "{\"id\":1}");
    }

    #[test]
    fn decode_frames__cr_line_endings() {
        let body = b"event: message\rdata: {\"id\":1}\r\r";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "{\"id\":1}");
    }

    #[test]
    fn decode_frames__comment_lines_ignored() {
        let body = b": keepalive\n\ndata: hi\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "hi");
    }

    #[test]
    fn decode_frames__trailing_partial_frame_dropped() {
        let body = b"data: complete\n\ndata: partial";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "complete");
    }

    #[test]
    fn decode_frames__empty_input_returns_empty() {
        assert!(decode_frames(b"").is_empty());
    }

    #[test]
    fn decode_frames__blank_line_without_data_emits_nothing() {
        assert!(decode_frames(b"event: message\n\n").is_empty());
    }

    #[test]
    fn decode_frames__field_value_strips_one_leading_space_only() {
        let body = b"data:  two-leading-spaces\n\n";
        let frames = decode_frames(body);
        assert_eq!(data_str(&frames[0]), " two-leading-spaces");
    }

    #[test]
    fn decode_frames__unknown_field_ignored() {
        let body = b"id: 42\nretry: 1000\ndata: hi\n\n";
        let frames = decode_frames(body);
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "hi");
    }

    // ── encode_results ────────────────────────────────────────

    #[test]
    fn encode_results__single_result_one_frame() {
        let bytes = encode_results(&[rpc(1)]);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: message\ndata: "));
        assert!(s.ends_with("\n\n"));
        assert_eq!(s.matches("\n\n").count(), 1);
    }

    #[test]
    fn encode_results__multiple_results_concatenated_frames() {
        let bytes = encode_results(&[rpc(1), rpc(2), rpc(3)]);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s.matches("event: message\n").count(), 3);
        assert_eq!(s.matches("\n\n").count(), 3);
    }

    #[test]
    fn encode_results__empty_input_returns_empty() {
        assert!(encode_results(&[]).is_empty());
    }

    #[test]
    fn encode_results__roundtrip_through_decoder() {
        let original = vec![rpc(1), rpc(2)];
        let encoded = encode_results(&original);
        let frames = decode_frames(&encoded);
        assert_eq!(frames.len(), 2);
        for (frame, expected) in frames.iter().zip(&original) {
            let parsed: JsonRpcResult = serde_json::from_slice(frame).unwrap();
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }
}
