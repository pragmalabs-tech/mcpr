//! Shallow JSON-RPC envelope for the target proxy pipeline.
//!
//! See `PIPELINE.md` §Types. Intake parses only
//! the envelope: `jsonrpc`, `id`, `method`, `params`, `result`, `error`.
//! `params` and `result` stay as [`RawValue`] — unparsed bytes — until a
//! middleware opts in to a typed view via [`JsonRpcEnvelope::params_as`]
//! or [`JsonRpcEnvelope::result_as`].
//!
//! MCP 2025-11-25 does not batch, so [`JsonRpcEnvelope::parse`] rejects
//! top-level JSON arrays.

use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::value::RawValue;

/// Shallow parse of a single JSON-RPC 2.0 message.
#[derive(Debug, Clone)]
pub struct JsonRpcEnvelope {
    pub id: Option<JsonRpcId>,
    pub method: Option<String>,
    pub params: Option<Box<RawValue>>,
    pub result: Option<Box<RawValue>>,
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC id. `Null` is valid per spec for replies to un-parseable
/// requests. Absent id (request-without-id, i.e. a notification) is
/// `None` on the envelope's `id` field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
    Null,
}

/// JSON-RPC error object. `data` stays as raw bytes so middlewares pay
/// no cost when they don't inspect it.
#[derive(Debug, Clone)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Box<RawValue>>,
}

/// Reason [`JsonRpcEnvelope::parse`] declined the bytes. These are soft
/// signals for the intake layer — a `NotJson` body is not an error to
/// the proxy, just a hint to try the next classification branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Bytes are not valid JSON.
    NotJson,
    /// Valid JSON, but missing or wrong `jsonrpc` field.
    NotJsonRpc20,
    /// Valid JSON-RPC 2.0 object, but the field combination does not
    /// match any of the four kinds (Request, Notification, Result,
    /// Error). Batches (top-level arrays) land here.
    InvalidShape,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    jsonrpc: Option<String>,
    // `some_value` preserves JSON `null` as `Some(Value::Null)` instead
    // of collapsing it to `None` — needed to distinguish an absent id
    // from an explicit null id (spec-legal reply to an un-parseable
    // request).
    #[serde(default, deserialize_with = "some_value")]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
    #[serde(default)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<RawError>,
}

fn some_value<'de, D>(d: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    serde_json::Value::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct RawError {
    code: i32,
    message: String,
    #[serde(default)]
    data: Option<Box<RawValue>>,
}

impl JsonRpcEnvelope {
    /// Parse a single JSON-RPC 2.0 message. Rejects batches.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if first_non_ws(bytes) == Some(b'[') {
            return Err(ParseError::InvalidShape);
        }

        let raw: Raw = serde_json::from_slice(bytes).map_err(|_| ParseError::NotJson)?;

        if raw.jsonrpc.as_deref() != Some("2.0") {
            return Err(ParseError::NotJsonRpc20);
        }

        let id = match raw.id {
            None => None,
            Some(serde_json::Value::Null) => Some(JsonRpcId::Null),
            Some(serde_json::Value::Number(n)) => Some(JsonRpcId::Number(
                n.as_i64().ok_or(ParseError::InvalidShape)?,
            )),
            Some(serde_json::Value::String(s)) => Some(JsonRpcId::String(s)),
            Some(_) => return Err(ParseError::InvalidShape),
        };

        let error = raw.error.map(|e| JsonRpcError {
            code: e.code,
            message: e.message,
            data: e.data,
        });

        let shape = (
            raw.method.is_some(),
            id.is_some(),
            raw.result.is_some(),
            error.is_some(),
        );
        // The four legal shapes: request, notification, result reply, error reply.
        let valid = matches!(
            shape,
            (true,  true,  false, false)  // request
            | (true,  false, false, false) // notification
            | (false, true,  true,  false) // result reply
            | (false, true,  false, true) // error reply
        );
        if !valid {
            return Err(ParseError::InvalidShape);
        }

        Ok(JsonRpcEnvelope {
            id,
            method: raw.method,
            params: raw.params,
            result: raw.result,
            error,
        })
    }

    /// Deserialize `params` into `T`. Returns `None` if `params` is
    /// absent or does not match `T`'s shape.
    pub fn params_as<T: DeserializeOwned>(&self) -> Option<T> {
        let raw = self.params.as_ref()?;
        serde_json::from_str(raw.get()).ok()
    }

    /// Deserialize `result` into `T`. Returns `None` if `result` is
    /// absent or does not match `T`'s shape.
    pub fn result_as<T: DeserializeOwned>(&self) -> Option<T> {
        let raw = self.result.as_ref()?;
        serde_json::from_str(raw.get()).ok()
    }
}

fn first_non_ws(bytes: &[u8]) -> Option<u8> {
    bytes.iter().copied().find(|b| !b.is_ascii_whitespace())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Greet {
        name: String,
    }

    // ── parse happy paths ─────────────────────────────────────

    #[test]
    fn parse__request_shape() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(env.id, Some(JsonRpcId::Number(1)));
        assert_eq!(env.method.as_deref(), Some("tools/list"));
        assert!(env.params.is_some());
        assert!(env.result.is_none());
        assert!(env.error.is_none());
    }

    #[test]
    fn parse__notification_shape() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"p":0.5}}"#,
        )
        .unwrap();
        assert!(env.id.is_none());
        assert_eq!(env.method.as_deref(), Some("notifications/progress"));
    }

    #[test]
    fn parse__result_shape() {
        let env =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":"r1","result":{"ok":true}}"#).unwrap();
        assert_eq!(env.id, Some(JsonRpcId::String("r1".into())));
        assert!(env.result.is_some());
    }

    #[test]
    fn parse__error_shape() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":7,"error":{"code":-32600,"message":"invalid"}}"#,
        )
        .unwrap();
        let err = env.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "invalid");
    }

    #[test]
    fn parse__null_id_accepted() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
        )
        .unwrap();
        assert_eq!(env.id, Some(JsonRpcId::Null));
    }

    #[test]
    fn parse__id_fractional_number_rejected() {
        let err =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#).unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    // ── parse error cases ─────────────────────────────────────

    #[test]
    fn parse__empty_body_returns_not_json() {
        assert_eq!(
            JsonRpcEnvelope::parse(b"").unwrap_err(),
            ParseError::NotJson
        );
    }

    #[test]
    fn parse__garbage_bytes_return_not_json() {
        assert_eq!(
            JsonRpcEnvelope::parse(b"not json at all").unwrap_err(),
            ParseError::NotJson,
        );
    }

    #[test]
    fn parse__missing_jsonrpc_field() {
        let err = JsonRpcEnvelope::parse(br#"{"id":1,"method":"foo"}"#).unwrap_err();
        assert_eq!(err, ParseError::NotJsonRpc20);
    }

    #[test]
    fn parse__wrong_jsonrpc_version() {
        let err =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"1.0","id":1,"method":"foo"}"#).unwrap_err();
        assert_eq!(err, ParseError::NotJsonRpc20);
    }

    #[test]
    fn parse__bare_jsonrpc_is_invalid_shape() {
        let err = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0"}"#).unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    #[test]
    fn parse__top_level_array_rejected() {
        let err = JsonRpcEnvelope::parse(br#"[{"jsonrpc":"2.0","method":"x"}]"#).unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    #[test]
    fn parse__top_level_array_with_leading_ws_rejected() {
        let err = JsonRpcEnvelope::parse(b"   [ ]").unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    #[test]
    fn parse__both_result_and_error_rejected() {
        let err = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#,
        )
        .unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    #[test]
    fn parse__response_without_id_rejected() {
        let err = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","result":{}}"#).unwrap_err();
        assert_eq!(err, ParseError::InvalidShape);
    }

    // ── params_as / result_as ─────────────────────────────────

    #[test]
    fn params_as__deserializes_on_match() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":1,"method":"greet","params":{"name":"rod"}}"#,
        )
        .unwrap();
        assert_eq!(env.params_as::<Greet>(), Some(Greet { name: "rod".into() }));
    }

    #[test]
    fn params_as__none_on_mismatch() {
        let env = JsonRpcEnvelope::parse(
            br#"{"jsonrpc":"2.0","id":1,"method":"greet","params":{"wrong":1}}"#,
        )
        .unwrap();
        assert!(env.params_as::<Greet>().is_none());
    }

    #[test]
    fn params_as__none_when_absent() {
        let env = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1,"method":"greet"}"#).unwrap();
        assert!(env.params_as::<Greet>().is_none());
    }

    #[test]
    fn result_as__deserializes_on_match() {
        let env =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"name":"rod"}}"#).unwrap();
        assert_eq!(env.result_as::<Greet>(), Some(Greet { name: "rod".into() }));
    }
}
