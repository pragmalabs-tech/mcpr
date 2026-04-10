//! Database file path resolution.
//!
//! The storage engine needs to know where to put the SQLite file. This module
//! resolves the path from three sources, in priority order:
//!
//! 1. **Explicit config**: `[store] path` in `mcpr.toml` — user chose a specific location.
//! 2. **Environment variable**: `MCPR_DB` — useful for CI, Docker, or per-session overrides.
//! 3. **Platform default**: follows OS conventions so the file lives where users expect:
//!    - Linux: `$XDG_DATA_HOME/mcpr/mcpr.db` → fallback `~/.local/share/mcpr/mcpr.db`
//!    - macOS: `~/Library/Application Support/mcpr/mcpr.db`
//!    - Windows: `%APPDATA%\mcpr\mcpr.db`
//!
//! The parent directory is created automatically if it doesn't exist.

use std::path::PathBuf;

/// Environment variable name for overriding the database path.
const MCPR_DB_ENV: &str = "MCPR_DB";

/// Database filename within the mcpr data directory.
const DB_FILENAME: &str = "mcpr.db";

/// Application directory name used under the platform data dir.
const APP_DIR: &str = "mcpr";

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

    // 3. Platform default — follows OS conventions via the `dirs` crate.
    //    dirs::data_local_dir() returns:
    //      Linux:   $XDG_DATA_HOME or ~/.local/share
    //      macOS:   ~/Library/Application Support
    //      Windows: %LOCALAPPDATA% (e.g., C:\Users\X\AppData\Local)
    dirs::data_local_dir().map(|d| d.join(APP_DIR).join(DB_FILENAME))
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
mod tests {
    use super::*;

    #[test]
    fn explicit_config_path_wins() {
        let result = resolve_db_path(Some("/custom/path/my.db"));
        assert_eq!(result, Some(PathBuf::from("/custom/path/my.db")));
    }

    #[test]
    fn platform_default_returns_some() {
        // Should return Some on any system with $HOME set.
        let result = resolve_db_path(None);
        assert!(
            result.is_some(),
            "platform default should resolve on dev machines"
        );
        let path = result.unwrap();
        assert!(path.to_str().unwrap().contains("mcpr"));
        assert!(path.to_str().unwrap().ends_with("mcpr.db"));
    }

    #[test]
    fn ensure_parent_dir_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sub").join("dir").join("mcpr.db");
        assert!(!db_path.parent().unwrap().exists());

        ensure_parent_dir(&db_path).unwrap();
        assert!(db_path.parent().unwrap().exists());
    }
}
