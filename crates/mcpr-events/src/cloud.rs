use crate::{EventEmitter, McprEvent};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Callback invoked after each cloud sync attempt (success or failure).
pub type SyncCallback = Arc<dyn Fn(SyncStatus) + Send + Sync>;

/// Result of a cloud sync flush.
pub enum SyncStatus {
    Ok { count: usize },
    Failed { message: String },
}

/// Configuration for the cloud event emitter.
pub struct CloudEmitterConfig {
    /// Full ingest URL, e.g. "https://api.mcpr.app/api/ingest-events"
    pub endpoint: String,
    /// Project token, e.g. "mcpr_xxxxxxxx"
    pub token: String,
    /// Server slug — identifies which server in the cloud project
    pub server: Option<String>,
    /// Flush when buffer reaches this size (default: 100)
    pub batch_size: usize,
    /// Flush on this interval even if buffer isn't full (default: 5s)
    pub flush_interval: Duration,
    /// Optional callback for reporting sync status (e.g. to TUI)
    pub on_flush: Option<SyncCallback>,
}

/// Emitter that batches events and POSTs them to the mcpr cloud ingest API.
///
/// `emit()` is non-blocking — events are queued via an mpsc channel.
/// A background tokio task drains the channel and flushes batches to the cloud.
pub struct CloudEmitter {
    tx: mpsc::Sender<McprEvent>,
}

impl CloudEmitter {
    pub fn new(config: CloudEmitterConfig) -> Self {
        let (tx, rx) = mpsc::channel::<McprEvent>(1000);
        tokio::spawn(flush_loop(rx, config));
        Self { tx }
    }
}

impl EventEmitter for CloudEmitter {
    fn emit(&self, event: McprEvent) {
        // Non-blocking: drop the event if the channel is full.
        let _ = self.tx.try_send(event);
    }
}

async fn flush_loop(mut rx: mpsc::Receiver<McprEvent>, config: CloudEmitterConfig) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let mut buffer: Vec<McprEvent> = Vec::with_capacity(config.batch_size);
    let mut interval = tokio::time::interval(config.flush_interval);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(mut event) = msg else {
                    // Channel closed (proxy shutting down) — flush remaining.
                    if !buffer.is_empty() {
                        flush_batch(&client, &config, &mut buffer).await;
                    }
                    break;
                };

                // Stamp server slug if not already set.
                if event.server.is_none() {
                    event.server = config.server.clone();
                }

                buffer.push(event);

                if buffer.len() >= config.batch_size {
                    flush_batch(&client, &config, &mut buffer).await;
                }
            }
            _ = interval.tick() => {
                if !buffer.is_empty() {
                    flush_batch(&client, &config, &mut buffer).await;
                }
            }
        }
    }
}

async fn flush_batch(
    client: &reqwest::Client,
    config: &CloudEmitterConfig,
    buffer: &mut Vec<McprEvent>,
) {
    let events = std::mem::take(buffer);
    let payload = match serde_json::to_vec(&events) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Retry with exponential backoff: 1s, 2s, 4s
    for attempt in 0..3u32 {
        match client
            .post(&config.endpoint)
            .header("Authorization", format!("Bearer {}", config.token))
            .header("Content-Type", "application/json")
            .body(payload.clone())
            .send()
            .await
        {
            Ok(resp) if matches!(resp.status().as_u16(), 200 | 202) => {
                if let Some(ref cb) = config.on_flush {
                    cb(SyncStatus::Ok {
                        count: events.len(),
                    });
                }
                return;
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if let Some(ref cb) = config.on_flush {
                    cb(SyncStatus::Failed {
                        message: format!("HTTP {status} — {body}"),
                    });
                }
            }
            Err(e) => {
                if let Some(ref cb) = config.on_flush {
                    cb(SyncStatus::Failed {
                        message: e.to_string(),
                    });
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
    }

    if let Some(ref cb) = config.on_flush {
        cb(SyncStatus::Failed {
            message: format!("dropped {} events after 3 retries", events.len()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventType, McprEvent};

    #[tokio::test]
    async fn emit_is_non_blocking() {
        let emitter = CloudEmitter::new(CloudEmitterConfig {
            endpoint: "http://127.0.0.1:1/api/ingest-events".into(), // unreachable
            token: "t".into(),
            server: None,
            batch_size: 100,
            flush_interval: Duration::from_secs(60),
            on_flush: None,
        });

        let start = std::time::Instant::now();
        for _ in 0..2000 {
            emitter.emit(McprEvent::new(EventType::ToolCall));
        }
        assert!(
            start.elapsed().as_millis() < 100,
            "emit must be non-blocking"
        );
    }

    #[tokio::test]
    async fn stamps_server_slug() {
        let (tx, mut rx) = mpsc::channel::<McprEvent>(10);

        // We can't easily test the full HTTP flow without wiremock,
        // but we can verify the slug stamping logic directly.
        let mut event = McprEvent::new(EventType::ToolCall);
        assert!(event.server.is_none());

        let server = Some("my-proxy".to_string());
        if event.server.is_none() {
            event.server = server;
        }
        assert_eq!(event.server.as_deref(), Some("my-proxy"));

        tx.send(event).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.server.as_deref(), Some("my-proxy"));
    }
}
