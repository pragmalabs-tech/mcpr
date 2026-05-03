use std::fmt;
use std::sync::Mutex;
use std::time::Duration;
use std::{sync::Arc, time::Instant};

use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct Timer {
    items: Arc<Mutex<Vec<TimerItem>>>,
}

#[derive(Debug)]
pub struct TimerItem {
    id: Uuid,
    name: String,
    started_at: Instant,
    ended_at: Option<Instant>,
}

impl fmt::Display for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lock = self.items.lock().unwrap();
        for item in lock.iter() {
            writeln!(f, "  {}", item)?;
        }
        Ok(())
    }
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

    pub fn track_start(&self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        let item = TimerItem {
            id,
            name: name.into(),
            started_at: Instant::now(),
            ended_at: None,
        };

        let mut lock = self.items.lock().unwrap();
        lock.push(item);

        id
    }

    pub fn track_end(&self, id: Uuid) {
        let mut lock = self.items.lock().unwrap();
        if let Some(item) = lock.iter_mut().find(|i| i.id == id) {
            item.ended_at = Some(Instant::now());
        }
    }

    /// Snapshot every span as `(name, duration_us)`. Spans that haven't
    /// had `track_end` called yet contribute 0us so the result is always
    /// well-defined for serialization.
    pub fn to_spans_us(&self) -> Vec<(String, u64)> {
        let lock = self.items.lock().unwrap();
        lock.iter()
            .map(|item| {
                let us = item
                    .ended_at
                    .map(|e| e.duration_since(item.started_at).as_micros() as u64)
                    .unwrap_or(0);
                (item.name.clone(), us)
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn to_spans_us__empty_timer_has_no_spans() {
        let timer = Timer::new();
        assert!(timer.to_spans_us().is_empty());
    }

    #[test]
    fn to_spans_us__closed_span_records_positive_duration() {
        let timer = Timer::new();
        let id = timer.track_start("Stage");
        std::thread::sleep(Duration::from_micros(50));
        timer.track_end(id);

        let spans = timer.to_spans_us();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "Stage");
        assert!(spans[0].1 >= 50);
    }

    #[test]
    fn to_spans_us__open_span_reports_zero() {
        let timer = Timer::new();
        timer.track_start("Open");

        let spans = timer.to_spans_us();
        assert_eq!(spans, vec![("Open".to_string(), 0)]);
    }

    #[test]
    fn to_spans_us__preserves_insertion_order() {
        let timer = Timer::new();
        let a = timer.track_start("A");
        let b = timer.track_start("B");
        let c = timer.track_start("C");
        timer.track_end(a);
        timer.track_end(b);
        timer.track_end(c);

        let names: Vec<String> = timer.to_spans_us().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn to_spans_us__keeps_duplicate_names_as_separate_entries() {
        let timer = Timer::new();
        let a = timer.track_start("Stage");
        let b = timer.track_start("Stage");
        timer.track_end(a);
        timer.track_end(b);

        let spans = timer.to_spans_us();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, "Stage");
        assert_eq!(spans[1].0, "Stage");
    }
}
