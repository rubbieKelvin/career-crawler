//! Runtime control of a crawl: pause/resume/stop, from the UI (via `control_commands`) or
//! from budgets (the sampler's data cap).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use areer_core::control::{self, Command};
use areer_core::events::{self, Event};
use sqlx::SqlitePool;
use tokio::sync::watch;

/// How often the crawler checks `control_commands`.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct CrawlControl {
    paused: AtomicBool,
    stop: watch::Sender<bool>,
}

impl Default for CrawlControl {
    fn default() -> Self {
        return Self {
            paused: AtomicBool::new(false),
            stop: watch::channel(false).0,
        };
    }
}

impl CrawlControl {
    pub fn is_paused(&self) -> bool {
        return self.paused.load(Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Relaxed);
    }

    pub fn request_stop(&self) {
        self.stop.send_replace(true);
    }

    pub fn stop_requested(&self) -> bool {
        return *self.stop.borrow();
    }

    /// Resolves once a stop has been requested (immediately if it already was).
    pub async fn stopped(&self) {
        let mut rx = self.stop.subscribe();
        // `wait_for` only errors if the sender is dropped, which can't happen while `self` lives.
        let _ = rx.wait_for(|stopped| *stopped).await;
    }

    /// Applies a command and records it as a `control_applied` event.
    pub async fn apply(
        &self,
        pool: &SqlitePool,
        command: Command,
        source: &str,
    ) -> anyhow::Result<()> {
        match command {
            Command::Pause => self.set_paused(true),
            Command::Resume => self.set_paused(false),
            Command::Stop => self.request_stop(),
        }
        tracing::info!(command = command.as_str(), source, "control applied");
        events::append(
            pool,
            &Event::ControlApplied {
                command: command.as_str().into(),
                source: source.into(),
            },
        )
        .await?;
        return Ok(());
    }
}

/// Polls `control_commands` until the crawl stops, applying each pending command once.
pub async fn poll_commands(pool: SqlitePool, control: Arc<CrawlControl>) {
    loop {
        match control::pending(&pool).await {
            Ok(pending) => {
                for (id, command) in pending {
                    let status = match command {
                        Some(command) => match control.apply(&pool, command, "ui").await {
                            Ok(()) => "done",
                            Err(e) => {
                                tracing::error!(error = %e, "failed to apply control command");
                                "failed"
                            }
                        },
                        None => "unknown",
                    };
                    if let Err(e) = control::mark(&pool, id, status).await {
                        tracing::error!(error = %e, "failed to mark control command");
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to poll control commands"),
        }
        tokio::select! {
            () = control.stopped() => return,
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use areer_core::db;

    #[tokio::test]
    async fn polls_and_applies_commands() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let control = Arc::new(CrawlControl::default());
        control::submit(&pool, Command::Pause).await.unwrap();
        let poller = tokio::spawn(poll_commands(pool.clone(), control.clone()));

        tokio::time::timeout(Duration::from_secs(5), async {
            while !control.is_paused() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("pause applied");

        control::submit(&pool, Command::Stop).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), control.stopped())
            .await
            .expect("stop applied");
        tokio::time::timeout(Duration::from_secs(5), poller)
            .await
            .unwrap()
            .unwrap();

        let applied: Vec<String> = sqlx::query_scalar(
            "SELECT json_extract(payload, '$.command') FROM events WHERE kind = 'control_applied'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(applied, ["pause", "stop"]);
    }
}
