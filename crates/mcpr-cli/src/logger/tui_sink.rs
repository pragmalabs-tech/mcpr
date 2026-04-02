use crate::tui::SharedTuiState;

use super::entry::LogEntry;
use super::sink::LogSink;

/// Sink that pushes log entries to the TUI dashboard.
///
/// Wraps the existing `SharedTuiState` so the TUI continues to work unchanged.
pub struct TuiSink {
    state: SharedTuiState,
}

impl TuiSink {
    pub fn new(state: SharedTuiState) -> Self {
        Self { state }
    }
}

impl LogSink for TuiSink {
    fn emit(&self, entry: &LogEntry) {
        self.state.lock().unwrap().push_log(entry.clone());
    }
}
