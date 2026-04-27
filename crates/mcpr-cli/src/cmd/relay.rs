//! Relay lifecycle commands — thin wrappers: logic → render.

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
