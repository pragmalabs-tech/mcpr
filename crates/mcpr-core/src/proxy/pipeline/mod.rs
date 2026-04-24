//! Per-request proxy pipeline: a trait-driven middleware chain built on
//! a small custom driver (see `PIPELINE.md`). The entrypoint
//! is [`driver::Pipeline::run`], constructed once at startup via
//! [`crate::proxy::build_default_pipeline`].

pub mod driver;
pub mod middleware;
pub mod middlewares;
pub mod stubs;
pub mod values;
