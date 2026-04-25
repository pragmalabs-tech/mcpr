//! Proxy lifecycle commands — thin wrappers: logic → render.

use std::path::Path;

use inquire::Confirm;

use crate::config::*;
use crate::logic;
use crate::render;

pub fn start(args: ProxyStartArgs) -> Result<(), String> {
    logic::proxy::start_proxy(&args.name)?;
    render::proxy_started(&args.name);
    Ok(())
}

pub fn stop(args: ProxyStopArgs) -> Result<(), String> {
    if args.all {
        let stopped = logic::proxy::stop_all_proxies();
        if stopped.is_empty() {
            render::no_running_proxies();
        } else {
            render::stopped_proxies(&stopped);
        }
        return Ok(());
    }

    let name = args
        .name
        .ok_or_else(|| "proxy name required. Use --all to stop all proxies.".to_string())?;

    match logic::proxy::stop_proxy(&name)? {
        logic::proxy::StopResult::Stopped { name, pid } => {
            render::proxy_stopping(&name, pid);
            render::proxy_stopped_done();
        }
        logic::proxy::StopResult::StaleCleaned { name } => {
            render::proxy_stale_cleaned(&name);
        }
    }
    Ok(())
}

pub fn restart(args: ProxyRestartArgs) -> Result<(), String> {
    if args.all {
        if args.config.is_some() {
            return Err("--config cannot be combined with --all".to_string());
        }
        let count = logic::proxy::restart_all_proxies()?;
        if count == 0 {
            render::no_proxies_to_restart();
        }
        return Ok(());
    }

    let name = args
        .name
        .ok_or_else(|| "proxy name required. Use --all to restart all proxies.".to_string())?;

    logic::proxy::restart_proxy(&name, args.config.as_deref().map(Path::new))?;
    render::proxy_restarted(&name);
    Ok(())
}

pub fn reload(args: ProxyReloadArgs) -> Result<(), String> {
    let outcome = logic::proxy::reload_proxy(&args.name, Path::new(&args.config))?;
    render_reload_outcome(&args.name, outcome)
}

/// Map a `ReloadOutcome` to a `Result` for the CLI: applied → printed
/// success, anything else → hard error so the binary exits non-zero with a
/// concrete reason.
fn render_reload_outcome(name: &str, outcome: logic::proxy::ReloadOutcome) -> Result<(), String> {
    match outcome {
        logic::proxy::ReloadOutcome::Applied => {
            render::proxy_reload_applied(name);
            Ok(())
        }
        logic::proxy::ReloadOutcome::Rejected { message } => {
            Err(format!("reload of proxy \"{name}\" rejected: {message}"))
        }
        logic::proxy::ReloadOutcome::Timeout => Err(format!(
            "reload signal sent to proxy \"{name}\", but no acknowledgment within 3s. Check `mcpr proxy logs {name}`."
        )),
    }
}

pub fn list(args: ProxyListArgs) -> Result<(), String> {
    let proxies = logic::proxy::list_proxies();
    render::proxy_list(&proxies, args.json.into());
    Ok(())
}

pub fn delete(args: ProxyDeleteArgs) -> Result<(), String> {
    if !args.yes {
        let prompt = format!(
            "Delete proxy \"{}\"? This removes its config snapshot and logs.",
            args.name
        );
        let confirmed = Confirm::new(&prompt)
            .with_default(false)
            .prompt()
            .map_err(|e| format!("prompt error: {e}"))?;
        if !confirmed {
            render::proxy_delete_cancelled(&args.name);
            return Ok(());
        }
    }

    logic::proxy::delete_proxy(&args.name)?;
    render::proxy_deleted(&args.name);
    Ok(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use logic::proxy::ReloadOutcome;

    #[test]
    fn render_reload_outcome__applied_is_ok() {
        assert!(render_reload_outcome("svc", ReloadOutcome::Applied).is_ok());
    }

    #[test]
    fn render_reload_outcome__rejected_is_err_with_name_and_reason() {
        let err = render_reload_outcome(
            "svc",
            ReloadOutcome::Rejected {
                message: "fields require restart: mcp".into(),
            },
        )
        .unwrap_err();
        assert!(err.contains("svc"));
        assert!(err.contains("rejected"));
        assert!(err.contains("fields require restart: mcp"));
    }

    #[test]
    fn render_reload_outcome__timeout_is_err_with_logs_hint() {
        let err = render_reload_outcome("svc", ReloadOutcome::Timeout).unwrap_err();
        assert!(err.contains("svc"));
        assert!(err.contains("no acknowledgment"));
        assert!(err.contains("mcpr proxy logs"));
    }
}
