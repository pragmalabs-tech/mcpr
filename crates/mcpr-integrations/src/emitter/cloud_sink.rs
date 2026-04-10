//! Cloud event sink — batches proxy events and POSTs them to cloud.mcpr.app.
//!
//! Implements `EventSink` from mcpr-core. Internally buffers events and
//! flushes on batch size or interval, with retry + exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use mcpr_core::event::{EventSink, ProxyEvent};
use tokio::sync::mpsc;

/// Callback invoked after each cloud sync attempt.
pub type SyncCallback = Arc<dyn Fn(SyncStatus) + Send + Sync>;

/// Result of a cloud sync flush.
pub enum SyncStatus {
    Ok { count: usize },
    Failed { message: String },
}

/// Configuration for the cloud sink.
pub struct CloudSinkConfig {
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
    /// Optional callback for reporting sync status
    pub on_flush: Option<SyncCallback>,
}

/// Cloud event sink — batches and POSTs proxy events to the cloud API.
///
/// Events are queued via an internal mpsc channel. A background tokio task
/// drains the channel and flushes batches with retry.
pub struct CloudSink {
    tx: mpsc::Sender<ProxyEvent>,
}

impl CloudSink {
    pub fn new(config: CloudSinkConfig) -> Self {
        let (tx, rx) = mpsc::channel::<ProxyEvent>(1000);
        tokio::spawn(flush_loop(rx, config));
        Self { tx }
    }
}

impl EventSink for CloudSink {
    fn on_event(&self, event: &ProxyEvent) {
        // Non-blocking: clone and send. Drop if channel is full.
        let _ = self.tx.try_send(event.clone());
    }

    fn name(&self) -> &'static str {
        "cloud"
    }
}

async fn flush_loop(mut rx: mpsc::Receiver<ProxyEvent>, config: CloudSinkConfig) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let mut buffer: Vec<ProxyEvent> = Vec::with_capacity(config.batch_size);
    let mut interval = tokio::time::interval(config.flush_interval);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(event) = msg else {
                    // Channel closed — flush remaining.
                    if !buffer.is_empty() {
                        flush_batch(&client, &config, &mut buffer).await;
                    }
                    break;
                };

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
    config: &CloudSinkConfig,
    buffer: &mut Vec<ProxyEvent>,
) {
    let events = std::mem::take(buffer);

    // Convert to JSON — the cloud API accepts the ProxyEvent format directly.
    // Stamp server slug on each event.
    let payload: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            let mut val = serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
            if let Some(ref server) = config.server
                && let Some(obj) = val.as_object_mut()
            {
                obj.entry("server")
                    .or_insert(serde_json::Value::String(server.clone()));
            }
            val
        })
        .collect();

    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(_) => return,
    };

    // Retry with exponential backoff: 1s, 2s, 4s
    for attempt in 0..3u32 {
        match client
            .post(&config.endpoint)
            .header("Authorization", format!("Bearer {}", config.token))
            .header("Content-Type", "application/json")
            .body(body.clone())
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
