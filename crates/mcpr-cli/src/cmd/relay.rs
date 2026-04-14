//! Relay lifecycle commands — thin wrappers: logic → render.

use crate::config::*;
use crate::logic;
use crate::render;

pub fn stop() -> Result<(), String> {
    match logic::relay::stop_relay()? {
        logic::relay::StopResult::Stopped { pid } => {
            render::relay_stopping(pid);
            render::relay_stopped_done();
        }
        logic::relay::StopResult::StaleCleaned => {
            render::relay_stale_cleaned();
        }
    }
    Ok(())
}

pub fn restart(args: RelayRestartArgs) -> Result<(), String> {
    if args.config.is_some() {
        // Config provided — stop and re-launch with new config.
        let _ = logic::relay::stop_relay();
        let config_path = args.config.unwrap();
        let exe = std::env::current_exe().map_err(|e| format!("cannot find mcpr binary: {e}"))?;
        let status = std::process::Command::new(exe)
            .args(["relay", "start", &config_path])
            .status()
            .map_err(|e| format!("failed to spawn relay: {e}"))?;
        if !status.success() {
            return Err("relay failed to start".to_string());
        }
    } else {
        logic::relay::restart_relay()?;
    }
    render::relay_restarted();
    Ok(())
}

pub fn status() -> Result<(), String> {
    match logic::relay::relay_status() {
        Ok(info) => {
            render::relay_status(&info);
            Ok(())
        }
        Err(e) => {
            render::relay_not_running();
            Err(e)
        }
    }
}
