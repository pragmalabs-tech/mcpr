//! Database file path resolution.
//!
//! The storage engine needs to know where to put the SQLite file. This module
//! resolves the path from three sources, in priority order:
//!
//! 1. **Explicit config**: `[store] path` in `mcpr.toml` — user chose a specific location.
//! 2. **Environment variable**: `MCPR_DB` — useful for CI, Docker, or per-session overrides.
//! 3. **Default**: `~/.mcpr/store.db` — all mcpr state lives under `~/.mcpr/`.
//!
//! The parent directory is created automatically if it doesn't exist.

use std::path::PathBuf;

/// Environment variable name for overriding the database path.
const MCPR_DB_ENV: &str = "MCPR_DB";

/// Database filename within the mcpr data directory.
const DB_FILENAME: &str = "store.db";

/// Resolve the database file path.
///
/// Priority: `config_path` > `$MCPR_DB` env > platform default.
///
/// Returns `None` only if no platform data directory can be determined
/// (extremely rare — means `$HOME` is unset on a headless system).
pub fn resolve_db_path(config_path: Option<&str>) -> Option<PathBuf> {
    // 1. Explicit config takes priority — the user made a deliberate choice.
    if let Some(p) = config_path {
        return Some(PathBuf::from(p));
    }

    // 2. Environment variable — useful for CI, Docker, or scripting.
    if let Ok(p) = std::env::var(MCPR_DB_ENV)
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }

    // 3. Default — all mcpr state under ~/.mcpr/
    dirs::home_dir().map(|h| h.join(".mcpr").join(DB_FILENAME))
}

/// Ensure the parent directory of the given path exists.
///
/// Called before opening the database. Creates intermediate directories
/// as needed. Returns an error if directory creation fails (e.g., permissions).
pub fn ensure_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn resolve_db_path__explicit_config_wins() {
        let result = resolve_db_path(Some("/custom/path/my.db"));
        assert_eq!(result, Some(PathBuf::from("/custom/path/my.db")));
    }

    #[test]
    fn resolve_db_path__platform_default_returns_some() {
        let result = resolve_db_path(None);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path.to_str().unwrap().contains(".mcpr"));
        assert!(path.to_str().unwrap().ends_with("store.db"));
    }

    #[test]
    fn ensure_parent_dir__creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sub").join("dir").join("mcpr.db");
        assert!(!db_path.parent().unwrap().exists());

        ensure_parent_dir(&db_path).unwrap();
        assert!(db_path.parent().unwrap().exists());
    }
}
