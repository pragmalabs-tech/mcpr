//! Middleware traits for the pipeline.
//!
//! See `PIPELINE.md` §Middleware. `RequestMiddleware` inspects and may
//! transform or short-circuit a `Request`; `ResponseMiddleware` mirrors
//! the pattern on the way out.
//!
//! The driver that runs these traits lives in [`super::driver`].

use async_trait::async_trait;

use super::values::{Context, Request, Response};

/// Decision a request middleware returns.
#[derive(Debug)]
pub enum Flow {
    /// Forward the (possibly transformed) request to the next
    /// middleware.
    Continue(Request),
    /// Produce a response directly. Later request middlewares and the
    /// router/transport do not run, but the full response chain still
    /// processes the response.
    ShortCircuit(Response),
}

#[async_trait]
pub trait RequestMiddleware: Send + Sync {
    /// Stable identifier used for `info!` registration logs and test
    /// introspection. Return a `&'static str` literal.
    fn name(&self) -> &'static str;

    /// Inspect or transform the request. Variants the middleware does
    /// not care about return `Flow::Continue(req)` unchanged.
    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow;
}

#[async_trait]
pub trait ResponseMiddleware: Send + Sync {
    fn name(&self) -> &'static str;

    /// Inspect or transform the response. Always returns a `Response`
    /// — replacing one variant with another is how a middleware swaps
    /// the result.
    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response;
}
