//! Live updates. The tailer polls the `events` and `metrics_samples` tables for new rows
//! (the crawler is another process, so the DB is the only channel) and broadcasts them as
//! JSON messages to every WebSocket client:
//! - `{"type":"event","id":…,"ts":…,"event":{"kind":…}}`
//! - `{"type":"metrics","sample":{…},"rates":{…}|null}`
//! - `{"type":"lagged","skipped":n}` when a slow client missed messages; it should refetch
//!   `/api/stats` and `/api/graph`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use career_core::samples::{self, Sample};
use career_core::{events, samples::rates};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::AppState;

/// Rows read per poll; more wait for the next poll.
const BATCH: i64 = 500;

pub type Sender = broadcast::Sender<Arc<str>>;

/// Polls for new events and samples every `interval`, starting after what's already
/// stored (history is served by the REST API).
pub async fn tail(pool: SqlitePool, tx: Sender, interval: Duration) {
    let mut last_event: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM events")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    let mut previous: Option<Sample> = samples::latest(&pool, 1)
        .await
        .ok()
        .and_then(|s| s.into_iter().next());
    let mut last_sample = previous.as_ref().map_or(0, |s| s.id);

    loop {
        tokio::time::sleep(interval).await;
        match events::since(&pool, last_event, BATCH).await {
            Ok(batch) => {
                for stored in batch {
                    last_event = stored.id;
                    let msg = json!({"type": "event", "id": stored.id, "ts": stored.ts, "event": stored.event});
                    // No subscribers is fine: nobody is watching.
                    let _ = tx.send(msg.to_string().into());
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to tail events"),
        }
        match samples::since(&pool, last_sample, BATCH).await {
            Ok(batch) => {
                for sample in batch {
                    last_sample = sample.id;
                    let rates = previous.as_ref().and_then(|p| rates(p, &sample));
                    let msg = json!({"type": "metrics", "sample": &sample, "rates": rates});
                    let _ = tx.send(msg.to_string().into());
                    previous = Some(sample);
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to tail metrics samples"),
        }
    }
}

pub async fn ws(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    let rx = state.live.subscribe();
    return upgrade.on_upgrade(move |socket| client(socket, rx));
}

async fn client(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<str>>) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let text: Arc<str> = match msg {
                    Ok(text) => text,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        json!({"type": "lagged", "skipped": skipped}).to_string().into()
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                // Pings are answered by axum; clients have nothing else to say.
                Some(Ok(_)) => {}
            },
        }
    }
}
