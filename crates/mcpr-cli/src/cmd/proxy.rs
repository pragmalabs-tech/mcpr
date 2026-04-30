//! Proxy lifecycle commands — thin wrappers: logic → render.

use inquire::Confirm;

use crate::config::*;
use crate::logic;
use crate::render;

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
