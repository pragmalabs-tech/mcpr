//! Command dispatch - thin handlers that wire logic to render.

mod proxy;
pub mod setup;
mod store;

use crate::config::{ProxyCommand, StoreCommand};

pub fn handle_proxy_command(cmd: ProxyCommand) {
    let result = match cmd {
        ProxyCommand::Run(_) => {
            unreachable!("`mcpr proxy run` is handled before async dispatch");
        }
        ProxyCommand::Setup(_) => {
            unreachable!("`mcpr proxy setup` is handled in async dispatch");
        }
        ProxyCommand::Stop(args) => proxy::stop(args),
        ProxyCommand::List(args) => proxy::list(args),
        ProxyCommand::Delete(args) => proxy::delete(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

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
