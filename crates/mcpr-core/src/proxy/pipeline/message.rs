//! MCP message taxonomy (spec v2025-11-25).
//!
//! See `PIPELINE.md` §Types. Method identity
//! is a cheap enum — one string match per message. Grouping by feature
//! area (`Tools`, `Resources`, …) matches the spec table and lets
//! middlewares pattern-match at the granularity they need.
//!
//! Every method enum has an `Unknown(String)` tail variant so non-spec
//! methods forward unchanged instead of failing classification.

use super::envelope::JsonRpcEnvelope;

/// Shallow envelope paired with its classification. Used inside
/// `McpRequest` (client direction) and `Response::McpBuffered` (server
/// direction).
#[derive(Debug, Clone)]
pub struct McpMessage {
    pub envelope: JsonRpcEnvelope,
    pub kind: MessageKind,
}

/// Direction discriminator for an `McpMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    Client(ClientKind),
    Server(ServerKind),
}

// ── Client → Server ──────────────────────────────────────────

/// Kind of message the client is sending. Computed at intake from
/// `method` + `id` + `result`/`error` presence.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleMethod {
    Initialize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsMethod {
    List,
    Call,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcesMethod {
    List,
    TemplatesList,
    Read,
    Subscribe,
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptsMethod {
    List,
    Get,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionMethod {
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggingMethod {
    SetLevel,
}

/// Task lifecycle methods. Used by both directions (client asks the
/// server about tasks; server can also request task state from the
/// client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasksMethod {
    List,
    Get,
    Result,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientNotifMethod {
    Initialized,
    Cancelled,
    Progress,
    RootsListChanged,
    TaskStatus,
    Unknown(String),
}

// ── Server → Client ──────────────────────────────────────────

/// Kind of message the server is sending. Appears in response bodies
/// (streamable-HTTP chunks or legacy SSE frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKind {
    Request(ServerMethod),
    Notification(ServerNotifMethod),
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMethod {
    Ping,
    Sampling,
    Elicitation,
    Roots,
    Tasks(TasksMethod),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerNotifMethod {
    Cancelled,
    Progress,
    LogMessage,
    ResourcesListChanged,
    ResourceUpdated,
    ToolsListChanged,
    PromptsListChanged,
    ElicitationComplete,
    TaskStatus,
    Unknown(String),
}

// ── Method parsing ────────────────────────────────────────────

impl ClientMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            "ping" => Self::Ping,
            "initialize" => Self::Lifecycle(LifecycleMethod::Initialize),
            "tools/list" => Self::Tools(ToolsMethod::List),
            "tools/call" => Self::Tools(ToolsMethod::Call),
            "resources/list" => Self::Resources(ResourcesMethod::List),
            "resources/templates/list" => Self::Resources(ResourcesMethod::TemplatesList),
            "resources/read" => Self::Resources(ResourcesMethod::Read),
            "resources/subscribe" => Self::Resources(ResourcesMethod::Subscribe),
            "resources/unsubscribe" => Self::Resources(ResourcesMethod::Unsubscribe),
            "prompts/list" => Self::Prompts(PromptsMethod::List),
            "prompts/get" => Self::Prompts(PromptsMethod::Get),
            "completion/complete" => Self::Completion(CompletionMethod::Complete),
            "logging/setLevel" => Self::Logging(LoggingMethod::SetLevel),
            "tasks/list" => Self::Tasks(TasksMethod::List),
            "tasks/get" => Self::Tasks(TasksMethod::Get),
            "tasks/result" => Self::Tasks(TasksMethod::Result),
            "tasks/cancel" => Self::Tasks(TasksMethod::Cancel),
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl ClientNotifMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            "notifications/initialized" => Self::Initialized,
            "notifications/cancelled" => Self::Cancelled,
            "notifications/progress" => Self::Progress,
            "notifications/roots/list_changed" => Self::RootsListChanged,
            "notifications/tasks/status" => Self::TaskStatus,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl ServerMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            "ping" => Self::Ping,
            "sampling/createMessage" => Self::Sampling,
            "elicitation/create" => Self::Elicitation,
            "roots/list" => Self::Roots,
            "tasks/list" => Self::Tasks(TasksMethod::List),
            "tasks/get" => Self::Tasks(TasksMethod::Get),
            "tasks/result" => Self::Tasks(TasksMethod::Result),
            "tasks/cancel" => Self::Tasks(TasksMethod::Cancel),
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl ServerNotifMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            "notifications/cancelled" => Self::Cancelled,
            "notifications/progress" => Self::Progress,
            "notifications/message" => Self::LogMessage,
            "notifications/resources/list_changed" => Self::ResourcesListChanged,
            "notifications/resources/updated" => Self::ResourceUpdated,
            "notifications/tools/list_changed" => Self::ToolsListChanged,
            "notifications/prompts/list_changed" => Self::PromptsListChanged,
            "notifications/elicitation/complete" => Self::ElicitationComplete,
            "notifications/tasks/status" => Self::TaskStatus,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

// ── Classification ────────────────────────────────────────────

/// Classify a client→server envelope. Assumes the envelope came from
/// [`JsonRpcEnvelope::parse`], which already rejected malformed shapes.
pub fn classify_client(env: &JsonRpcEnvelope) -> ClientKind {
    match (
        env.method.as_deref(),
        env.id.is_some(),
        env.result.is_some(),
        env.error.is_some(),
    ) {
        (Some(m), true, false, false) => ClientKind::Request(ClientMethod::parse(m)),
        (Some(m), false, false, false) => ClientKind::Notification(ClientNotifMethod::parse(m)),
        (None, true, true, false) => ClientKind::Result,
        (None, true, false, true) => ClientKind::Error,
        _ => {
            debug_assert!(
                false,
                "classify_client: envelope shape should have been rejected by parse",
            );
            ClientKind::Error
        }
    }
}

/// Classify a server→client envelope.
pub fn classify_server(env: &JsonRpcEnvelope) -> ServerKind {
    match (
        env.method.as_deref(),
        env.id.is_some(),
        env.result.is_some(),
        env.error.is_some(),
    ) {
        (Some(m), true, false, false) => ServerKind::Request(ServerMethod::parse(m)),
        (Some(m), false, false, false) => ServerKind::Notification(ServerNotifMethod::parse(m)),
        (None, true, true, false) => ServerKind::Result,
        (None, true, false, true) => ServerKind::Error,
        _ => {
            debug_assert!(
                false,
                "classify_server: envelope shape should have been rejected by parse",
            );
            ServerKind::Error
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn parsed(bytes: &[u8]) -> JsonRpcEnvelope {
        JsonRpcEnvelope::parse(bytes).unwrap()
    }

    // ── ClientMethod::parse coverage ─────────────────────────

    #[test]
    fn client_method__spec_coverage() {
        let cases: &[(&str, ClientMethod)] = &[
            ("ping", ClientMethod::Ping),
            (
                "initialize",
                ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            ),
            ("tools/list", ClientMethod::Tools(ToolsMethod::List)),
            ("tools/call", ClientMethod::Tools(ToolsMethod::Call)),
            (
                "resources/list",
                ClientMethod::Resources(ResourcesMethod::List),
            ),
            (
                "resources/templates/list",
                ClientMethod::Resources(ResourcesMethod::TemplatesList),
            ),
            (
                "resources/read",
                ClientMethod::Resources(ResourcesMethod::Read),
            ),
            (
                "resources/subscribe",
                ClientMethod::Resources(ResourcesMethod::Subscribe),
            ),
            (
                "resources/unsubscribe",
                ClientMethod::Resources(ResourcesMethod::Unsubscribe),
            ),
            ("prompts/list", ClientMethod::Prompts(PromptsMethod::List)),
            ("prompts/get", ClientMethod::Prompts(PromptsMethod::Get)),
            (
                "completion/complete",
                ClientMethod::Completion(CompletionMethod::Complete),
            ),
            (
                "logging/setLevel",
                ClientMethod::Logging(LoggingMethod::SetLevel),
            ),
            ("tasks/list", ClientMethod::Tasks(TasksMethod::List)),
            ("tasks/get", ClientMethod::Tasks(TasksMethod::Get)),
            ("tasks/result", ClientMethod::Tasks(TasksMethod::Result)),
            ("tasks/cancel", ClientMethod::Tasks(TasksMethod::Cancel)),
        ];
        for (m, expected) in cases {
            assert_eq!(ClientMethod::parse(m), *expected, "method = {m}");
        }
    }

    #[test]
    fn client_method__unknown_preserves_string() {
        assert_eq!(
            ClientMethod::parse("tools/future-method"),
            ClientMethod::Unknown("tools/future-method".into()),
        );
    }

    // ── ClientNotifMethod::parse coverage ────────────────────

    #[test]
    fn client_notif_method__spec_coverage() {
        let cases: &[(&str, ClientNotifMethod)] = &[
            ("notifications/initialized", ClientNotifMethod::Initialized),
            ("notifications/cancelled", ClientNotifMethod::Cancelled),
            ("notifications/progress", ClientNotifMethod::Progress),
            (
                "notifications/roots/list_changed",
                ClientNotifMethod::RootsListChanged,
            ),
            ("notifications/tasks/status", ClientNotifMethod::TaskStatus),
        ];
        for (m, expected) in cases {
            assert_eq!(ClientNotifMethod::parse(m), *expected, "method = {m}");
        }
    }

    #[test]
    fn client_notif_method__unknown_preserves_string() {
        assert_eq!(
            ClientNotifMethod::parse("notifications/something"),
            ClientNotifMethod::Unknown("notifications/something".into()),
        );
    }

    // ── ServerMethod::parse coverage ─────────────────────────

    #[test]
    fn server_method__spec_coverage() {
        let cases: &[(&str, ServerMethod)] = &[
            ("ping", ServerMethod::Ping),
            ("sampling/createMessage", ServerMethod::Sampling),
            ("elicitation/create", ServerMethod::Elicitation),
            ("roots/list", ServerMethod::Roots),
            ("tasks/list", ServerMethod::Tasks(TasksMethod::List)),
            ("tasks/get", ServerMethod::Tasks(TasksMethod::Get)),
            ("tasks/result", ServerMethod::Tasks(TasksMethod::Result)),
            ("tasks/cancel", ServerMethod::Tasks(TasksMethod::Cancel)),
        ];
        for (m, expected) in cases {
            assert_eq!(ServerMethod::parse(m), *expected, "method = {m}");
        }
    }

    #[test]
    fn server_method__unknown_preserves_string() {
        assert_eq!(
            ServerMethod::parse("custom/method"),
            ServerMethod::Unknown("custom/method".into()),
        );
    }

    // ── ServerNotifMethod::parse coverage ────────────────────

    #[test]
    fn server_notif_method__spec_coverage() {
        let cases: &[(&str, ServerNotifMethod)] = &[
            ("notifications/cancelled", ServerNotifMethod::Cancelled),
            ("notifications/progress", ServerNotifMethod::Progress),
            ("notifications/message", ServerNotifMethod::LogMessage),
            (
                "notifications/resources/list_changed",
                ServerNotifMethod::ResourcesListChanged,
            ),
            (
                "notifications/resources/updated",
                ServerNotifMethod::ResourceUpdated,
            ),
            (
                "notifications/tools/list_changed",
                ServerNotifMethod::ToolsListChanged,
            ),
            (
                "notifications/prompts/list_changed",
                ServerNotifMethod::PromptsListChanged,
            ),
            (
                "notifications/elicitation/complete",
                ServerNotifMethod::ElicitationComplete,
            ),
            ("notifications/tasks/status", ServerNotifMethod::TaskStatus),
        ];
        for (m, expected) in cases {
            assert_eq!(ServerNotifMethod::parse(m), *expected, "method = {m}");
        }
    }

    #[test]
    fn server_notif_method__unknown_preserves_string() {
        assert_eq!(
            ServerNotifMethod::parse("notifications/future"),
            ServerNotifMethod::Unknown("notifications/future".into()),
        );
    }

    // ── classify_client ──────────────────────────────────────

    #[test]
    fn classify_client__request() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        assert_eq!(
            classify_client(&e),
            ClientKind::Request(ClientMethod::Tools(ToolsMethod::List)),
        );
    }

    #[test]
    fn classify_client__notification() {
        let e = parsed(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(
            classify_client(&e),
            ClientKind::Notification(ClientNotifMethod::Initialized),
        );
    }

    #[test]
    fn classify_client__result() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(classify_client(&e), ClientKind::Result);
    }

    #[test]
    fn classify_client__error() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#);
        assert_eq!(classify_client(&e), ClientKind::Error);
    }

    #[test]
    fn classify_client__unknown_method() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"method":"custom/method"}"#);
        assert_eq!(
            classify_client(&e),
            ClientKind::Request(ClientMethod::Unknown("custom/method".into())),
        );
    }

    // ── classify_server ──────────────────────────────────────

    #[test]
    fn classify_server__request() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"method":"sampling/createMessage"}"#);
        assert_eq!(
            classify_server(&e),
            ServerKind::Request(ServerMethod::Sampling),
        );
    }

    #[test]
    fn classify_server__notification() {
        let e = parsed(br#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);
        assert_eq!(
            classify_server(&e),
            ServerKind::Notification(ServerNotifMethod::ToolsListChanged),
        );
    }

    #[test]
    fn classify_server__result() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(classify_server(&e), ServerKind::Result);
    }

    #[test]
    fn classify_server__error() {
        let e = parsed(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#);
        assert_eq!(classify_server(&e), ServerKind::Error);
    }
}
