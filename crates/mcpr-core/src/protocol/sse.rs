//! Server-Sent Events codec for MCP `text/event-stream` responses.
//!
//! Two surfaces:
//!   - Buffered: `decode_frames(&[u8])` / `encode_results(&[..])` for
//!     when the whole body is in memory (batch responses, tests).
//!   - Streaming: `decode_frame_stream(body)` / `encode_one(&result)`
//!     for the hot path so frames flow client-ward as upstream emits
//!     them, no `body.collect().await` blocking the proxy.
//!
//! Decoder follows the EventSource spec (HTML living standard) for the
//! parts MCP uses: `\n` / `\r\n` / `\r` line terminators, `field: value`
//! lines, blank line dispatches the buffered event, multiple `data:` lines
//! join with `\n`, comment lines (`:` prefix) ignored. Only frames whose
//! `event` is `message` (or absent, the default) are returned. `id` and
//! `retry` are recognized but ignored.

use std::collections::VecDeque;

use axum::body::Bytes;
use futures_util::stream::{Stream, StreamExt};
use http_body_util::BodyDataStream;

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
        write_one(&mut out, r);
    }
    Bytes::from(out)
}

/// Encode a single JSON-RPC result as one `event: message` frame.
pub fn encode_one(result: &JsonRpcResult) -> Bytes {
    let mut out = Vec::with_capacity(64);
    write_one(&mut out, result);
    Bytes::from(out)
}

fn write_one(out: &mut Vec<u8>, result: &JsonRpcResult) {
    out.extend_from_slice(b"event: message\ndata: ");
    let payload = serde_json::to_vec(result).expect("JsonRpcResult serializes");
    out.extend_from_slice(&payload);
    out.extend_from_slice(b"\n\n");
}

/// Stream complete SSE `data:` payloads from a hyper body, one yield per
/// dispatched `event: message` frame. Bytes flow chunk-by-chunk: a frame
/// only yields once its terminating blank line has arrived. A trailing
/// partial frame (body ends with no final blank line) is dropped.
pub fn decode_frame_stream<B>(body: B) -> impl Stream<Item = Result<Bytes, B::Error>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let chunks = BodyDataStream::new(body);
    futures_util::stream::unfold(
        (chunks, FrameParser::default(), false),
        |(mut chunks, mut parser, mut ended)| async move {
            loop {
                if let Some(frame) = parser.pop() {
                    return Some((Ok(frame), (chunks, parser, ended)));
                }
                if ended {
                    return None;
                }
                match chunks.next().await {
                    Some(Ok(chunk)) => parser.feed(&chunk, false),
                    Some(Err(e)) => return Some((Err(e), (chunks, parser, true))),
                    None => {
                        parser.feed(b"", true);
                        ended = true;
                    }
                }
            }
        },
    )
}

/// Incremental SSE frame parser. `feed(chunk, end_of_input)` accepts new
/// bytes and queues any complete frames; `pop()` removes the next ready
/// frame. A trailing `\r` is ambiguous (could pair with `\n` from the
/// next chunk); only commits when `end_of_input` is true. The partial
/// frame after the last blank line (if any) is dropped: only
/// blank-line-terminated frames dispatch.
#[derive(Default)]
struct FrameParser {
    /// Bytes received but not yet committed to a line.
    buf: Vec<u8>,
    /// Current frame's `event` field (empty = "message" default).
    event: String,
    /// Current frame's accumulated `data` payload (multiple `data:`
    /// lines joined with `\n`).
    data: String,
    /// Whether the current frame has any `data:` field at all.
    data_seen: bool,
    /// Frames complete and ready to yield.
    pending: VecDeque<Bytes>,
}

impl FrameParser {
    fn feed(&mut self, chunk: &[u8], end_of_input: bool) {
        self.buf.extend_from_slice(chunk);
        while let Some((line_len, term_len)) = find_line_end(&self.buf, end_of_input) {
            let line: Vec<u8> = self.buf[..line_len].to_vec();
            self.buf.drain(..line_len + term_len);
            self.handle_line(&line);
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        if line.is_empty() {
            if self.data_seen && (self.event.is_empty() || self.event == "message") {
                self.pending
                    .push_back(Bytes::copy_from_slice(self.data.as_bytes()));
            }
            self.event.clear();
            self.data.clear();
            self.data_seen = false;
            return;
        }
        if line.starts_with(b":") {
            return;
        }
        let (field, value) = split_field(line);
        match field {
            b"event" => {
                self.event.clear();
                self.event.push_str(std::str::from_utf8(value).unwrap_or(""));
            }
            b"data" => {
                if self.data_seen {
                    self.data.push('\n');
                }
                self.data.push_str(std::str::from_utf8(value).unwrap_or(""));
                self.data_seen = true;
            }
            _ => {}
        }
    }

    fn pop(&mut self) -> Option<Bytes> {
        self.pending.pop_front()
    }
}

/// Find the next line terminator. Returns `(line_len, terminator_len)`.
/// Trailing `\r` at end of buffer waits for next byte unless
/// `end_of_input` is true.
fn find_line_end(buf: &[u8], end_of_input: bool) -> Option<(usize, usize)> {
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            return Some((i, 1));
        }
        if b == b'\r' {
            if i + 1 < buf.len() {
                if buf[i + 1] == b'\n' {
                    return Some((i, 2));
                }
                return Some((i, 1));
            }
            if end_of_input {
                return Some((i, 1));
            }
            return None;
        }
    }
    None
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

    // ── encode_one ────────────────────────────────────────────

    #[test]
    fn encode_one__produces_single_frame() {
        let bytes = encode_one(&rpc(1));
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: message\ndata: "));
        assert!(s.ends_with("\n\n"));
        assert_eq!(s.matches("\n\n").count(), 1);
    }

    #[test]
    fn encode_one__matches_encode_results_for_single_input() {
        let r = rpc(7);
        assert_eq!(encode_one(&r), encode_results(&[r]));
    }

    // ── decode_frame_stream ───────────────────────────────────

    /// Body that yields each preset chunk as a separate `poll_frame`,
    /// then ends. Drives chunk-boundary tests.
    struct ChunkedBody {
        chunks: VecDeque<Bytes>,
    }

    impl ChunkedBody {
        fn new(chunks: Vec<&'static [u8]>) -> Self {
            Self {
                chunks: chunks.into_iter().map(Bytes::from_static).collect(),
            }
        }

        async fn drain(self) -> Vec<Bytes> {
            let stream = decode_frame_stream(self);
            let mut out = Vec::new();
            futures_util::pin_mut!(stream);
            while let Some(item) = stream.next().await {
                out.push(item.unwrap());
            }
            out
        }
    }

    impl hyper::body::Body for ChunkedBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            match self.chunks.pop_front() {
                Some(b) => std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(b)))),
                None => std::task::Poll::Ready(None),
            }
        }
    }

    #[tokio::test]
    async fn decode_frame_stream__single_frame_in_one_chunk() {
        let frames = ChunkedBody::new(vec![b"event: message\ndata: hi\n\n"])
            .drain()
            .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "hi");
    }

    #[tokio::test]
    async fn decode_frame_stream__multiple_frames_in_one_chunk() {
        let frames = ChunkedBody::new(vec![b"data: a\n\ndata: b\n\ndata: c\n\n"])
            .drain()
            .await;
        assert_eq!(frames.len(), 3);
        assert_eq!(data_str(&frames[0]), "a");
        assert_eq!(data_str(&frames[1]), "b");
        assert_eq!(data_str(&frames[2]), "c");
    }

    #[tokio::test]
    async fn decode_frame_stream__frame_split_across_chunks() {
        let frames = ChunkedBody::new(vec![b"data: hel", b"lo\n\n"])
            .drain()
            .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "hello");
    }

    #[tokio::test]
    async fn decode_frame_stream__chunk_ends_mid_field_name() {
        let frames = ChunkedBody::new(vec![b"data", b": x\n\n"]).drain().await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "x");
    }

    #[tokio::test]
    async fn decode_frame_stream__chunk_ends_with_carriage_return() {
        // \r at chunk boundary is ambiguous (could pair with \n); parser
        // must wait for the next chunk before deciding.
        let frames = ChunkedBody::new(vec![b"data: x\r", b"\n\r\n"])
            .drain()
            .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "x");
    }

    #[tokio::test]
    async fn decode_frame_stream__non_message_event_dropped() {
        let frames = ChunkedBody::new(vec![
            b"event: ping\ndata: ignored\n\nevent: message\ndata: kept\n\n",
        ])
        .drain()
        .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "kept");
    }

    #[tokio::test]
    async fn decode_frame_stream__keepalive_comment_skipped() {
        let frames = ChunkedBody::new(vec![b": keepalive\n\ndata: hi\n\n"])
            .drain()
            .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "hi");
    }

    #[tokio::test]
    async fn decode_frame_stream__trailing_partial_frame_dropped() {
        let frames = ChunkedBody::new(vec![b"data: complete\n\ndata: partial"])
            .drain()
            .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(data_str(&frames[0]), "complete");
    }

    #[tokio::test]
    async fn decode_frame_stream__empty_body_yields_nothing() {
        assert!(ChunkedBody::new(vec![]).drain().await.is_empty());
    }

    #[tokio::test]
    async fn decode_frame_stream__yields_frame_before_body_ends() {
        // First frame must pop before the body's second poll. Encodes
        // "stream emits as soon as a complete frame lands"; exercises
        // the pop-pending branch in the unfold loop.
        struct GatedBody {
            first: Option<Bytes>,
        }

        impl hyper::body::Body for GatedBody {
            type Data = Bytes;
            type Error = std::convert::Infallible;
            fn poll_frame(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>>
            {
                if let Some(b) = self.first.take() {
                    return std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(b))));
                }
                std::task::Poll::Pending
            }
        }

        let body = GatedBody {
            first: Some(Bytes::from_static(b"data: first\n\n")),
        };
        let stream = decode_frame_stream(body);
        futures_util::pin_mut!(stream);
        let frame = stream.next().await.unwrap().unwrap();
        assert_eq!(data_str(&frame), "first");
    }
}
