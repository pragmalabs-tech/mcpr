//! Proxy lifecycle commands — thin wrappers: logic → render.

use std::path::Path;

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
    let config = args.config.as_deref().ok_or_else(|| {
        format!(
            "--config is required. Use `mcpr proxy reload {} --config <path>` to apply a new config.",
            args.name
        )
    })?;
    logic::proxy::reload_proxy(&args.name, Path::new(config))?;
    render::proxy_reloaded(&args.name);
    Ok(())
}

pub fn list(args: ProxyListArgs) -> Result<(), String> {
    let proxies = logic::proxy::list_proxies();
    render::proxy_list(&proxies, args.json.into());
    Ok(())
}
