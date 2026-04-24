//! Per-request proxy pipeline: parse → route → request middleware → handler
//! → response middleware → emit. [`run`] is the single entrypoint every
//! HTTP request goes through.

pub mod context;
pub mod emit;
pub mod handlers;
pub mod parser;
pub mod route;
pub mod run;
pub mod steps;

pub mod driver;
pub mod envelope;
pub mod message;
pub mod middleware;
pub mod stubs;
pub mod values;

pub use run::run;
