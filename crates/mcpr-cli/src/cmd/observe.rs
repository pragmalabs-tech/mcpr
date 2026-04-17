//! Observability commands — thin wrappers: parse args → query → render.

use mcpr_integrations::store::query::{
    clients::ClientsParams,
    logs::LogsParams,
    schema::{SchemaChangesParams, SchemaParams, SchemaUnusedParams},
    sessions::SessionsParams,
    slow::SlowParams,
    stats::StatsParams,
};

use crate::config::*;
use crate::logic::query::{open_query_engine, parse_since, parse_threshold_us};
use crate::proxy_lock;
use crate::render::{self, OutputMode};

pub fn logs(args: ProxyLogsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let mode = OutputMode::from(args.json);

    // When --session is set and --since is not, show all time.
    let since_ts = match (&args.since, &args.session) {
        (Some(s), _) => parse_since(s)?,
        (None, Some(_)) => 0,
        (None, None) => parse_since("1h")?,
    };

    let params = LogsParams {
        proxy: name,
        since_ts,
        limit: args.tail,
        tool: args.tool.clone(),
        method: args.method.clone(),
        session: args.session.clone(),
        status: args.status.clone(),
        error_code: args.error_code.clone(),
    };

    let rows = engine
        .logs(&params)
        .map_err(|e| format!("query failed: {e}"))?;

    render::log_rows(&rows, mode, true);
    if rows.is_empty() && mode == OutputMode::Pretty {
        render::logs_empty();
    }

    // --follow mode: poll for new rows
    if args.follow {
        let mut last_ts = rows.last().map(|r| r.ts).unwrap_or(since_ts);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let new_rows = engine
                .logs_since(&params, last_ts)
                .map_err(|e| format!("follow query failed: {e}"))?;
            render::log_rows(&new_rows, mode, false);
            if let Some(r) = new_rows.last() {
                last_ts = r.ts;
            }
        }
    }

    Ok(())
}

pub fn slow(args: ProxySlowArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let mode = OutputMode::from(args.json);
    let since_ts = parse_since(&args.since)?;
    let threshold_us = parse_threshold_us(&args.threshold)?;

    let params = SlowParams {
        proxy: name.clone(),
        threshold_us,
        since_ts,
        tool: args.tool.clone(),
        limit: args.limit,
    };

    let rows = engine
        .slow(&params)
        .map_err(|e| format!("query failed: {e}"))?;

    if mode == OutputMode::Pretty {
        render::slow_banner(&name, &args.since, &args.threshold);
    }
    render::slow_rows(&rows, mode, true);
    if mode == OutputMode::Pretty {
        if rows.is_empty() {
            render::slow_empty();
        } else {
            render::slow_summary(&rows, &args.since);
        }
    }

    // --follow mode
    if args.follow {
        let mut last_ts = rows.last().map(|r| r.ts).unwrap_or(since_ts);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let new_rows = engine
                .slow_since(&params, last_ts)
                .map_err(|e| format!("follow query failed: {e}"))?;
            render::slow_rows(&new_rows, mode, false);
            if let Some(r) = new_rows.last() {
                last_ts = r.ts;
            }
        }
    }

    Ok(())
}

pub fn sessions(args: ProxySessionsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let since_ts = parse_since(&args.since)?;

    let rows = engine
        .sessions(&SessionsParams {
            proxy: name.clone(),
            since_ts,
            limit: args.limit,
            active_only: args.active,
            client: args.client.clone(),
        })
        .map_err(|e| format!("query failed: {e}"))?;

    render::sessions(&rows, &name, &args.since, args.json.into());
    Ok(())
}

pub fn clients(args: ProxyClientsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let since_ts = parse_since(&args.since)?;

    let rows = engine
        .clients(&ClientsParams {
            proxy: name.clone(),
            since_ts,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    render::clients(&rows, &name, &args.since, args.json.into());
    Ok(())
}

pub fn status(args: ProxyStatusArgs) -> Result<(), String> {
    let mode = OutputMode::from(args.json);

    // Show running proxies from lockfiles.
    let proxies = proxy_lock::list_proxies();
    let running: Vec<_> = proxies
        .iter()
        .filter(|(_, s)| matches!(s, proxy_lock::LockStatus::Held(_)))
        .collect();

    if !running.is_empty() && mode == OutputMode::Pretty {
        render::status_running_proxies(&running);
    }

    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let since_ts = parse_since(&args.since)?;

    let stats_result = engine
        .stats(&StatsParams {
            proxy: name.clone(),
            since_ts,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    let session_rows = engine
        .sessions(&SessionsParams {
            proxy: name.clone(),
            since_ts,
            limit: 1000,
            active_only: false,
            client: None,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    render::status_overview(
        &stats_result,
        &session_rows,
        &running,
        &name,
        &args.since,
        mode,
    );
    Ok(())
}

pub fn session(args: ProxySessionArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;

    let detail = engine
        .session_detail(&args.session_id)
        .map_err(|e| format!("query failed: {e}"))?
        .ok_or_else(|| format!("session not found: {}", args.session_id))?;

    render::session_detail(&detail, args.json.into());
    Ok(())
}

pub fn schema(args: ProxySchemaArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = args.proxy.clone();
    let mode = OutputMode::from(args.json);

    if args.unused {
        let since_ts = parse_since(&args.since)?;
        let rows = engine
            .schema_unused(&SchemaUnusedParams {
                proxy: name.clone(),
                since_ts,
            })
            .map_err(|e| format!("query failed: {e}"))?;

        render::schema_unused(&rows, &name, &args.since, mode);
        return Ok(());
    }

    if args.changes {
        let rows = engine
            .schema_changes(&SchemaChangesParams {
                upstream_url: None,
                method: args.method.clone(),
                limit: args.limit,
            })
            .map_err(|e| format!("query failed: {e}"))?;

        render::schema_changes(&rows, mode);
        return Ok(());
    }

    let rows = engine
        .schema(&SchemaParams {
            upstream_url: None,
            method: args.method.clone(),
        })
        .map_err(|e| format!("query failed: {e}"))?;

    // Get status summary if we have an initialize payload.
    let status = rows
        .iter()
        .find(|r| r.method == "initialize")
        .and_then(|init_row| engine.schema_status(&init_row.upstream_url).ok());

    render::schema(&rows, status.as_ref(), mode);
    Ok(())
}
