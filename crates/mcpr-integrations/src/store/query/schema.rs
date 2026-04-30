//! Query: `mcpr proxy schema` — show captured MCP server schema and change history.

use rusqlite::params;
use serde::Serialize;

use super::QueryEngine;

/// Parameters for the schema snapshot query.
pub struct SchemaParams {
    /// Filter to a specific proxy name. `None` returns all proxies.
    pub proxy: Option<String>,
    pub method: Option<String>,
}

/// Parameters for the schema changes query.
pub struct SchemaChangesParams {
    /// Filter to a specific proxy name. `None` returns all proxies.
    pub proxy: Option<String>,
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

/// A hydration-focused schema row. Adds `proxy` which the general
/// `SchemaRow` omits (it's not useful for the status-display consumers
/// that call `schema()`). Used at startup to seed `SchemaManager`.
#[derive(Debug, Clone)]
pub struct LatestSchemaRow {
    pub proxy: String,
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

/// Parameters for the unused tools query.
pub struct SchemaUnusedParams {
    pub proxy: Option<String>,
    pub since_ts: i64,
}

/// A tool listed in the schema with its usage stats.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaToolUsageRow {
    pub tool_name: String,
    pub description: String,
    pub calls: i64,
    pub errors: i64,
    pub last_called_at: Option<i64>,
}

/// Computed schema status for a given upstream.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatusRow {
    pub upstream_url: String,
    /// "unknown", "partial", or "complete".
    pub status: String,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub protocol_version: Option<String>,
    pub capabilities: Vec<String>,
    pub methods_captured: Vec<String>,
    pub last_captured_at: Option<i64>,
}

impl QueryEngine {
    /// List per-item schema snapshots. The new model stores one row per
    /// item (tool/prompt/resource/resource_template) rather than one row
    /// per (upstream, method) payload, so:
    ///   `upstream_url` is empty (we no longer track upstream URLs at the
    ///                            schema layer)
    ///   `method`       is the item kind ("tool" | "prompt" | …)
    ///   `payload`      is the per-item JSON
    ///   `schema_hash`  is `payload_hash`
    ///
    /// TODO: redesign the public CLI shape around items instead of methods.
    pub fn schema(&self, params: &SchemaParams) -> Result<Vec<SchemaRow>, rusqlite::Error> {
        let sql = "
            SELECT '' AS upstream_url, kind AS method, payload, captured_at, payload_hash
            FROM schema_items
            WHERE (?1 IS NULL OR proxy = ?1)
              AND (?2 IS NULL OR kind = ?2)
            ORDER BY kind, item_key
        ";
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(params![params.proxy, params.method], |row| {
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

    /// Fetch the per-item schema change log. The legacy fields map onto
    /// the new schema as:
    ///   `upstream_url` — empty (unavailable)
    ///   `method`       — item kind
    ///   `change_type`  — reason ("added" | "observed")
    ///   `item_name`    — item_key
    pub fn schema_changes(
        &self,
        params: &SchemaChangesParams,
    ) -> Result<Vec<SchemaChangeRow>, rusqlite::Error> {
        let sql = "
            SELECT '' AS upstream_url,
                   kind AS method,
                   reason AS change_type,
                   item_key AS item_name,
                   old_hash,
                   new_hash,
                   detected_at
            FROM schema_changes
            WHERE (?1 IS NULL OR proxy = ?1)
              AND (?2 IS NULL OR kind = ?2)
            ORDER BY detected_at DESC
            LIMIT ?3
        ";
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(params![params.proxy, params.method, params.limit], |row| {
            Ok(SchemaChangeRow {
                upstream_url: row.get(0)?,
                method: row.get(1)?,
                change_type: row.get(2)?,
                item_name: row.get(3)?,
                old_hash: row.get(4)?,
                new_hash: Some(row.get::<_, String>(5)?),
                detected_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Schema status used to be derived from a captured `initialize`
    /// payload. The new event model drops `initialize` capture entirely
    /// — schema_items only holds tools/prompts/resources/templates.
    /// Return "unknown" with a list of kinds we have items for.
    ///
    /// TODO: reintroduce a `SchemaSnapshot` event for `initialize` so we
    /// can populate serverInfo / protocolVersion / capabilities again.
    pub fn schema_status(&self, upstream_url: &str) -> Result<SchemaStatusRow, rusqlite::Error> {
        let kinds_sql = "
            SELECT DISTINCT kind, MAX(captured_at) AS last
            FROM schema_items
            GROUP BY kind
            ORDER BY kind
        ";
        let mut stmt = self.conn().prepare(kinds_sql)?;
        let kinds: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let last_captured = kinds.iter().map(|(_, ts)| *ts).max();
        let methods_captured: Vec<String> = kinds.into_iter().map(|(k, _)| k).collect();

        Ok(SchemaStatusRow {
            upstream_url: upstream_url.to_string(),
            status: "unknown".to_string(),
            server_name: None,
            server_version: None,
            protocol_version: None,
            capabilities: vec![],
            methods_captured,
            last_captured_at: last_captured,
        })
    }

    /// Used by SchemaManager hydration at startup. The new model has no
    /// payload-per-method shape, so there's nothing useful to return.
    /// SchemaManager rebuilds from the next observed list response.
    ///
    /// TODO: replace with a per-kind hydrator once SchemaManager learns
    /// to consume `schema_items` rows directly.
    pub fn latest_schema_row(
        &self,
        _proxy: &str,
        _method: &str,
    ) -> Result<Option<LatestSchemaRow>, rusqlite::Error> {
        Ok(None)
    }

    /// Cross-reference recorded tools with actual request logs to surface
    /// tools that were declared but never called.
    pub fn schema_unused(
        &self,
        params: &SchemaUnusedParams,
    ) -> Result<Vec<SchemaToolUsageRow>, rusqlite::Error> {
        let items_sql = "
            SELECT item_key, payload FROM schema_items
            WHERE kind = 'tool'
              AND (?1 IS NULL OR proxy = ?1)
        ";
        let mut stmt = self.conn().prepare(items_sql)?;
        let items: Vec<(String, String)> = stmt
            .query_map(params![params.proxy], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        if items.is_empty() {
            return Ok(vec![]);
        }

        let usage_sql = "
            SELECT COUNT(*) AS calls,
                   SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) AS errors,
                   MAX(ts) AS last_called_at
            FROM request_log
            WHERE (?1 IS NULL OR proxy = ?1) AND ts >= ?2 AND tool = ?3
        ";

        let mut result = Vec::with_capacity(items.len());
        for (name, payload) in &items {
            let description = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|v| {
                    v.get("description")
                        .and_then(|d| d.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();

            let row = self.conn().query_row(
                usage_sql,
                params![params.proxy, params.since_ts, name],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            );
            let (calls, errors, last_called_at) = row.unwrap_or((0, 0, None));
            result.push(SchemaToolUsageRow {
                tool_name: name.clone(),
                description,
                calls,
                errors,
                last_called_at,
            });
        }

        result.sort_by(|a, b| a.calls.cmp(&b.calls));
        Ok(result)
    }
}
