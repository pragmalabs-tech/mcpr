//! Plain-function steps that the buffered/streamed handlers call
//! explicitly. Each former `ResponseMiddleware` impl lives here as a
//! function — the middleware trait is deleted in Step 6.

pub mod health;
pub mod rewrite;
pub mod schema;
pub mod session;
pub mod url_map;
