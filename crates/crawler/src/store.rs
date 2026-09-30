//! Persists one visit atomically: domain, page, links, domain edges, newly discovered
//! frontier URLs, the frontier state change, and the event describing it, all in one
//! transaction. The UI never sees a half-recorded page.

use career_core::events::{self, Event};
use career_core::frontier::{self, Candidate, Item, State};
use career_core::time::now_ms;
use career_core::urls;
use sqlx::{SqliteConnection, SqlitePool};
use url::Url;

use crate::crawl::{DomainPages, budget_key};
use crate::fetcher::FetchError;
use crate::scoring::{self, LinkInput};
use crate::visit::{Outcome, Visit};

/// Links beyond this on one page are ignored (mega-menus, sitemaps-as-HTML).
const MAX_LINKS_PER_PAGE: usize = 500;
/// Total tries for a URL that fails transiently (timeouts, connection errors, 429/5xx).
const MAX_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone)]
pub struct LinkPolicy {
    pub max_depth: u32,
    pub min_link_score: f64,
}

pub async fn record(
    pool: &SqlitePool,
    item: &Item,
    visit: &Visit,
    policy: &LinkPolicy,
    domain_pages: &DomainPages,
) -> anyhow::Result<Event> {
    let mut tx = pool.begin().await?;
    let now = now_ms();
    let requested = visit.requested.as_str();
    let redirected = visit.final_url != visit.requested;
    let domain = urls::registrable_domain(&visit.final_url)
        .unwrap_or_else(|| urls::host_key(&visit.final_url));

    let event = match &visit.outcome {
        Outcome::Page {
            status,
            parsed,
            bytes_wire,
            content_hash,
            ..
        } => {
            let domain_id = upsert_domain(&mut tx, &domain, now).await?;
            // Reached again, e.g. two URLs redirecting to the same page: keep the original row.
            let existing: Option<i64> =
                sqlx::query_scalar("SELECT id FROM pages WHERE url = ? AND fetched_at IS NOT NULL")
                    .bind(visit.final_url.as_str())
                    .fetch_optional(&mut *tx)
                    .await?;
            let same_content: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pages WHERE domain_id = ? AND content_hash = ? AND url <> ?)",
            )
            .bind(domain_id)
            .bind(content_hash)
            .bind(visit.final_url.as_str())
            .fetch_one(&mut *tx)
            .await?;
            let duplicate = existing.is_some() || same_content;
            let page_id = match existing {
                Some(id) => id,
                None => {
                    let kind = if same_content {
                        "duplicate"
                    } else if visit.final_url.path() == "/" {
                        "home"
                    } else {
                        "other"
                    };
                    upsert_page(
                        &mut tx,
                        &PageRow {
                            url: &visit.final_url,
                            domain_id,
                            kind,
                            http_status: Some(status.as_u16()),
                            content_hash: Some(content_hash),
                            bytes_wire: Some(*bytes_wire),
                            error: None,
                        },
                        now,
                    )
                    .await?
                }
            };

            let mut links = 0;
            let mut enqueued = 0;
            if !duplicate && !parsed.nofollow {
                for link in parsed.links.iter().take(MAX_LINKS_PER_PAGE) {
                    links += 1;
                    let target_domain = urls::registrable_domain(&link.url);
                    let depth = item.depth + 1;
                    let scored = scoring::score_link(&LinkInput {
                        url: &link.url,
                        anchor_text: &link.text,
                        nofollow: link.nofollow,
                        source_domain: &domain,
                        target_domain: target_domain.as_deref(),
                        depth,
                        target_domain_pages: domain_pages.get(&budget_key(&link.url)),
                    });

                    sqlx::query(
                        "INSERT INTO page_links (src_page_id, dst_url, anchor_text, link_score)
                         VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                    )
                    .bind(page_id)
                    .bind(link.url.as_str())
                    .bind(&link.text)
                    .bind(scored.as_ref().map(|s| s.score))
                    .execute(&mut *tx)
                    .await?;

                    if let Some(target) = target_domain.as_deref()
                        && target != domain
                        && !scoring::is_blocked(&link.url, Some(target))
                    {
                        let target_id = upsert_domain(&mut tx, target, now).await?;
                        upsert_edge(&mut tx, domain_id, target_id, now).await?;
                    }

                    let Some(scored) = scored else { continue };
                    let budget_left = !domain_pages.is_full(&budget_key(&link.url));
                    if scored.score < policy.min_link_score
                        || depth > policy.max_depth
                        || !budget_left
                    {
                        continue;
                    }
                    let reason = scored.reason();
                    let candidate = Candidate {
                        url: &link.url,
                        score: scored.score,
                        depth,
                        from_page_id: Some(page_id),
                        reason: &reason,
                    };
                    if frontier::enqueue(&mut *tx, &candidate).await? {
                        enqueued += 1;
                    }
                }
            }

            frontier::set_state(&mut *tx, requested, State::Done).await?;
            if redirected {
                frontier::mark_visited(&mut *tx, &visit.final_url, item.depth).await?;
            }
            sqlx::query("UPDATE domains SET last_crawled = ? WHERE id = ?")
                .bind(now)
                .bind(domain_id)
                .execute(&mut *tx)
                .await?;

            Event::PageFetched {
                url: visit.final_url.to_string(),
                domain,
                status: status.as_u16(),
                depth: item.depth,
                links,
                enqueued,
                duplicate,
                bytes_wire: *bytes_wire,
            }
        }

        Outcome::RobotsDenied => {
            frontier::set_state(&mut *tx, requested, State::Skipped).await?;
            Event::FetchFailed {
                url: visit.final_url.to_string(),
                domain: Some(domain),
                reason: "robots_denied".into(),
                will_retry: false,
            }
        }

        failure => {
            let (reason, http_status, transient) = match failure {
                Outcome::NotHtml { .. } => ("not_html".to_string(), None, false),
                Outcome::HttpError { status } => (
                    format!("http_{}", status.as_u16()),
                    Some(status.as_u16()),
                    status.as_u16() == 429 || status.is_server_error(),
                ),
                Outcome::TooManyRedirects => ("too_many_redirects".to_string(), None, false),
                Outcome::Failed(e) => (
                    e.kind().to_string(),
                    None,
                    matches!(e, FetchError::Timeout | FetchError::Connect(_)),
                ),
                Outcome::Page { .. } | Outcome::RobotsDenied => unreachable!("handled above"),
            };
            let will_retry = transient && item.attempts + 1 < MAX_ATTEMPTS;
            if will_retry {
                frontier::retry(&mut *tx, requested).await?;
            } else {
                let domain_id = upsert_domain(&mut tx, &domain, now).await?;
                upsert_page(
                    &mut tx,
                    &PageRow {
                        url: &visit.final_url,
                        domain_id,
                        kind: "other",
                        http_status,
                        content_hash: None,
                        bytes_wire: None,
                        error: Some(&reason),
                    },
                    now,
                )
                .await?;
                frontier::set_state(&mut *tx, requested, State::Done).await?;
                if redirected {
                    frontier::mark_visited(&mut *tx, &visit.final_url, item.depth).await?;
                }
            }
            Event::FetchFailed {
                url: visit.final_url.to_string(),
                domain: Some(domain),
                reason,
                will_retry,
            }
        }
    };

    events::append(&mut *tx, &event).await?;
    tx.commit().await?;
    return Ok(event);
}

async fn upsert_domain(conn: &mut SqliteConnection, host: &str, now: i64) -> anyhow::Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO domains (host, first_seen) VALUES (?, ?)
         ON CONFLICT(host) DO UPDATE SET host = excluded.host
         RETURNING id",
    )
    .bind(host)
    .bind(now)
    .fetch_one(conn)
    .await?;
    return Ok(id);
}

async fn upsert_edge(
    conn: &mut SqliteConnection,
    src: i64,
    dst: i64,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO edges (src_domain_id, dst_domain_id, weight, first_seen) VALUES (?, ?, 1, ?)
         ON CONFLICT(src_domain_id, dst_domain_id) DO UPDATE SET weight = weight + 1",
    )
    .bind(src)
    .bind(dst)
    .bind(now)
    .execute(conn)
    .await?;
    return Ok(());
}

struct PageRow<'a> {
    url: &'a Url,
    domain_id: i64,
    kind: &'a str,
    http_status: Option<u16>,
    content_hash: Option<&'a str>,
    bytes_wire: Option<u64>,
    error: Option<&'a str>,
}

async fn upsert_page(
    conn: &mut SqliteConnection,
    page: &PageRow<'_>,
    now: i64,
) -> anyhow::Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO pages (url, domain_id, kind, http_status, fetched_at, content_hash, bytes_wire, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(url) DO UPDATE SET
           domain_id = excluded.domain_id, kind = excluded.kind, http_status = excluded.http_status,
           fetched_at = excluded.fetched_at, content_hash = excluded.content_hash,
           bytes_wire = excluded.bytes_wire, error = excluded.error
         RETURNING id",
    )
    .bind(page.url.as_str())
    .bind(page.domain_id)
    .bind(page.kind)
    .bind(page.http_status)
    .bind(now)
    .bind(page.content_hash)
    .bind(page.bytes_wire.map(|b| b as i64))
    .bind(page.error)
    .fetch_one(conn)
    .await?;
    return Ok(id);
}
