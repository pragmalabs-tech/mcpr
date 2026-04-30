//! Command dispatch — thin handlers that wire logic → render.

mod observe;
mod proxy;
mod relay;
pub mod setup;
mod store;

use crate::config::{ProxyCommand, RelayCommand, StoreCommand};

pub fn handle_proxy_command(cmd: ProxyCommand) {
    let result = match cmd {
        // Lifecycle commands
        ProxyCommand::Run(_) => {
            unreachable!("`mcpr proxy run` is handled before async dispatch");
        }
        ProxyCommand::Setup(_) => {
            unreachable!("`mcpr proxy setup` is handled in async dispatch");
        }
        ProxyCommand::Stop(args) => proxy::stop(args),
        ProxyCommand::List(args) => proxy::list(args),
        ProxyCommand::Delete(args) => proxy::delete(args),

        // Observability commands
        ProxyCommand::Logs(args) => observe::logs(args),
        ProxyCommand::Slow(args) => observe::slow(args),
        ProxyCommand::Sessions(args) => observe::sessions(args),
        ProxyCommand::Clients(args) => observe::clients(args),
        ProxyCommand::Status(args) => observe::status(args),
        ProxyCommand::Session(args) => observe::session(args),
        ProxyCommand::Schema(args) => observe::schema(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

pub fn handle_relay_command(cmd: RelayCommand) {
    let result = match cmd {
        RelayCommand::Run(_) => {
            unreachable!("`mcpr relay run` is handled before async dispatch");
        }
        RelayCommand::Stop => relay::stop(),
        RelayCommand::Status => relay::status(),
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
