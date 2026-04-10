// TUI is currently disabled in `mcpr start`. It will be revived as
// `mcpr proxy view` — a standalone viewer that attaches to a running daemon.
#[allow(dead_code)]
mod app;
pub mod state;
#[allow(dead_code)]
mod ui;

pub use state::{ConnectionStatus, SharedTuiState, new_shared_state};
