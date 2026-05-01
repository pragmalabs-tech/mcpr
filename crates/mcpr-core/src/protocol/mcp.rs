// MCP JSON-RPC types.
// Spec: https://modelcontextprotocol.io/specification/2025-11-25/schema

use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use strum::{EnumString, IntoStaticStr};

/// JSON-RPC request id. Per JSON-RPC 2.0, this is `Number | String | Null`.
/// `Null` shows up on error responses where the server couldn't parse the
/// request id (e.g. parse error, invalid request envelope).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(i64),
    String(String),
    Null,
}

impl Serialize for RequestId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            RequestId::Number(n) => s.serialize_i64(*n),
            RequestId::String(v) => s.serialize_str(v),
            RequestId::Null => s.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        match v {
            Value::Number(n) => n
                .as_i64()
                .map(RequestId::Number)
                .ok_or_else(|| serde::de::Error::custom("request id: expected integer")),
            Value::String(s) => Ok(RequestId::String(s)),
            Value::Null => Ok(RequestId::Null),
            _ => Err(serde::de::Error::custom(
                "request id: expected number, string, or null",
            )),
        }
    }
}

/// `"2.0"` literal — rejects anything else on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(JsonRpcVersion)
        } else {
            Err(serde::de::Error::custom("expected jsonrpc \"2.0\""))
        }
    }
}

/// Client identity extracted from the MCP `initialize` request's `clientInfo` param.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub name: String,
    pub version: Option<String>,
}

/// Server identity extracted from the MCP `initialize` response's `serverInfo` field.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: Option<String>,
}

/// JSON-RPC error. `data` is kept raw — middlewares skip parsing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Box<RawValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: ClientMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Map<String, Value>>,
}

impl JsonRpcRequest {
    /// Tool name from `tools/call` params.
    pub fn get_tool(&self) -> Option<&str> {
        if !matches!(self.method, ClientMethod::Tools(ToolsMethod::Call)) {
            return None;
        }
        self.params.as_ref()?.get("name")?.as_str()
    }

    /// Resource URI from `resources/read|subscribe|unsubscribe` params.
    pub fn get_resource_uri(&self) -> Option<&str> {
        if !matches!(
            self.method,
            ClientMethod::Resources(
                ResourcesMethod::Read | ResourcesMethod::Subscribe | ResourcesMethod::Unsubscribe
            )
        ) {
            return None;
        }
        self.params.as_ref()?.get("uri")?.as_str()
    }

    /// Prompt name from `prompts/get` params.
    pub fn get_prompt(&self) -> Option<&str> {
        if !matches!(self.method, ClientMethod::Prompts(PromptsMethod::Get)) {
            return None;
        }
        self.params.as_ref()?.get("name")?.as_str()
    }

    /// Extract `clientInfo` from MCP initialize request params.
    pub fn parse_client_info(&self) -> Option<ClientInfo> {
        let params = self.params.as_ref()?;
        let client_info = params.get("clientInfo")?;
        let name = client_info.get("name")?.as_str()?.to_string();
        let version = client_info
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        Some(ClientInfo { name, version })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

impl JsonRpcResponse {
    /// Extract `serverInfo` from MCP initialize response result.
    pub fn parse_server_info(&self) -> Option<ServerInfo> {
        let server_info = self.result.as_ref()?.get("serverInfo")?;
        let name = server_info.get("name")?.as_str()?.to_string();
        let version = server_info
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        Some(ServerInfo { name, version })
    }
}

/// Wire envelope for a JSON-RPC error response: `{jsonrpc, id, error}`.
/// Per spec, `id` may be `Null` when the server couldn't determine the
/// request id (parse error, invalid request).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub error: JsonRpcError,
}

/// Untagged dispatch: presence of `result` vs `error` decides the variant.
/// `deny_unknown_fields` on each struct prevents the wrong shape from
/// silently matching with the other field ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResult {
    Response(JsonRpcResponse),
    Error(JsonRpcErrorResponse),
}

/// Envelope + direction/method classification.
#[derive(Debug, Clone)]
pub struct McpMessage {
    pub envelope: JsonRpcRequest,
    pub kind: MessageKind,
}

/// Direction tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    Client(ClientKind),
    Server(ServerKind),
}

// ── Client → Server ──────────────────────────────────────────

/// Classified at intake from `method` + `id` + `result`/`error` presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKind {
    Request(ClientMethod),
    Notification(ClientNotifMethod),
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMethod {
    Ping,
    Lifecycle(LifecycleMethod),
    Tools(ToolsMethod),
    Resources(ResourcesMethod),
    Prompts(PromptsMethod),
    Completion(CompletionMethod),
    Logging(LoggingMethod),
    Tasks(TasksMethod),
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum LifecycleMethod {
    #[strum(serialize = "initialize")]
    Initialize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum ToolsMethod {
    #[strum(serialize = "tools/list")]
    List,
    #[strum(serialize = "tools/call")]
    Call,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum ResourcesMethod {
    #[strum(serialize = "resources/list")]
    List,
    #[strum(serialize = "resources/templates/list")]
    TemplatesList,
    #[strum(serialize = "resources/read")]
    Read,
    #[strum(serialize = "resources/subscribe")]
    Subscribe,
    #[strum(serialize = "resources/unsubscribe")]
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum PromptsMethod {
    #[strum(serialize = "prompts/list")]
    List,
    #[strum(serialize = "prompts/get")]
    Get,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum CompletionMethod {
    #[strum(serialize = "completion/complete")]
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum LoggingMethod {
    #[strum(serialize = "logging/setLevel")]
    SetLevel,
}

/// Task lifecycle. Used in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum TasksMethod {
    #[strum(serialize = "tasks/list")]
    List,
    #[strum(serialize = "tasks/get")]
    Get,
    #[strum(serialize = "tasks/result")]
    Result,
    #[strum(serialize = "tasks/cancel")]
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString)]
pub enum ClientNotifMethod {
    #[strum(serialize = "notifications/initialized")]
    Initialized,
    #[strum(serialize = "notifications/cancelled")]
    Cancelled,
    #[strum(serialize = "notifications/progress")]
    Progress,
    #[strum(serialize = "notifications/roots/list_changed")]
    RootsListChanged,
    #[strum(serialize = "notifications/tasks/status")]
    TaskStatus,
    #[strum(default)]
    Unknown(String),
}

// ── Server → Client ──────────────────────────────────────────

/// Appears in response bodies (HTTP chunks or SSE frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKind {
    Request(ServerMethod),
    Notification(ServerNotifMethod),
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString)]
pub enum ServerMethod {
    #[strum(serialize = "ping")]
    Ping,
    #[strum(serialize = "sampling/createMessage")]
    Sampling,
    #[strum(serialize = "elicitation/create")]
    Elicitation,
    #[strum(serialize = "roots/list")]
    Roots,
    #[strum(disabled)]
    Tasks(TasksMethod),
    #[strum(disabled)]
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString)]
pub enum ServerNotifMethod {
    #[strum(serialize = "notifications/cancelled")]
    Cancelled,
    #[strum(serialize = "notifications/progress")]
    Progress,
    #[strum(serialize = "notifications/message")]
    LogMessage,
    #[strum(serialize = "notifications/resources/list_changed")]
    ResourcesListChanged,
    #[strum(serialize = "notifications/resources/updated")]
    ResourceUpdated,
    #[strum(serialize = "notifications/tools/list_changed")]
    ToolsListChanged,
    #[strum(serialize = "notifications/prompts/list_changed")]
    PromptsListChanged,
    #[strum(serialize = "notifications/elicitation/complete")]
    ElicitationComplete,
    #[strum(serialize = "notifications/tasks/status")]
    TaskStatus,
    #[strum(default)]
    Unknown(String),
}

// ── Method parsing ────────────────────────────────────────────
// Outer enums hold inner newtype variants — strum can't dispatch into
// them, so `parse` cascades and falls back to `Unknown(_)`.

impl ClientMethod {
    pub fn parse(method: &str) -> Self {
        if method == "ping" {
            return Self::Ping;
        }
        if let Ok(m) = LifecycleMethod::from_str(method) {
            return Self::Lifecycle(m);
        }
        if let Ok(m) = ToolsMethod::from_str(method) {
            return Self::Tools(m);
        }
        if let Ok(m) = ResourcesMethod::from_str(method) {
            return Self::Resources(m);
        }
        if let Ok(m) = PromptsMethod::from_str(method) {
            return Self::Prompts(m);
        }
        if let Ok(m) = CompletionMethod::from_str(method) {
            return Self::Completion(m);
        }
        if let Ok(m) = LoggingMethod::from_str(method) {
            return Self::Logging(m);
        }
        if let Ok(m) = TasksMethod::from_str(method) {
            return Self::Tasks(m);
        }
        Self::Unknown(method.to_owned())
    }

    /// Inverse of `parse`. `None` for `Unknown(_)`.
    pub fn as_str(&self) -> Option<&'static str> {
        Some(match self {
            Self::Ping => "ping",
            Self::Lifecycle(m) => m.into(),
            Self::Tools(m) => m.into(),
            Self::Resources(m) => m.into(),
            Self::Prompts(m) => m.into(),
            Self::Completion(m) => m.into(),
            Self::Logging(m) => m.into(),
            Self::Tasks(m) => m.into(),
            Self::Unknown(_) => return None,
        })
    }
}

impl ClientNotifMethod {
    pub fn parse(method: &str) -> Self {
        // Infallible — `Unknown(String)` is the strum default.
        Self::from_str(method).unwrap_or_else(|_| Self::Unknown(method.to_owned()))
    }
}

impl ServerMethod {
    pub fn parse(method: &str) -> Self {
        if let Ok(m) = <Self as FromStr>::from_str(method) {
            return m;
        }
        if let Ok(m) = TasksMethod::from_str(method) {
            return Self::Tasks(m);
        }
        Self::Unknown(method.to_owned())
    }

    /// Inverse of `parse`. `None` for `Unknown(_)`.
    pub fn as_str(&self) -> Option<&'static str> {
        Some(match self {
            Self::Ping => "ping",
            Self::Sampling => "sampling/createMessage",
            Self::Elicitation => "elicitation/create",
            Self::Roots => "roots/list",
            Self::Tasks(m) => m.into(),
            Self::Unknown(_) => return None,
        })
    }
}

impl ServerNotifMethod {
    pub fn parse(method: &str) -> Self {
        // Infallible — `Unknown(String)` is the strum default.
        Self::from_str(method).unwrap_or_else(|_| Self::Unknown(method.to_owned()))
    }
}

// ── Serde for method enums ────────────────────────────────────
// Wire form is the method string, not serde's default tagged shape.

impl Serialize for ClientMethod {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown(name) => s.serialize_str(name),
            other => s.serialize_str(other.as_str().expect("non-Unknown")),
        }
    }
}

impl<'de> Deserialize<'de> for ClientMethod {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s))
    }
}

impl Serialize for ServerMethod {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown(name) => s.serialize_str(name),
            other => s.serialize_str(other.as_str().expect("non-Unknown")),
        }
    }
}

impl<'de> Deserialize<'de> for ServerMethod {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s))
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── JsonRpcVersion ────────────────────────────────────────

    #[test]
    fn jsonrpc_version__serializes_as_2_0() {
        assert_eq!(serde_json::to_string(&JsonRpcVersion).unwrap(), "\"2.0\"");
    }

    #[test]
    fn jsonrpc_version__accepts_2_0() {
        let v: JsonRpcVersion = serde_json::from_str("\"2.0\"").unwrap();
        assert_eq!(v, JsonRpcVersion);
    }

    #[test]
    fn jsonrpc_version__rejects_other_strings() {
        assert!(serde_json::from_str::<JsonRpcVersion>("\"1.0\"").is_err());
        assert!(serde_json::from_str::<JsonRpcVersion>("\"2\"").is_err());
        assert!(serde_json::from_str::<JsonRpcVersion>("\"\"").is_err());
    }

    #[test]
    fn jsonrpc_version__rejects_non_string() {
        assert!(serde_json::from_str::<JsonRpcVersion>("2.0").is_err());
        assert!(serde_json::from_str::<JsonRpcVersion>("null").is_err());
    }

    // ── RequestId ─────────────────────────────────────────────

    #[test]
    fn request_id__number_serializes_as_bare_number() {
        assert_eq!(serde_json::to_string(&RequestId::Number(42)).unwrap(), "42");
    }

    #[test]
    fn request_id__string_serializes_as_bare_string() {
        assert_eq!(
            serde_json::to_string(&RequestId::String("req-1".into())).unwrap(),
            "\"req-1\""
        );
    }

    #[test]
    fn request_id__deserializes_bare_number() {
        let id: RequestId = serde_json::from_str("42").unwrap();
        assert_eq!(id, RequestId::Number(42));
    }

    #[test]
    fn request_id__deserializes_bare_string() {
        let id: RequestId = serde_json::from_str("\"req-1\"").unwrap();
        assert_eq!(id, RequestId::String("req-1".into()));
    }

    // ── ClientMethod parse / as_str ───────────────────────────

    #[test]
    fn client_method__parse_ping() {
        assert_eq!(ClientMethod::parse("ping"), ClientMethod::Ping);
    }

    #[test]
    fn client_method__parse_initialize() {
        assert_eq!(
            ClientMethod::parse("initialize"),
            ClientMethod::Lifecycle(LifecycleMethod::Initialize)
        );
    }

    #[test]
    fn client_method__parse_tools_call() {
        assert_eq!(
            ClientMethod::parse("tools/call"),
            ClientMethod::Tools(ToolsMethod::Call)
        );
    }

    #[test]
    fn client_method__parse_resources_templates_list() {
        assert_eq!(
            ClientMethod::parse("resources/templates/list"),
            ClientMethod::Resources(ResourcesMethod::TemplatesList)
        );
    }

    #[test]
    fn client_method__parse_prompts_get() {
        assert_eq!(
            ClientMethod::parse("prompts/get"),
            ClientMethod::Prompts(PromptsMethod::Get)
        );
    }

    #[test]
    fn client_method__parse_completion_complete() {
        assert_eq!(
            ClientMethod::parse("completion/complete"),
            ClientMethod::Completion(CompletionMethod::Complete)
        );
    }

    #[test]
    fn client_method__parse_logging_set_level() {
        assert_eq!(
            ClientMethod::parse("logging/setLevel"),
            ClientMethod::Logging(LoggingMethod::SetLevel)
        );
    }

    #[test]
    fn client_method__parse_tasks_get() {
        assert_eq!(
            ClientMethod::parse("tasks/get"),
            ClientMethod::Tasks(TasksMethod::Get)
        );
    }

    #[test]
    fn client_method__parse_unknown_preserves_string() {
        assert_eq!(
            ClientMethod::parse("custom/method"),
            ClientMethod::Unknown("custom/method".into())
        );
    }

    #[test]
    fn client_method__as_str_unknown_returns_none() {
        assert_eq!(ClientMethod::Unknown("x".into()).as_str(), None);
    }

    #[test]
    fn client_method__roundtrip_each_family() {
        for s in [
            "ping",
            "initialize",
            "tools/list",
            "tools/call",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "resources/subscribe",
            "resources/unsubscribe",
            "prompts/list",
            "prompts/get",
            "completion/complete",
            "logging/setLevel",
            "tasks/list",
            "tasks/get",
            "tasks/result",
            "tasks/cancel",
        ] {
            assert_eq!(ClientMethod::parse(s).as_str(), Some(s), "method {s}");
        }
    }

    // ── ServerMethod parse / as_str ───────────────────────────

    #[test]
    fn server_method__parse_unit_variants() {
        assert_eq!(ServerMethod::parse("ping"), ServerMethod::Ping);
        assert_eq!(
            ServerMethod::parse("sampling/createMessage"),
            ServerMethod::Sampling
        );
        assert_eq!(
            ServerMethod::parse("elicitation/create"),
            ServerMethod::Elicitation
        );
        assert_eq!(ServerMethod::parse("roots/list"), ServerMethod::Roots);
    }

    #[test]
    fn server_method__parse_tasks_newtype() {
        assert_eq!(
            ServerMethod::parse("tasks/cancel"),
            ServerMethod::Tasks(TasksMethod::Cancel)
        );
    }

    #[test]
    fn server_method__parse_unknown_preserves_string() {
        assert_eq!(
            ServerMethod::parse("custom/method"),
            ServerMethod::Unknown("custom/method".into())
        );
    }

    #[test]
    fn server_method__as_str_unknown_returns_none() {
        assert_eq!(ServerMethod::Unknown("x".into()).as_str(), None);
    }

    #[test]
    fn server_method__roundtrip_each_family() {
        for s in [
            "ping",
            "sampling/createMessage",
            "elicitation/create",
            "roots/list",
            "tasks/list",
            "tasks/cancel",
        ] {
            assert_eq!(ServerMethod::parse(s).as_str(), Some(s), "method {s}");
        }
    }

    // ── ClientMethod / ServerMethod serde ─────────────────────

    #[test]
    fn client_method__serializes_as_string() {
        let m = ClientMethod::Tools(ToolsMethod::Call);
        assert_eq!(serde_json::to_string(&m).unwrap(), "\"tools/call\"");
    }

    #[test]
    fn client_method__serialize_unknown_preserves_string() {
        let m = ClientMethod::Unknown("x/y".into());
        assert_eq!(serde_json::to_string(&m).unwrap(), "\"x/y\"");
    }

    #[test]
    fn client_method__deserialize_known() {
        let m: ClientMethod = serde_json::from_str("\"tools/list\"").unwrap();
        assert_eq!(m, ClientMethod::Tools(ToolsMethod::List));
    }

    #[test]
    fn client_method__deserialize_unknown() {
        let m: ClientMethod = serde_json::from_str("\"custom/method\"").unwrap();
        assert_eq!(m, ClientMethod::Unknown("custom/method".into()));
    }

    #[test]
    fn server_method__serializes_as_string() {
        let m = ServerMethod::Sampling;
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            "\"sampling/createMessage\""
        );
    }

    #[test]
    fn server_method__serialize_tasks_newtype() {
        let m = ServerMethod::Tasks(TasksMethod::Cancel);
        assert_eq!(serde_json::to_string(&m).unwrap(), "\"tasks/cancel\"");
    }

    #[test]
    fn server_method__deserialize_unknown() {
        let m: ServerMethod = serde_json::from_str("\"foo\"").unwrap();
        assert_eq!(m, ServerMethod::Unknown("foo".into()));
    }

    // ── Notification methods ──────────────────────────────────

    #[test]
    fn client_notif__parse_known() {
        assert_eq!(
            ClientNotifMethod::parse("notifications/initialized"),
            ClientNotifMethod::Initialized
        );
    }

    #[test]
    fn client_notif__parse_unknown_falls_back() {
        assert_eq!(
            ClientNotifMethod::parse("notifications/custom"),
            ClientNotifMethod::Unknown("notifications/custom".into())
        );
    }

    #[test]
    fn server_notif__parse_known() {
        assert_eq!(
            ServerNotifMethod::parse("notifications/tools/list_changed"),
            ServerNotifMethod::ToolsListChanged
        );
    }

    #[test]
    fn server_notif__parse_unknown_falls_back() {
        assert_eq!(
            ServerNotifMethod::parse("custom"),
            ServerNotifMethod::Unknown("custom".into())
        );
    }

    // ── JsonRpcRequest serde ──────────────────────────────────

    #[test]
    fn jsonrpc_request__roundtrip() {
        let mut params = Map::new();
        params.insert("name".into(), json!("get_weather"));
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Tools(ToolsMethod::Call),
            params: Some(params),
        };
        let back: JsonRpcRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back.id, req.id);
        assert_eq!(back.method, req.method);
        assert_eq!(back.params, req.params);
    }

    #[test]
    fn jsonrpc_request__method_serialized_as_plain_string() {
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Ping,
            params: None,
        };
        let v: Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], json!("2.0"));
        assert_eq!(v["method"], json!("ping"));
    }

    #[test]
    fn jsonrpc_request__params_skipped_when_none() {
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Ping,
            params: None,
        };
        assert!(!serde_json::to_string(&req).unwrap().contains("params"));
    }

    #[test]
    fn jsonrpc_request__rejects_wrong_version() {
        let bad = json!({"jsonrpc": "1.0", "id": 1, "method": "ping"});
        assert!(serde_json::from_value::<JsonRpcRequest>(bad).is_err());
    }

    // ── JsonRpcResult untagged dispatch ───────────────────────

    #[test]
    fn jsonrpc_result__parses_response_shape() {
        let v = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let r: JsonRpcResult = serde_json::from_value(v).unwrap();
        assert!(matches!(r, JsonRpcResult::Response(_)));
    }

    #[test]
    fn jsonrpc_result__parses_error_envelope_shape() {
        let v = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": "boom"},
        });
        let r: JsonRpcResult = serde_json::from_value(v).unwrap();
        let JsonRpcResult::Error(envelope) = r else {
            panic!("expected JsonRpcResult::Error");
        };
        assert_eq!(envelope.id, RequestId::Number(1));
        assert_eq!(envelope.error.code, -32000);
        assert_eq!(envelope.error.message, "boom");
    }

    #[test]
    fn jsonrpc_result__parses_error_envelope_with_null_id() {
        // The upstream body that broke the Inspector flow: a 4xx-like
        // JSON-RPC error response with `id: null` (set when the server
        // couldn't determine the request id, per JSON-RPC 2.0 §5.1).
        let v = json!({
            "jsonrpc": "2.0",
            "error": {"code": -32000, "message": "Bad Request: No valid session ID"},
            "id": null,
        });
        let r: JsonRpcResult = serde_json::from_value(v).unwrap();
        let JsonRpcResult::Error(envelope) = r else {
            panic!("expected JsonRpcResult::Error");
        };
        assert_eq!(envelope.id, RequestId::Null);
        assert_eq!(envelope.error.code, -32000);
    }

    #[test]
    fn jsonrpc_result__bare_error_is_rejected() {
        // Bare `{code, message}` was the old (incorrect) deserialization
        // target. With the envelope shape, it must fail.
        let v = json!({"code": -32000, "message": "boom"});
        assert!(serde_json::from_value::<JsonRpcResult>(v).is_err());
    }

    #[test]
    fn request_id__deserializes_null() {
        let v = json!(null);
        let id: RequestId = serde_json::from_value(v).unwrap();
        assert_eq!(id, RequestId::Null);
    }

    #[test]
    fn request_id__null_serializes_as_json_null() {
        let id = RequestId::Null;
        let v = serde_json::to_value(id).unwrap();
        assert!(v.is_null());
    }

    // ── get_tool / get_resource_uri / get_prompt ──────────────

    fn req(method: ClientMethod, params: Option<Map<String, Value>>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method,
            params,
        }
    }

    fn one_param(k: &str, v: Value) -> Option<Map<String, Value>> {
        let mut m = Map::new();
        m.insert(k.into(), v);
        Some(m)
    }

    #[test]
    fn get_tool__returns_name_for_tools_call() {
        let r = req(
            ClientMethod::Tools(ToolsMethod::Call),
            one_param("name", json!("get_weather")),
        );
        assert_eq!(r.get_tool(), Some("get_weather"));
    }

    #[test]
    fn get_tool__none_for_tools_list() {
        let r = req(
            ClientMethod::Tools(ToolsMethod::List),
            one_param("name", json!("get_weather")),
        );
        assert_eq!(r.get_tool(), None);
    }

    #[test]
    fn get_tool__none_when_params_missing() {
        let r = req(ClientMethod::Tools(ToolsMethod::Call), None);
        assert_eq!(r.get_tool(), None);
    }

    #[test]
    fn get_tool__none_when_name_not_string() {
        let r = req(
            ClientMethod::Tools(ToolsMethod::Call),
            one_param("name", json!(42)),
        );
        assert_eq!(r.get_tool(), None);
    }

    #[test]
    fn get_resource_uri__read() {
        let r = req(
            ClientMethod::Resources(ResourcesMethod::Read),
            one_param("uri", json!("file:///x")),
        );
        assert_eq!(r.get_resource_uri(), Some("file:///x"));
    }

    #[test]
    fn get_resource_uri__subscribe() {
        let r = req(
            ClientMethod::Resources(ResourcesMethod::Subscribe),
            one_param("uri", json!("file:///x")),
        );
        assert_eq!(r.get_resource_uri(), Some("file:///x"));
    }

    #[test]
    fn get_resource_uri__unsubscribe() {
        let r = req(
            ClientMethod::Resources(ResourcesMethod::Unsubscribe),
            one_param("uri", json!("file:///x")),
        );
        assert_eq!(r.get_resource_uri(), Some("file:///x"));
    }

    #[test]
    fn get_resource_uri__none_for_resources_list() {
        let r = req(
            ClientMethod::Resources(ResourcesMethod::List),
            one_param("uri", json!("file:///x")),
        );
        assert_eq!(r.get_resource_uri(), None);
    }

    #[test]
    fn get_resource_uri__none_when_params_missing() {
        let r = req(ClientMethod::Resources(ResourcesMethod::Read), None);
        assert_eq!(r.get_resource_uri(), None);
    }

    #[test]
    fn get_prompt__returns_name_for_prompts_get() {
        let r = req(
            ClientMethod::Prompts(PromptsMethod::Get),
            one_param("name", json!("greet")),
        );
        assert_eq!(r.get_prompt(), Some("greet"));
    }

    #[test]
    fn get_prompt__none_for_prompts_list() {
        let r = req(
            ClientMethod::Prompts(PromptsMethod::List),
            one_param("name", json!("greet")),
        );
        assert_eq!(r.get_prompt(), None);
    }

    #[test]
    fn get_prompt__none_when_params_missing() {
        let r = req(ClientMethod::Prompts(PromptsMethod::Get), None);
        assert_eq!(r.get_prompt(), None);
    }

    // ── parse_client_info ──────────────────────────────────────

    #[test]
    fn parse_client_info__name_and_version() {
        let r = req(
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            one_param(
                "clientInfo",
                json!({"name": "Claude Code", "version": "1.2.0"}),
            ),
        );
        let info = r.parse_client_info().unwrap();
        assert_eq!(info.name, "Claude Code");
        assert_eq!(info.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn parse_client_info__name_only() {
        let r = req(
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            one_param("clientInfo", json!({"name": "cursor"})),
        );
        let info = r.parse_client_info().unwrap();
        assert_eq!(info.name, "cursor");
        assert!(info.version.is_none());
    }

    #[test]
    fn parse_client_info__missing_clientinfo_is_none() {
        let r = req(
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            one_param("capabilities", json!({})),
        );
        assert!(r.parse_client_info().is_none());
    }

    #[test]
    fn parse_client_info__missing_name_is_none() {
        let r = req(
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            one_param("clientInfo", json!({"version": "1.0"})),
        );
        assert!(r.parse_client_info().is_none());
    }

    #[test]
    fn parse_client_info__non_string_name_is_none() {
        let r = req(
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            one_param("clientInfo", json!({"name": 42})),
        );
        assert!(r.parse_client_info().is_none());
    }

    #[test]
    fn parse_client_info__none_when_params_missing() {
        let r = req(ClientMethod::Lifecycle(LifecycleMethod::Initialize), None);
        assert!(r.parse_client_info().is_none());
    }
}
