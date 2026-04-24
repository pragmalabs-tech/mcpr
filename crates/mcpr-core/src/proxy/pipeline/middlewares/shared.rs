//! Helpers shared across middlewares.
//!
//! `normalize_platform` maps a client name (e.g. "claude-desktop") to a
//! platform identifier ("claude") for `SessionStart` events.

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
}
