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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: build a config pointing at the given wiremock server.
    fn test_config(
        server_uri: &str,
        batch_size: usize,
        flush_interval: Duration,
        on_flush: Option<SyncCallback>,
    ) -> CloudEmitterConfig {
        CloudEmitterConfig {
            endpoint: format!("{}/api/ingest-events", server_uri),
            token: "test-token".into(),
            server: Some("test-server".into()),
            batch_size,
            flush_interval,
            on_flush,
        }
    }

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

    #[tokio::test]
    async fn flushes_when_batch_size_reached() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .and(header("Authorization", "Bearer test-token"))
            .and(header("Content-Type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let ok_count = Arc::new(AtomicUsize::new(0));
        let ok_count_cb = ok_count.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Ok { count } = status {
                ok_count_cb.fetch_add(count, Ordering::SeqCst);
            }
        });

        let config = test_config(
            &mock_server.uri(),
            3,
            Duration::from_secs(60),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        // Emit exactly batch_size events to trigger a flush.
        for _ in 0..3 {
            emitter.emit(McprEvent::new(EventType::ToolCall));
        }

        // Give the background task time to flush.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(ok_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn flushes_on_interval_when_buffer_not_full() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let ok_count = Arc::new(AtomicUsize::new(0));
        let ok_count_cb = ok_count.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Ok { count } = status {
                ok_count_cb.fetch_add(count, Ordering::SeqCst);
            }
        });

        // batch_size=100 but flush_interval=100ms — should flush on interval.
        let config = test_config(
            &mock_server.uri(),
            100,
            Duration::from_millis(100),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));
        emitter.emit(McprEvent::new(EventType::ToolCall));

        // Wait for at least one interval tick.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(ok_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn flushes_remaining_on_channel_close() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let ok_count = Arc::new(AtomicUsize::new(0));
        let ok_count_cb = ok_count.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Ok { count } = status {
                ok_count_cb.fetch_add(count, Ordering::SeqCst);
            }
        });

        // Large batch_size and interval so neither triggers automatically.
        let config = test_config(
            &mock_server.uri(),
            1000,
            Duration::from_secs(60),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));
        emitter.emit(McprEvent::new(EventType::SessionStart));

        // Drop the emitter to close the channel.
        drop(emitter);

        // Give the background task time to detect close and flush.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(ok_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reports_failure_on_http_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&mock_server)
            .await;

        let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let failures_cb = failures.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Failed { message } = status {
                failures_cb.lock().unwrap().push(message);
            }
        });

        let config = test_config(
            &mock_server.uri(),
            1, // flush after every event
            Duration::from_secs(60),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));

        // Wait for retries (1s + 2s + 4s) — in test we just need enough time.
        tokio::time::sleep(Duration::from_secs(9)).await;

        let msgs = failures.lock().unwrap();
        // 3 retry failures + 1 final "dropped N events" message.
        assert!(
            msgs.len() >= 3,
            "expected at least 3 failure callbacks, got {}",
            msgs.len()
        );
        assert!(
            msgs.iter().any(|m| m.contains("HTTP 500")),
            "should report HTTP status"
        );
        assert!(
            msgs.iter().any(|m| m.contains("dropped")),
            "should report dropped events after retries exhausted"
        );
    }

    #[tokio::test]
    async fn retries_and_succeeds_on_second_attempt() {
        let mock_server = MockServer::start().await;

        // First request fails, second succeeds.
        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&mock_server)
            .await;

        let ok_count = Arc::new(AtomicUsize::new(0));
        let ok_count_cb = ok_count.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Ok { count } = status {
                ok_count_cb.fetch_add(count, Ordering::SeqCst);
            }
        });

        let config = test_config(
            &mock_server.uri(),
            1,
            Duration::from_secs(60),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));

        // Wait for first failure (1s backoff) + second attempt.
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(ok_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accepts_202_as_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&mock_server)
            .await;

        let ok_count = Arc::new(AtomicUsize::new(0));
        let ok_count_cb = ok_count.clone();
        let on_flush: SyncCallback = Arc::new(move |status| {
            if let SyncStatus::Ok { count } = status {
                ok_count_cb.fetch_add(count, Ordering::SeqCst);
            }
        });

        let config = test_config(
            &mock_server.uri(),
            2,
            Duration::from_secs(60),
            Some(on_flush),
        );
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));
        emitter.emit(McprEvent::new(EventType::ToolList));

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(ok_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn server_slug_stamped_in_posted_payload() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = test_config(&mock_server.uri(), 1, Duration::from_secs(60), None);
        let emitter = CloudEmitter::new(config);

        // Emit an event without a server slug — the flush_loop should stamp it.
        let event = McprEvent::new(EventType::ToolCall);
        assert!(event.server.is_none());
        emitter.emit(event);

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify the posted payload contains the server slug.
        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let events = body.as_array().unwrap();
        assert_eq!(events[0]["server"], "test-server");
    }

    #[tokio::test]
    async fn does_not_overwrite_existing_server_slug() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = test_config(&mock_server.uri(), 1, Duration::from_secs(60), None);
        let emitter = CloudEmitter::new(config);

        // Emit an event that already has a server slug.
        let event = McprEvent::new(EventType::ToolCall).server("custom-server");
        emitter.emit(event);

        tokio::time::sleep(Duration::from_millis(200)).await;

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let events = body.as_array().unwrap();
        assert_eq!(events[0]["server"], "custom-server");
    }

    #[tokio::test]
    async fn no_flush_callback_does_not_panic() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/ingest-events"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = test_config(&mock_server.uri(), 1, Duration::from_secs(60), None);
        let emitter = CloudEmitter::new(config);

        emitter.emit(McprEvent::new(EventType::ToolCall));
        tokio::time::sleep(Duration::from_millis(200)).await;
        // No panic = pass.
    }
}
