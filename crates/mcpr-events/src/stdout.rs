use crate::emitter::EventEmitter;
use crate::event::McprEvent;

/// Emits events as one-line JSON to stdout.
///
/// This is the default emitter. Pipe to jq for pretty printing:
/// ```bash
/// mcpr --mcp :9000 2>/dev/null | jq
/// ```
pub struct StdoutEmitter {
    pretty: bool,
}

impl StdoutEmitter {
    pub fn new() -> Self {
        Self { pretty: false }
    }

    pub fn pretty(mut self, enabled: bool) -> Self {
        self.pretty = enabled;
        self
    }
}

impl Default for StdoutEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter for StdoutEmitter {
    fn emit(&self, event: McprEvent) {
        let json = if self.pretty {
            serde_json::to_string_pretty(&event)
        } else {
            serde_json::to_string(&event)
        };
        if let Ok(json) = json {
            println!("{json}");
        }
    }
}
