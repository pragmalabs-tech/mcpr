//! Store configuration — TOML section and validation.
//!
//! Maps to the `[store]` section in `mcpr.toml`. Implements [`ModuleConfig`]
//! so the store owns its own defaults and validation logic.

use mcpr_core::config::{ConfigIssue, ModuleConfig, Severity};

/// `[store]` section in `mcpr.toml`.
///
/// Controls the SQLite-based request storage engine. All fields are optional —
/// sensible defaults are applied based on the platform.
///
/// ```toml
/// [store]
/// # Enable or disable request storage. Default: true.
/// # Set to false to run mcpr as a pure proxy with no local persistence.
/// enabled = true
///
/// # Override the database file path. Default: platform-specific.
/// # Linux:   ~/.local/share/mcpr/mcpr.db
/// # macOS:   ~/Library/Application Support/mcpr/mcpr.db
/// # Windows: %APPDATA%\mcpr\mcpr.db
/// path = "/var/lib/mcpr/requests.db"
///
/// # Proxy name tag written to every row. Default: derived from upstream URL.
/// # Use this when you run multiple proxies sharing one database file,
/// # or when you want a human-friendly name in `mcpr proxy logs <name>`.
/// name = "api-server"
/// ```
#[derive(serde::Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct FileStoreConfig {
    /// Whether request storage is enabled.
    ///
    /// When false, no database is opened and no events are recorded.
    /// CLI query commands (`mcpr proxy logs`, etc.) will report that
    /// storage is disabled.
    ///
    /// Default: `true` — storage is on by default because observability
    /// is the primary value proposition of mcpr beyond basic proxying.
    pub enabled: Option<bool>,

    /// Override the database file path.
    ///
    /// When set, the store uses this exact path instead of the platform
    /// default. Useful for:
    /// - Placing the DB on a specific disk or partition.
    /// - Docker/container deployments where the data dir is mounted.
    /// - Running integration tests with an isolated database.
    ///
    /// The parent directory is created automatically if it doesn't exist.
    /// Default: platform-specific (see [`super::path::resolve_db_path`]).
    pub path: Option<String>,

    /// Human-readable proxy name, written to every request and session row.
    ///
    /// This is how `mcpr proxy logs <name>` identifies which proxy's data
    /// to query. When multiple proxies share a database, each needs a
    /// unique name.
    ///
    /// Default: derived from the upstream MCP URL hostname (e.g.,
    /// "localhost-9000" for `http://localhost:9000/mcp`). Set this
    /// explicitly when the derived name isn't descriptive enough.
    pub name: Option<String>,
}

impl FileStoreConfig {
    /// Whether storage is enabled. Defaults to true.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl ModuleConfig for FileStoreConfig {
    fn name(&self) -> &'static str {
        "store"
    }

    fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        // Path must not be empty if explicitly set.
        if let Some(ref p) = self.path
            && p.trim().is_empty()
        {
            issues.push(ConfigIssue {
                severity: Severity::Error,
                module: "store",
                message: "store.path cannot be an empty string — remove the key to use the platform default, or set a valid path".into(),
            });
        }

        // Name must not be empty if explicitly set.
        if let Some(ref n) = self.name
            && n.trim().is_empty()
        {
            issues.push(ConfigIssue {
                severity: Severity::Error,
                module: "store",
                message: "store.name cannot be an empty string — remove the key to auto-derive from the upstream URL, or set a valid name".into(),
            });
        }

        issues
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn file_store_config__default_is_valid() {
        let config = FileStoreConfig::default();
        assert!(config.is_enabled());
        assert!(config.validate().is_empty());
    }

    #[test]
    fn file_store_config__disabled_is_valid() {
        let config = FileStoreConfig {
            enabled: Some(false),
            path: None,
            name: None,
        };
        assert!(!config.is_enabled());
        assert!(config.validate().is_empty());
    }

    #[test]
    fn file_store_config__empty_path_is_error() {
        let config = FileStoreConfig {
            enabled: None,
            path: Some("".into()),
            name: None,
        };
        let issues = config.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].message.contains("store.path"));
    }

    #[test]
    fn file_store_config__empty_name_is_error() {
        let config = FileStoreConfig {
            enabled: None,
            path: None,
            name: Some("  ".into()),
        };
        let issues = config.validate();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("store.name"));
    }

    #[test]
    fn file_store_config__parses_from_toml() {
        let toml_str = r#"
            enabled = false
            path = "/tmp/mcpr.db"
            name = "my-proxy"
        "#;
        let config: FileStoreConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.enabled, Some(false));
        assert_eq!(config.path.as_deref(), Some("/tmp/mcpr.db"));
        assert_eq!(config.name.as_deref(), Some("my-proxy"));
    }
}
