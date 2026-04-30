//! SQL schema definitions for the mcpr storage engine.
//!
//! All `CREATE TABLE`, `CREATE INDEX`, view, and prepared-statement strings
//! live here. [`super::db::init_schema`] runs `CREATE_ALL_SQL` on first open;
//! `IF NOT EXISTS` guards make it idempotent for subsequent starts.
//!
//! # Design
//!
//! - `requests` and `responses` are append-only and joined on
//!   `(session_id, request_id)`. The `request_log` view performs the join
//!   and exposes the legacy "completed request" shape (latency, status,
//!   bytes_*) used by the CLI query layer.
//! - `sessions` is upserted from `SessionInfo` snapshots — the writer never
//!   computes counters; `request_count` and `last_active` come from the
//!   in-memory `SessionStore` in `mcpr-core`.
//! - `schema_items` holds the latest seen item per
//!   `(proxy, kind, item_key)`; `schema_changes` is an append-only log.
//!   No overall-schema versioning at this stage — see `event.rs` notes.

/// Storage schema version. Bumped only on breaking layout changes.
pub const SCHEMA_VERSION: &str = "1";

/// One-shot schema bootstrap. Idempotent via `IF NOT EXISTS`.
pub const CREATE_ALL_SQL: &str = r#"
-- ── sessions ──────────────────────────────────────────────────────────
-- One row per MCP session. UPSERT'd on every Session event.
-- Counters and last_active mirror SessionInfo, which is the authority.
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    proxy           TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('active','closed')),
    client_name     TEXT,
    client_version  TEXT,
    created_at      INTEGER NOT NULL,
    last_active     INTEGER NOT NULL,
    request_count   INTEGER NOT NULL
);

-- ── requests ──────────────────────────────────────────────────────────
-- Append-only inbound MCP requests. McpBatch unfolds to one row per rpc;
-- raw HTTP traffic is dropped (no jsonrpc id, no method).
CREATE TABLE IF NOT EXISTS requests (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              INTEGER NOT NULL,
    proxy           TEXT NOT NULL,
    session_id      TEXT,
    request_id      TEXT NOT NULL,
    method          TEXT NOT NULL,
    tool            TEXT,
    resource_uri    TEXT,
    prompt_name     TEXT,
    bytes_in        INTEGER,
    UNIQUE (session_id, request_id)
);

-- ── responses ─────────────────────────────────────────────────────────
-- Append-only upstream responses. Joined to `requests` on
-- (session_id, request_id) by the `request_log` view to recover latency
-- and outcome.
CREATE TABLE IF NOT EXISTS responses (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              INTEGER NOT NULL,
    session_id      TEXT,
    request_id      TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('ok','error')),
    error_code      INTEGER,
    error_msg       TEXT,
    bytes_out       INTEGER,
    UNIQUE (session_id, request_id)
);

-- ── schema_items ──────────────────────────────────────────────────────
-- Latest observed payload per (proxy, kind, item_key). Upserted on every
-- Schema event; payload_hash is sha256(kind \0 canonical_json).
CREATE TABLE IF NOT EXISTS schema_items (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    proxy           TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('tool','prompt','resource','resource_template')),
    item_key        TEXT NOT NULL,
    payload         TEXT NOT NULL,
    payload_hash    TEXT NOT NULL,
    captured_at     INTEGER NOT NULL,
    UNIQUE (proxy, kind, item_key)
);

-- ── schema_changes ────────────────────────────────────────────────────
-- Append-only log of every Schema event whose payload_hash differs from
-- the prior snapshot for the same (proxy, kind, item_key). First seen
-- has old_hash = NULL.
CREATE TABLE IF NOT EXISTS schema_changes (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    proxy           TEXT NOT NULL,
    kind            TEXT NOT NULL,
    item_key        TEXT NOT NULL,
    reason          TEXT NOT NULL CHECK (reason IN ('added','observed')),
    old_hash        TEXT,
    new_hash        TEXT NOT NULL,
    detected_at     INTEGER NOT NULL
);

-- ── meta ──────────────────────────────────────────────────────────────
-- Key-value config for schema versioning and binary version tracking.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ── indexes ───────────────────────────────────────────────────────────
CREATE INDEX IF NOT EXISTS idx_requests_proxy_ts   ON requests (proxy, ts);
CREATE INDEX IF NOT EXISTS idx_requests_session    ON requests (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_requests_tool       ON requests (tool, ts) WHERE tool IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_requests_method     ON requests (method, ts);

CREATE INDEX IF NOT EXISTS idx_responses_join      ON responses (session_id, request_id);
CREATE INDEX IF NOT EXISTS idx_responses_status    ON responses (status, ts);

CREATE INDEX IF NOT EXISTS idx_sessions_proxy      ON sessions (proxy, last_active);
CREATE INDEX IF NOT EXISTS idx_sessions_active     ON sessions (proxy, last_active) WHERE state = 'active';

CREATE INDEX IF NOT EXISTS idx_schema_items_proxy  ON schema_items (proxy, kind);
CREATE INDEX IF NOT EXISTS idx_schema_changes_proxy ON schema_changes (proxy, detected_at);

-- ── request_log view ─────────────────────────────────────────────────
-- Recovers the legacy "completed request" shape by joining requests with
-- responses on (session_id, request_id). The `latency_us` column is
-- ms-precision under the hood (proxy stamps timestamps in ms) but is
-- multiplied by 1000 to keep the historical column unit. `error_code`
-- is cast to TEXT so existing CLI/render code that treats it as a
-- string keeps compiling.
CREATE VIEW IF NOT EXISTS request_log AS
SELECT
    r.id,
    r.ts,
    r.proxy,
    r.session_id,
    r.request_id,
    r.method,
    r.tool,
    r.resource_uri,
    r.prompt_name,
    r.bytes_in,
    (res.ts - r.ts) * 1000                AS latency_us,
    COALESCE(res.status, 'pending')       AS status,
    CAST(res.error_code AS TEXT)          AS error_code,
    res.error_msg,
    res.bytes_out
FROM requests r
LEFT JOIN responses res
    ON r.session_id = res.session_id
   AND r.request_id = res.request_id;

-- ── sessions_view ────────────────────────────────────────────────────
-- Backwards-compatible projection of the new `sessions` table to the
-- legacy column names used by the CLI render layer:
--   id            → session_id
--   created_at    → started_at
--   last_active   → last_seen_at
--   request_count → total_calls
-- Plus two derived columns:
--   ended_at        — the last_active stamp at the time `state` flipped
--                     to 'closed'; NULL while session is active.
--   client_platform — heuristic normalization from `client_name`.
--   total_errors    — count of error rows in `responses` for this session.
CREATE VIEW IF NOT EXISTS sessions_view AS
SELECT
    s.id            AS session_id,
    s.proxy,
    s.client_name,
    s.client_version,
    CASE
        WHEN s.client_name IS NULL                         THEN NULL
        WHEN s.client_name LIKE 'claude%'                  THEN 'claude'
        WHEN s.client_name LIKE 'cursor%'                  THEN 'cursor'
        WHEN s.client_name LIKE 'chatgpt%'                 THEN 'chatgpt'
        WHEN s.client_name LIKE 'vscode%'                  THEN 'vscode'
        WHEN s.client_name LIKE 'visual-studio-code%'      THEN 'vscode'
        ELSE 'unknown'
    END                                                 AS client_platform,
    s.created_at    AS started_at,
    s.last_active   AS last_seen_at,
    CASE WHEN s.state = 'closed' THEN s.last_active ELSE NULL END AS ended_at,
    s.request_count AS total_calls,
    (SELECT COUNT(*) FROM responses res
     WHERE res.session_id = s.id AND res.status = 'error') AS total_errors,
    s.state
FROM sessions s;
"#;

/// Seed the schema_version row on first creation. Idempotent.
pub const META_SEED_SQL: &str = "
INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');
INSERT OR IGNORE INTO meta (key, value) VALUES ('created_at', CAST(strftime('%s', 'now') AS TEXT) || '000');
";

/// UPSERT the running mcpr binary version on every startup.
pub const UPSERT_MCPR_VERSION_SQL: &str = "
INSERT INTO meta (key, value) VALUES ('mcpr_version', ?1)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value;
";

// ── Prepared-statement SQL ────────────────────────────────────────────
// All bind values are passed as `&str` / `i64` / `Option<…>` from the writer;
// no row-shape struct stands between the event and the executed statement.

/// INSERT a request row. ?1=ts, ?2=proxy, ?3=session_id, ?4=request_id,
/// ?5=method, ?6=tool, ?7=resource_uri, ?8=prompt_name, ?9=bytes_in.
/// `INSERT OR IGNORE` so a duplicate (session_id, request_id) is dropped
/// silently rather than aborting the batch transaction.
pub const INSERT_REQUEST_SQL: &str = "
INSERT OR IGNORE INTO requests (
    ts, proxy, session_id, request_id,
    method, tool, resource_uri, prompt_name,
    bytes_in
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9);
";

/// INSERT a response row. ?1=ts, ?2=session_id, ?3=request_id,
/// ?4=status, ?5=error_code, ?6=error_msg, ?7=bytes_out.
pub const INSERT_RESPONSE_SQL: &str = "
INSERT OR IGNORE INTO responses (
    ts, session_id, request_id,
    status, error_code, error_msg, bytes_out
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);
";

/// UPSERT a session. ?1=id, ?2=proxy, ?3=state, ?4=client_name,
/// ?5=client_version, ?6=created_at, ?7=last_active, ?8=request_count.
/// On conflict, preserves the original `created_at` and updates everything
/// else (state, last_active, counters, late client_info if it arrived later).
pub const UPSERT_SESSION_SQL: &str = "
INSERT INTO sessions (
    id, proxy, state, client_name, client_version,
    created_at, last_active, request_count
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(id) DO UPDATE SET
    state          = excluded.state,
    client_name    = COALESCE(excluded.client_name, sessions.client_name),
    client_version = COALESCE(excluded.client_version, sessions.client_version),
    last_active    = excluded.last_active,
    request_count  = excluded.request_count;
";

/// Fetch the current `payload_hash` for an item. Used by the writer to
/// decide whether the incoming Schema event is a real change.
/// ?1=proxy, ?2=kind, ?3=item_key.
pub const GET_SCHEMA_ITEM_HASH_SQL: &str = "
SELECT payload_hash FROM schema_items
WHERE proxy = ?1 AND kind = ?2 AND item_key = ?3;
";

/// UPSERT the latest schema item snapshot.
/// ?1=proxy, ?2=kind, ?3=item_key, ?4=payload, ?5=payload_hash, ?6=captured_at.
pub const UPSERT_SCHEMA_ITEM_SQL: &str = "
INSERT INTO schema_items (proxy, kind, item_key, payload, payload_hash, captured_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(proxy, kind, item_key) DO UPDATE SET
    payload      = excluded.payload,
    payload_hash = excluded.payload_hash,
    captured_at  = excluded.captured_at;
";

/// APPEND a row to the schema change log.
/// ?1=proxy, ?2=kind, ?3=item_key, ?4=reason, ?5=old_hash, ?6=new_hash, ?7=detected_at.
pub const INSERT_SCHEMA_CHANGE_SQL: &str = "
INSERT INTO schema_changes (proxy, kind, item_key, reason, old_hash, new_hash, detected_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);
";
