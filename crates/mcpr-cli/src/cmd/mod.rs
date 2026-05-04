//! Command dispatch - thin handlers that wire logic to render.

pub mod setup;
mod store;

use crate::config::StoreCommand;

pub fn handle_store_command(cmd: StoreCommand) {
    let result = match cmd {
        StoreCommand::Stats => store::stats(),
        StoreCommand::Vacuum(args) => store::vacuum(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
