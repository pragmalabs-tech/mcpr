//! Query: `mcpr proxy schema` — show captured MCP server schema and change history.

use rusqlite::params;
use serde::Serialize;

use super::QueryEngine;

/// Parameters for the schema snapshot query.
pub struct SchemaParams {
    pub upstream_url: Option<String>,
    pub method: Option<String>,
}

/// Parameters for the schema changes query.
pub struct SchemaChangesParams {
    pub upstream_url: Option<String>,
    pub method: Option<String>,
    pub limit: i64,
}

/// A single schema snapshot row from `server_schema`.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaRow {
    pub upstream_url: String,
    pub method: String,
    pub payload: String,
    pub captured_at: i64,
    pub schema_hash: String,
}

/// A schema change record from `schema_changes`.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaChangeRow {
    pub upstream_url: String,
    pub method: String,
    pub change_type: String,
    pub item_name: Option<String>,
    pub old_hash: Option<String>,
    pub new_hash: Option<String>,
    pub detected_at: i64,
}

/// Computed schema status for a given upstream.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatusRow {
    pub upstream_url: String,
    /// "unknown", "partial", "complete", "stale"
    pub status: String,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub protocol_version: Option<String>,
    pub capabilities: Vec<String>,
    pub methods_captured: Vec<String>,
    pub last_captured_at: Option<i64>,
}

impl QueryEngine {
    /// Fetch all captured schema snapshots, optionally filtered.
    pub fn schema(&self, params: &SchemaParams) -> Result<Vec<SchemaRow>, rusqlite::Error> {
        let sql = "
            SELECT upstream_url, method, payload, captured_at, schema_hash
            FROM server_schema
            WHERE (?1 IS NULL OR upstream_url = ?1)
              AND (?2 IS NULL OR method = ?2)
            ORDER BY upstream_url, method
        ";
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(params![params.upstream_url, params.method], |row| {
            Ok(SchemaRow {
                upstream_url: row.get(0)?,
                method: row.get(1)?,
                payload: row.get(2)?,
                captured_at: row.get(3)?,
                schema_hash: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Fetch schema change history.
    pub fn schema_changes(
        &self,
        params: &SchemaChangesParams,
    ) -> Result<Vec<SchemaChangeRow>, rusqlite::Error> {
        let sql = "
            SELECT upstream_url, method, change_type, item_name, old_hash, new_hash, detected_at
            FROM schema_changes
            WHERE (?1 IS NULL OR upstream_url = ?1)
              AND (?2 IS NULL OR method = ?2)
            ORDER BY detected_at DESC
            LIMIT ?3
        ";
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(
            params![params.upstream_url, params.method, params.limit],
            |row| {
                Ok(SchemaChangeRow {
                    upstream_url: row.get(0)?,
                    method: row.get(1)?,
                    change_type: row.get(2)?,
                    item_name: row.get(3)?,
                    old_hash: row.get(4)?,
                    new_hash: row.get(5)?,
                    detected_at: row.get(6)?,
                })
            },
        )?;
        rows.collect()
    }

    /// Compute the schema status for a given upstream URL.
    pub fn schema_status(&self, upstream_url: &str) -> Result<SchemaStatusRow, rusqlite::Error> {
        let methods_sql = "
            SELECT method, captured_at FROM server_schema
            WHERE upstream_url = ?1
            ORDER BY method
        ";
        let mut stmt = self.conn().prepare(methods_sql)?;
        let methods: Vec<(String, i64)> = stmt
            .query_map(params![upstream_url], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        if methods.is_empty() {
            return Ok(SchemaStatusRow {
                upstream_url: upstream_url.to_string(),
                status: "unknown".to_string(),
                server_name: None,
                server_version: None,
                protocol_version: None,
                capabilities: vec![],
                methods_captured: vec![],
                last_captured_at: None,
            });
        }

        let method_names: Vec<String> = methods.iter().map(|(m, _)| m.clone()).collect();
        let last_captured = methods.iter().map(|(_, ts)| *ts).max();

        // Extract server info from initialize payload if available.
        let (server_name, server_version, protocol_version, capabilities) =
            self.extract_server_info(upstream_url);

        // Check for stale markers newer than the latest capture for that method.
        let stale_sql = "
            SELECT COUNT(*) FROM schema_changes sc
            WHERE sc.upstream_url = ?1
              AND sc.change_type = 'stale'
              AND sc.detected_at > COALESCE(
                  (SELECT ss.captured_at FROM server_schema ss
                   WHERE ss.upstream_url = sc.upstream_url AND ss.method = sc.method),
                  0
              )
        ";
        let stale_count: i64 = self
            .conn()
            .query_row(stale_sql, params![upstream_url], |row| row.get(0))?;
        let is_stale = stale_count > 0;

        let has_initialize = method_names.iter().any(|m| m == "initialize");
        let list_methods = [
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "prompts/list",
        ];
        let has_any_list = list_methods
            .iter()
            .any(|m| method_names.iter().any(|n| n == m));

        let status = if is_stale {
            "stale"
        } else if has_initialize && has_any_list {
            "complete"
        } else {
            "partial"
        };

        Ok(SchemaStatusRow {
            upstream_url: upstream_url.to_string(),
            status: status.to_string(),
            server_name,
            server_version,
            protocol_version,
            capabilities,
            methods_captured: method_names,
            last_captured_at: last_captured,
        })
    }

    /// Extract server info from a captured initialize payload.
    fn extract_server_info(
        &self,
        upstream_url: &str,
    ) -> (Option<String>, Option<String>, Option<String>, Vec<String>) {
        let payload: Option<String> = self
            .conn()
            .query_row(
                "SELECT payload FROM server_schema WHERE upstream_url = ?1 AND method = 'initialize'",
                params![upstream_url],
                |row| row.get(0),
            )
            .ok();

        let payload = match payload {
            Some(p) => p,
            None => return (None, None, None, vec![]),
        };

        let val: serde_json::Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(_) => return (None, None, None, vec![]),
        };

        let server_name = val
            .get("serverInfo")
            .and_then(|i| i.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from);
        let server_version = val
            .get("serverInfo")
            .and_then(|i| i.get("version"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let protocol_version = val
            .get("protocolVersion")
            .and_then(|p| p.as_str())
            .map(String::from);
        let capabilities = val
            .get("capabilities")
            .and_then(|c| c.as_object())
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();

        (server_name, server_version, protocol_version, capabilities)
    }
}
