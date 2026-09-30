//! Crawl events. Every event is appended to the `events` table; the UI process tails
//! that table, so this is the only channel from crawler to UI (see
//! `brainstorms/11-process-architecture.md`).

use serde::{Deserialize, Serialize};
use sqlx::{SqliteExecutor, SqlitePool};

use crate::time::now_ms;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    CrawlerStarted {
        pid: u32,
    },
    SeedsLoaded {
        parsed: usize,
        enqueued: u64,
        invalid: usize,
    },
    CrawlerStopped {
        reason: String,
    },
    PageFetched {
        url: String,
        domain: String,
        status: u16,
        depth: u32,
        links: usize,
        enqueued: usize,
        /// Same content as another page on the domain; its links were not followed.
        duplicate: bool,
        bytes_wire: u64,
    },
    DomainClassified {
        domain: String,
        name: Option<String>,
        status: String,
        previous: String,
        score: f64,
    },
    CareersFound {
        domain: String,
        /// The careers page, or the ATS board's landing page.
        url: String,
        /// `page`, `ats_embed` or `ats_link`.
        source: String,
        /// ATS vendor, when the careers page is an ATS board.
        ats: Option<String>,
    },
    FetchFailed {
        url: String,
        domain: Option<String>,
        /// `robots_denied`, `http_<status>`, `not_html`, `too_many_redirects`, or a fetch error kind.
        reason: String,
        will_retry: bool,
    },
}

impl Event {
    /// The `kind` tag, also stored in its own column for filtering.
    pub fn kind(&self) -> &'static str {
        return match self {
            Event::CrawlerStarted { .. } => "crawler_started",
            Event::SeedsLoaded { .. } => "seeds_loaded",
            Event::CrawlerStopped { .. } => "crawler_stopped",
            Event::PageFetched { .. } => "page_fetched",
            Event::DomainClassified { .. } => "domain_classified",
            Event::CareersFound { .. } => "careers_found",
            Event::FetchFailed { .. } => "fetch_failed",
        };
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: i64,
    pub ts: i64,
    pub event: Event,
}

/// Appends an event and returns its id. Pass a transaction to write it atomically with
/// the state change it describes.
pub async fn append<'e>(exec: impl SqliteExecutor<'e>, event: &Event) -> anyhow::Result<i64> {
    let payload = serde_json::to_string(event)?;
    let id =
        sqlx::query_scalar("INSERT INTO events (ts, kind, payload) VALUES (?, ?, ?) RETURNING id")
            .bind(now_ms())
            .bind(event.kind())
            .bind(payload)
            .fetch_one(exec)
            .await?;
    return Ok(id);
}

/// Events with id greater than `after_id`, oldest first. Used for tailing and history replay.
pub async fn since(
    pool: &SqlitePool,
    after_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<StoredEvent>> {
    let rows: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT id, ts, payload FROM events WHERE id > ? ORDER BY id LIMIT ?")
            .bind(after_id)
            .bind(limit)
            .fetch_all(pool)
            .await?;
    return rows
        .into_iter()
        .map(|(id, ts, payload)| {
            Ok(StoredEvent {
                id,
                ts,
                event: serde_json::from_str(&payload)?,
            })
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[test]
    fn kind_matches_serde_tag() {
        for e in [
            Event::CrawlerStarted { pid: 1 },
            Event::SeedsLoaded {
                parsed: 0,
                enqueued: 0,
                invalid: 0,
            },
            Event::CrawlerStopped {
                reason: String::new(),
            },
            Event::PageFetched {
                url: String::new(),
                domain: String::new(),
                status: 200,
                depth: 0,
                links: 0,
                enqueued: 0,
                duplicate: false,
                bytes_wire: 0,
            },
            Event::DomainClassified {
                domain: String::new(),
                name: None,
                status: String::new(),
                previous: String::new(),
                score: 0.0,
            },
            Event::CareersFound {
                domain: String::new(),
                url: String::new(),
                source: String::new(),
                ats: None,
            },
            Event::FetchFailed {
                url: String::new(),
                domain: None,
                reason: String::new(),
                will_retry: false,
            },
        ] {
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["kind"], e.kind());
        }
    }

    #[tokio::test]
    async fn append_then_tail() {
        let (_dir, pool) = test_pool().await;
        let a = append(&pool, &Event::CrawlerStarted { pid: 7 })
            .await
            .unwrap();
        let b = append(
            &pool,
            &Event::CrawlerStopped {
                reason: "done".into(),
            },
        )
        .await
        .unwrap();

        let all = since(&pool, 0, 100).await.unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(all[0].event, Event::CrawlerStarted { pid: 7 });

        let tail = since(&pool, a, 100).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].id, b);
    }
}
