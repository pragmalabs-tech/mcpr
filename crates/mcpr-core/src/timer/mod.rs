use std::fmt;
use std::sync::Mutex;
use std::time::Duration;
use std::{sync::Arc, time::Instant};

#[derive(Debug, Clone, Default)]
pub struct Timer {
    items: Arc<Mutex<Vec<TimerItem>>>,
}

#[derive(Debug)]
pub struct TimerItem {
    name: String,
    started_at: Instant,
    ended_at: Option<Instant>,
}

impl fmt::Display for TimerItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ended_at {
            Some(ended) => write!(
                f,
                "{}: {}",
                self.name,
                format_duration(ended.duration_since(self.started_at))
            ),
            None => write!(
                f,
                "{}: {} (running)",
                self.name,
                format_duration(self.started_at.elapsed())
            ),
        }
    }
}

fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 1_000 {
        format!("{}ns", nanos)
    } else if nanos < 1_000_000 {
        format!("{:.2}µs", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

impl Timer {
    pub fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn track_start(&mut self, name: &str) {
        let item = TimerItem {
            name: name.into(),
            started_at: Instant::now(),
            ended_at: None,
        };

        let mut lock = self.items.lock().unwrap();
        lock.push(item);
    }

    pub fn track_end(&mut self, name: &str) {
        let mut lock = self.items.lock().unwrap();
        if let Some(item) = lock.iter_mut().find(|i| i.name == name) {
            item.ended_at = Some(Instant::now());
        }
    }
}
