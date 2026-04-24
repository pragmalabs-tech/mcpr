//! Helpers shared across middlewares.
//!
//! `normalize_platform` lifts the client-name-to-platform mapping from
//! `pipeline/emit.rs` so `SessionRecordMiddleware` does not depend on a
//! module scheduled for deletion in Phase 5. `client_method_str` is the
//! inverse of `ClientMethod::parse` — middlewares that need the method
//! string (schema ingest, CSP rewrite) call it instead of round-tripping
//! through `serde_json`.

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
