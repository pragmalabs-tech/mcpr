//! Per-request proxy pipeline: parse → route → request middleware → handler
//! → response middleware → emit. [`run`] is the single entrypoint every
//! HTTP request goes through.

pub mod context;
pub mod emit;
pub mod middleware;
pub mod parser;
pub mod route;
pub mod run;

pub use run::run;
