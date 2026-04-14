//! Command dispatch — thin handlers that wire logic → render.

mod observe;
mod proxy;
mod relay;
mod store;

use crate::config::{ProxyCommand, RelayCommand, StoreCommand};

pub fn handle_proxy_command(cmd: ProxyCommand) {
    let result = match cmd {
        // Lifecycle commands
        ProxyCommand::Run(_) => {
            unreachable!("`mcpr proxy run` is handled before async dispatch");
        }
        ProxyCommand::Stop(args) => proxy::stop(args),
        ProxyCommand::Restart(args) => proxy::restart(args),
        ProxyCommand::Start(args) => proxy::start(args),
        ProxyCommand::List(args) => proxy::list(args),

        // Observability commands
        ProxyCommand::Logs(args) => observe::logs(args),
        ProxyCommand::Slow(args) => observe::slow(args),
        ProxyCommand::Stats(args) => observe::stats(args),
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
        RelayCommand::Run(_) | RelayCommand::Start(_) => {
            unreachable!("`mcpr relay run/start` is handled before async dispatch");
        }
        RelayCommand::Stop => relay::stop(),
        RelayCommand::Restart(args) => relay::restart(args),
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
