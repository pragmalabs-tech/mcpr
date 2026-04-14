//! Store commands — thin wrappers: logic → render.

use mcpr_integrations::store::query::store_ops::VacuumParams;

use crate::config::StoreVacuumArgs;
use crate::logic::query::{open_query_engine, parse_since};
use crate::render;

pub fn stats() -> Result<(), String> {
    let (engine, db_path) = open_query_engine()?;

    let result = engine
        .store_stats(&db_path)
        .map_err(|e| format!("query failed: {e}"))?;

    render::store_stats(&result, &db_path);
    Ok(())
}

pub fn vacuum(args: StoreVacuumArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let before_ts = parse_since(&args.before)?;

    let result = engine
        .vacuum(&VacuumParams {
            before_ts,
            proxy: args.proxy.clone(),
            dry_run: args.dry_run,
        })
        .map_err(|e| format!("vacuum failed: {e}"))?;

    render::store_vacuum(&result, args.dry_run);
    Ok(())
}
