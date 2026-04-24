//! Helpers shared across middlewares.
//!
//! `normalize_platform` lifts the client-name-to-platform mapping from
//! `pipeline/emit.rs` so `SessionRecordMiddleware` does not depend on a
//! module scheduled for deletion in Phase 5. `client_method_str` is the
//! inverse of `ClientMethod::parse` — middlewares that need the method
//! string (schema ingest, CSP rewrite) call it instead of round-tripping
//! through `serde_json`.

use serde_json::{Map, Value};

use super::super::envelope::{JsonRpcEnvelope, JsonRpcId};
use super::super::message::{
    ClientMethod, CompletionMethod, LifecycleMethod, LoggingMethod, PromptsMethod, ResourcesMethod,
    TasksMethod, ToolsMethod,
};

pub(crate) fn normalize_platform(client_name: &str) -> &'static str {
    let lower = client_name.to_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("cursor") {
        "cursor"
    } else if lower.contains("chatgpt") || lower.contains("openai") {
        "chatgpt"
    } else if lower.contains("copilot") || lower.contains("vscode") || lower.contains("vs-code") {
        "vscode"
    } else if lower.contains("windsurf") {
        "windsurf"
    } else {
        "unknown"
    }
}

/// Reassemble the JSON-RPC message from the shallow envelope. Used by
/// `EnvelopeSealMiddleware` on the way out and by `ProxyTransport` to
/// re-serialize the request body before forwarding upstream. Parse
/// failures on the cached `RawValue` bytes would have been caught at
/// intake — we default to `Null` rather than panic.
pub(crate) fn serialize_envelope(env: &JsonRpcEnvelope) -> Vec<u8> {
    let mut map = Map::with_capacity(5);
    map.insert("jsonrpc".into(), Value::String("2.0".into()));
    if let Some(id) = &env.id {
        map.insert("id".into(), id_to_value(id));
    }
    if let Some(method) = &env.method {
        map.insert("method".into(), Value::String(method.clone()));
    }
    if let Some(params) = &env.params {
        map.insert(
            "params".into(),
            serde_json::from_str(params.get()).unwrap_or(Value::Null),
        );
    }
    if let Some(result) = &env.result {
        map.insert(
            "result".into(),
            serde_json::from_str(result.get()).unwrap_or(Value::Null),
        );
    }
    if let Some(error) = &env.error {
        let mut err = Map::with_capacity(3);
        err.insert("code".into(), Value::Number((error.code as i64).into()));
        err.insert("message".into(), Value::String(error.message.clone()));
        if let Some(data) = &error.data {
            err.insert(
                "data".into(),
                serde_json::from_str(data.get()).unwrap_or(Value::Null),
            );
        }
        map.insert("error".into(), Value::Object(err));
    }
    serde_json::to_vec(&Value::Object(map)).unwrap_or_default()
}

fn id_to_value(id: &JsonRpcId) -> Value {
    match id {
        JsonRpcId::Number(n) => Value::Number((*n).into()),
        JsonRpcId::String(s) => Value::String(s.clone()),
        JsonRpcId::Null => Value::Null,
    }
}

pub(crate) fn client_method_str(m: &ClientMethod) -> Option<&'static str> {
    Some(match m {
        ClientMethod::Ping => "ping",
        ClientMethod::Lifecycle(LifecycleMethod::Initialize) => "initialize",
        ClientMethod::Tools(ToolsMethod::List) => "tools/list",
        ClientMethod::Tools(ToolsMethod::Call) => "tools/call",
        ClientMethod::Resources(ResourcesMethod::List) => "resources/list",
        ClientMethod::Resources(ResourcesMethod::TemplatesList) => "resources/templates/list",
        ClientMethod::Resources(ResourcesMethod::Read) => "resources/read",
        ClientMethod::Resources(ResourcesMethod::Subscribe) => "resources/subscribe",
        ClientMethod::Resources(ResourcesMethod::Unsubscribe) => "resources/unsubscribe",
        ClientMethod::Prompts(PromptsMethod::List) => "prompts/list",
        ClientMethod::Prompts(PromptsMethod::Get) => "prompts/get",
        ClientMethod::Completion(CompletionMethod::Complete) => "completion/complete",
        ClientMethod::Logging(LoggingMethod::SetLevel) => "logging/setLevel",
        ClientMethod::Tasks(TasksMethod::List) => "tasks/list",
        ClientMethod::Tasks(TasksMethod::Get) => "tasks/get",
        ClientMethod::Tasks(TasksMethod::Result) => "tasks/result",
        ClientMethod::Tasks(TasksMethod::Cancel) => "tasks/cancel",
        ClientMethod::Unknown(_) => return None,
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn normalize_platform__claude_variants() {
        assert_eq!(normalize_platform("claude-desktop"), "claude");
        assert_eq!(normalize_platform("Claude Code"), "claude");
    }

    #[test]
    fn normalize_platform__cursor() {
        assert_eq!(normalize_platform("cursor"), "cursor");
    }

    #[test]
    fn normalize_platform__chatgpt_and_openai() {
        assert_eq!(normalize_platform("chatgpt"), "chatgpt");
        assert_eq!(normalize_platform("openai-client"), "chatgpt");
    }

    #[test]
    fn normalize_platform__vscode_family() {
        assert_eq!(normalize_platform("github-copilot"), "vscode");
        assert_eq!(normalize_platform("vscode"), "vscode");
        assert_eq!(normalize_platform("vs-code"), "vscode");
    }

    #[test]
    fn normalize_platform__unknown() {
        assert_eq!(normalize_platform("whatever-agent"), "unknown");
        assert_eq!(normalize_platform(""), "unknown");
    }

    #[test]
    fn client_method_str__roundtrip_for_all_spec_methods() {
        let all = [
            ClientMethod::Ping,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            ClientMethod::Tools(ToolsMethod::List),
            ClientMethod::Tools(ToolsMethod::Call),
            ClientMethod::Resources(ResourcesMethod::List),
            ClientMethod::Resources(ResourcesMethod::TemplatesList),
            ClientMethod::Resources(ResourcesMethod::Read),
            ClientMethod::Resources(ResourcesMethod::Subscribe),
            ClientMethod::Resources(ResourcesMethod::Unsubscribe),
            ClientMethod::Prompts(PromptsMethod::List),
            ClientMethod::Prompts(PromptsMethod::Get),
            ClientMethod::Completion(CompletionMethod::Complete),
            ClientMethod::Logging(LoggingMethod::SetLevel),
            ClientMethod::Tasks(TasksMethod::List),
            ClientMethod::Tasks(TasksMethod::Get),
            ClientMethod::Tasks(TasksMethod::Result),
            ClientMethod::Tasks(TasksMethod::Cancel),
        ];
        for m in &all {
            let s = client_method_str(m).expect("spec method must have a string");
            assert_eq!(ClientMethod::parse(s), *m, "roundtrip for {s}");
        }
    }

    #[test]
    fn client_method_str__unknown_returns_none() {
        assert!(client_method_str(&ClientMethod::Unknown("foo/bar".into())).is_none());
    }
}
