//! Configuration trait and validation types for mcpr modules.
//!
//! Each mcpr module (store, cloud, logging, etc.) implements [`ModuleConfig`]
//! on its TOML config section struct. This lets every module own its own defaults,
//! validation logic, and documentation — while the CLI orchestrates validation by
//! iterating over all registered modules.
//!
//! # Design rationale
//!
//! Without this trait, all config validation lives in one monolithic function in
//! `mcpr-cli/src/config.rs`, and every new module must touch that file. With it,
//! each crate validates itself — the CLI just loops over `&[&dyn ModuleConfig]`.

use std::fmt;

// ── Severity ───────────────────────────────────────────────────────────

/// How serious a configuration issue is.
///
/// - `Error`: the proxy cannot start with this config (e.g., invalid URL, missing required field).
/// - `Warn`: the proxy can start, but behavior may be surprising (e.g., port 0 binds randomly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warn => write!(f, "warn"),
        }
    }
}

// ── ConfigIssue ────────────────────────────────────────────────────────

/// A single validation issue found in a module's configuration.
///
/// Returned by [`ModuleConfig::validate`]. The CLI collects these from all
/// modules and presents them to the user (in `mcpr validate` or on startup).
#[derive(Debug, Clone)]
pub struct ConfigIssue {
    /// Whether this issue prevents startup or is just a warning.
    pub severity: Severity,

    /// Which module reported the issue (e.g., "store", "cloud").
    /// Matches the TOML section name so the user knows where to look.
    pub module: &'static str,

    /// Human-readable description of what's wrong and how to fix it.
    pub message: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.module, self.severity, self.message)
    }
}

// ── ModuleConfig trait ─────────────────────────────────────────────────

/// Trait for module-owned configuration sections.
///
/// Each mcpr module implements this on its file config struct (the struct
/// that maps to a `[section]` in `mcpr.toml`). This gives each module
/// ownership over:
///
/// - **Naming**: what TOML section key it lives under.
/// - **Defaults**: runtime-aware defaults (e.g., platform-specific paths).
/// - **Validation**: checking its own fields without the CLI knowing the rules.
///
/// # Example
///
/// ```rust,ignore
/// use mcpr_core::config::{ModuleConfig, ConfigIssue, Severity};
///
/// #[derive(serde::Deserialize, Default)]
/// pub struct FileStoreConfig {
///     pub path: Option<String>,
/// }
///
/// impl ModuleConfig for FileStoreConfig {
///     fn name(&self) -> &'static str { "store" }
///
///     fn validate(&self) -> Vec<ConfigIssue> {
///         let mut issues = vec![];
///         if let Some(ref p) = self.path {
///             if p.is_empty() {
///                 issues.push(ConfigIssue {
///                     severity: Severity::Error,
///                     module: "store",
///                     message: "store.path cannot be empty string".into(),
///                 });
///             }
///         }
///         issues
///     }
/// }
/// ```
///
/// # CLI integration
///
/// The CLI collects all `&dyn ModuleConfig` and calls `validate()` on each:
///
/// ```rust,ignore
/// let modules: Vec<&dyn ModuleConfig> = vec![&config.store, &config.cloud, ...];
/// let issues: Vec<ConfigIssue> = modules.iter().flat_map(|m| m.validate()).collect();
/// ```
pub trait ModuleConfig {
    /// Module name — must match the TOML section key (e.g., "store", "cloud").
    ///
    /// Used in error messages and logging so the user knows which section
    /// of `mcpr.toml` to fix.
    fn name(&self) -> &'static str;

    /// Validate this module's configuration.
    ///
    /// Returns an empty vec if everything is valid. Each issue carries its own
    /// severity — the CLI decides whether to abort (on Error) or continue (on Warn).
    ///
    /// Implementations should validate field values, required combinations,
    /// and cross-field constraints within this module. Cross-module validation
    /// (e.g., "relay mode requires port") stays in the CLI.
    fn validate(&self) -> Vec<ConfigIssue>;

    /// Apply runtime-aware defaults that can't be expressed in `Default::default()`.
    ///
    /// Called after TOML deserialization, before validation. Use this for defaults
    /// that depend on the platform, environment variables, or other runtime state
    /// (e.g., platform-specific DB paths, env-based feature flags).
    ///
    /// The default implementation is a no-op.
    fn apply_defaults(&mut self) {}
}
