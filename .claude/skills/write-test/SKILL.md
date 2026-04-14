---
name: write-tests
description: Always use this skill before writing any test code in the mcpr repository
---

# Writing mcpr Tests

Load this skill when writing or editing tests anywhere in this project.

## Where to Write Tests

**Prefer inline unit tests (`#[cfg(test)]`) for any module.** Each module should own its tests. Choose placement by what you're testing:

| What you're testing | Where to put it |
|---|---|
| Pure functions, parsing, formatting, data transforms | Inline `#[cfg(test)]` in the same file |
| Module-internal logic (event routing, config resolution) | Inline `#[cfg(test)]` in the same file |
| Cross-module integration (proxy → handler → event bus → store) | `tests/` directory (integration tests) |
| HTTP endpoint behavior (axum handlers, admin API) | Inline with `axum_test` or `tests/` with a test server |
| CLI argument parsing and command dispatch | Inline `#[cfg(test)]` in `config.rs` or `cmd/` modules |

**Never test private implementation details that are likely to change.** Test the contract (inputs → outputs), not the internal steps.

## Test Style

### One test, one thing

Each `#[test]` or `#[tokio::test]` function tests **one behavior**. Name it after what it verifies, not what it calls:

```rust
// Good — describes the behavior
#[test]
fn parse_threshold_rejects_empty_unit() { ... }

// Bad — describes the code path
#[test]
fn test_parse_threshold_us() { ... }
```

### Keep test bodies minimal

The test body should contain **only** what is being tested — setup, action, assertion. No logging, no comments explaining what assert_eq does, no redundant intermediate variables.

```rust
// Good
#[test]
fn format_bytes_gigabyte_range() {
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
}

// Bad — unnecessary noise
#[test]
fn format_bytes_gigabyte_range() {
    let input = 2 * 1024 * 1024 * 1024; // 2 GB in bytes
    let result = format_bytes(input);
    let expected = "2.0 GB";
    assert_eq!(result, expected, "Should format 2GB correctly");
}
```

### DRY the scaffolding

If 3+ tests share setup (opening a DB, building a config, constructing an HTTP request), extract a helper function. Name it after what it produces, not what it does:

```rust
fn seeded_engine() -> QueryEngine { ... }
fn gateway_config() -> GatewayConfig { ... }
fn mcp_post_request(body: &str) -> Request<Body> { ... }
```

### Assertions

- **`assert_eq!`** for value equality — always preferred when comparing concrete values
- **`assert!`** for boolean conditions — use when the check is naturally boolean (`is_empty()`, `contains()`, comparisons)
- **`assert!(matches!(...))` or `matches!` guard** for enum variant checks without destructuring
- **Never use `assert!(x == y)`** — use `assert_eq!(x, y)` for better error messages
- **`assert_ne!`** when you need to verify something changed

## Coverage Strategy

Every function or behavior worth testing should have **three categories** of tests:

### 1. Happy path

The normal, expected usage. One or two tests that confirm the function works as designed.

```rust
#[test]
fn parse_since_valid_hours() {
    let ts = parse_since("1h").unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    assert!((now - ts - 3_600_000).abs() < 1000);
}
```

### 2. Edge cases

Inputs at the boundary of valid/invalid. Think about:

- **Zero / empty**: `0`, `""`, `[]`, `None`
- **Boundary values**: just below/at/above thresholds (e.g., 999μs → 1000μs for format switching)
- **Maximum values**: `i64::MAX`, very long strings, huge byte counts
- **Type boundaries**: negative numbers where only positive expected, overflow risk
- **Off-by-one**: first element, last element, exactly-at-limit

```rust
#[test]
fn format_latency_boundary_us_to_ms() {
    assert_eq!(format_latency(999), "999μs");   // just below
    assert_eq!(format_latency(1_000), "1.00ms"); // exactly at boundary
}
```

### 3. Error / invalid input

Confirm the function fails gracefully with bad input — returns `Err`, returns a default, does not panic.

```rust
#[test]
fn parse_since_rejects_garbage() {
    assert!(parse_since("bad").is_err());
    assert!(parse_since("").is_err());
    assert!(parse_since("abc123").is_err());
}
```

### What NOT to test

- **Trivial getters/setters** with no logic
- **Direct library delegations** (if your function just calls `serde_json::to_string`, don't test serde)
- **Display/Debug impls** unless they have custom formatting logic
- **Third-party behavior** — trust your dependencies

## Async Tests

Use `#[tokio::test]` for async code. Keep the runtime config minimal:

```rust
#[tokio::test]
async fn event_bus_routes_to_sinks() {
    let sink = TestSink::new();
    let bus = EventBus::start(vec![Box::new(sink.clone())]);
    bus.emit(test_event());
    bus.shutdown().await;
    assert_eq!(sink.events(), vec![test_event()]);
}
```

For tests that need multi-threaded runtime (spawning tasks, channels):
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_proxy_requests() { ... }
```

## Testing with External Resources

### SQLite (store tests)

Use `tempfile::tempdir()` for an isolated database per test. Clean up is automatic.

```rust
#[test]
fn store_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(StoreConfig { db_path, .. });
    // ... test against the store
}
```

### Filesystem (config, lockfiles)

Use `tempfile::tempdir()` for any test that reads/writes files. Never write to the real `~/.mcpr/`.

### Network (HTTP handlers)

For axum handler tests, build the router and use `axum::body::to_bytes` or tower's `ServiceExt::oneshot`:

```rust
#[tokio::test]
async fn health_endpoint_returns_ok() {
    let app = build_app(test_app_state());
    let resp = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
```

## Test Organization Within a Module

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers (if needed) ───────────────────────────────────
    fn sample_log_row() -> LogRow { ... }

    // ── Tests grouped by function/behavior ────────────────────

    // parse_since
    #[test]
    fn parse_since_valid() { ... }
    #[test]
    fn parse_since_rejects_empty() { ... }

    // parse_threshold_us
    #[test]
    fn parse_threshold_microseconds() { ... }
    #[test]
    fn parse_threshold_milliseconds() { ... }
    #[test]
    fn parse_threshold_rejects_bare_unit() { ... }
}
```

Group tests by the function or behavior they cover. Separate groups with a blank line. No need for nested modules unless the file has 50+ tests.

## Naming Conventions

Use the pattern `[behavior]__[case]` — double underscore separates **what** from **when/how**:

| Category | Example |
|---|---|
| Happy path | `parse_since__valid_hours` |
| Edge case | `format_latency__boundary_us_to_ms` |
| Error case | `parse_threshold__rejects_empty_unit` |
| Zero/empty | `format_bytes_col__none_returns_dash` |
| Round-trip | `pid_file__roundtrip` |
| Async behavior | `event_bus__flushes_on_shutdown` |
| Negative | `config__missing_mcp_is_error` |

**Naming rules:**
- Use snake_case, no `test_` prefix (the `#[test]` attribute is enough)
- Left of `__`: the function or concept being tested
- Right of `__`: the specific scenario or condition
- Be specific: `parse_since__rejects_empty_string` not `parse_since__bad_input`

## Platform-Specific Tests

Use `#[cfg(unix)]` for tests that depend on Unix features (signals, fork, PID files):

```rust
#[cfg(unix)]
#[test]
fn process_alive_detects_self() {
    assert!(is_process_alive(std::process::id()));
}
```

Do not write platform-specific tests for Windows unless the code has Windows-specific paths.

## Coverage

**Target: at least 70% line coverage per module.** When writing tests for a module, aim to cover every meaningful code path — not just the happy path.

### How to measure

Use `cargo-llvm-cov` (install once: `cargo install cargo-llvm-cov`):

```bash
# Coverage for a single crate (summary)
cargo llvm-cov -p mcpr --lib

# Coverage with per-file detail
cargo llvm-cov -p mcpr --lib --text

# HTML report (opens in browser)
cargo llvm-cov -p mcpr --lib --html --open

# Coverage for a specific module (filter by file)
cargo llvm-cov -p mcpr --lib --text | grep render.rs
```

### What counts toward coverage

- **Must cover**: all public functions, all match arms that handle user input, all error paths that return `Err` or exit
- **Should cover**: internal helpers called from multiple places, non-trivial closures
- **Can skip**: platform-gated code you can't run (`#[cfg(not(unix))]` on macOS), panic-only branches like `unreachable!()`, trivial Display/Debug impls

### When coverage is below 70%

Before finishing a test pass, check coverage. If a module is below 70%:

1. Identify uncovered lines with `--text` or `--html`
2. Prioritize: error handling paths > edge cases > alternate branches
3. Add targeted tests for the uncovered paths
4. Don't write meaningless tests just to hit the number — if the remaining uncovered code is truly untestable (e.g., process::exit paths, daemon fork logic), that's acceptable

### Coverage in CI

When checking coverage in CI or before a PR, use the `--fail-under` flag:

```bash
cargo llvm-cov -p mcpr --lib --fail-under-lines 70
```

## Running Tests

```bash
# All tests in one crate
cargo test -p mcpr

# A specific test
cargo test -p mcpr parse_since

# With output (for debugging)
cargo test -p mcpr -- --nocapture

# All workspace tests
cargo test --workspace
```
