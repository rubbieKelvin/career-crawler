//! Persists one visit atomically: domain (with its company classification, careers page and
//! ATS board), page, links, domain edges, newly discovered frontier URLs, the frontier state
//! change, and the events describing it, all in one transaction. The UI never sees a
//! half-recorded page.

use career_core::domains::DomainStatus;
use career_core::events::{self, Event};
use career_core::frontier::{self, Candidate, Item, State};
use career_core::time::now_ms;
use career_core::urls;
use sqlx::{SqliteConnection, SqlitePool};
use url::Url;

use crate::ats;
use crate::careers;
use crate::classify::{self, COMPANY_THRESHOLD, NOT_COMPANY_THRESHOLD};
use crate::crawl::{Budgets, budget_key};
use crate::fetcher::FetchError;
use crate::parse::ParsedPage;
use crate::scoring::{self, COMPANY_DOMAIN, LinkInput, NOT_COMPANY_PENALTY};
use crate::visit::{Outcome, Visit};

/// Links beyond this on one page are ignored (mega-menus, sitemaps-as-HTML).
const MAX_LINKS_PER_PAGE: usize = 500;
/// Total tries for a URL that fails transiently (timeouts, connection errors, 429/5xx).
const MAX_ATTEMPTS: u32 = 2;
/// Frontier score for a new domain's homepage, queued so the domain can be classified.
const PROBE_HOME_SCORE: f64 = 60.0;

/// What recording a visit produced.
#[derive(Debug)]
pub struct Recorded {
    /// Events appended, main one first.
    pub events: Vec<Event>,
    /// A company with no careers link was just probed. The caller should also scan its
    /// sitemap (network I/O, so it happens outside this transaction).
    pub sitemap_scan: Option<SitemapScan>,
}

#[derive(Debug, Clone)]
pub struct SitemapScan {
    pub domain: String,
    pub home: Url,
    pub depth: u32,
}

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
    budgets: &Budgets,
) -> anyhow::Result<Recorded> {
    let mut tx = pool.begin().await?;
    let now = now_ms();
    let requested = visit.requested.as_str();
    let redirected = visit.final_url != visit.requested;
    let domain = urls::registrable_domain(&visit.final_url)
        .unwrap_or_else(|| urls::host_key(&visit.final_url));

    let mut extra_events = Vec::new();
    let mut sitemap_scan = None;

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

            // Classify before scoring links, so internal links see the domain's new status.
            // ATS board pages describe the vendor's domain, not the company, so they're skipped.
            if !duplicate && !ats::is_board(&visit.final_url) {
                let page = ClassifiedPage {
                    domain_id,
                    domain: &domain,
                    page_id,
                    url: &visit.final_url,
                    parsed,
                    depth: item.depth,
                    is_main_home: careers::is_main_home(&visit.requested, &domain)
                        || careers::is_main_home(&visit.final_url, &domain),
                };
                if let Some(event) = classify_domain(&mut tx, &page, budgets).await? {
                    extra_events.push(event);
                }
                let status = budgets.status(&domain);
                if status != DomainStatus::NotCompany {
                    let found = record_careers(&mut tx, &page, status, policy).await?;
                    extra_events.extend(found.events);
                    sitemap_scan = found.sitemap_scan;
                }
            }

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
                        target_domain_pages: budgets.count(&budget_key(&link.url)),
                        target_status: target_domain
                            .as_deref()
                            .map_or(DomainStatus::Discovered, |d| budgets.status(d)),
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
                    let budget_left = !budgets.is_exhausted(&budget_key(&link.url));
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

    let mut recorded = vec![event];
    recorded.extend(extra_events);
    for event in &recorded {
        events::append(&mut *tx, event).await?;
    }
    tx.commit().await?;
    return Ok(Recorded {
        events: recorded,
        sitemap_scan,
    });
}

struct CareersUpdate {
    events: Vec<Event>,
    sitemap_scan: Option<SitemapScan>,
}

/// Records what this page says about its domain's careers page and ATS board (see
/// `careers`), and probes well-known careers locations for a company that has none yet.
async fn record_careers(
    tx: &mut SqliteConnection,
    page: &ClassifiedPage<'_>,
    status: DomainStatus,
    policy: &LinkPolicy,
) -> anyhow::Result<CareersUpdate> {
    let (mut careers_url, ats, probed): (Option<String>, Option<String>, bool) =
        sqlx::query_as("SELECT careers_url, ats, careers_probed FROM domains WHERE id = ?")
            .bind(page.domain_id)
            .fetch_one(&mut *tx)
            .await?;
    let mut events = Vec::new();

    if careers::is_careers_page(page.url, page.domain)
        && careers::is_better_careers_url(careers_url.as_deref(), page.url)
    {
        sqlx::query("UPDATE domains SET careers_url = ? WHERE id = ?")
            .bind(page.url.as_str())
            .bind(page.domain_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE pages SET kind = 'careers' WHERE id = ?")
            .bind(page.page_id)
            .execute(&mut *tx)
            .await?;
        careers_url = Some(page.url.to_string());
        events.push(Event::CareersFound {
            domain: page.domain.to_string(),
            url: page.url.to_string(),
            source: "page".into(),
            ats: None,
        });
    }

    if ats.is_none()
        && let Some((board, source)) = careers::attributed_board(page.parsed, page.domain)
    {
        let board_url = board.url();
        sqlx::query(
            "UPDATE domains SET ats = ?, ats_token = ?, careers_url = COALESCE(careers_url, ?) WHERE id = ?",
        )
        .bind(board.vendor.as_str())
        .bind(&board.token)
        .bind(board_url.as_str())
        .bind(page.domain_id)
        .execute(&mut *tx)
        .await?;
        careers_url.get_or_insert_with(|| board_url.to_string());
        // Embedded boards aren't links, so queue the board explicitly.
        if page.depth < policy.max_depth {
            let reason = source.as_str();
            let candidate = Candidate {
                url: &board_url,
                score: careers::BOARD_SCORE,
                depth: page.depth + 1,
                from_page_id: Some(page.page_id),
                reason,
            };
            frontier::enqueue(&mut *tx, &candidate).await?;
        }
        events.push(Event::CareersFound {
            domain: page.domain.to_string(),
            url: board_url.to_string(),
            source: source.as_str().into(),
            ats: Some(board.vendor.as_str().into()),
        });
    }

    // Decide from the homepage only: that's where a nav/footer careers link would be, and
    // other pages may be fetched before the careers link they point to.
    let mut sitemap_scan = None;
    if status == DomainStatus::Company
        && page.is_main_home
        && careers_url.is_none()
        && !probed
        && careers::careers_links(page.parsed, page.domain).is_empty()
    {
        for probe in careers::probe_urls(page.url, page.domain) {
            let candidate = Candidate {
                url: &probe,
                score: careers::PROBE_SCORE,
                depth: page.depth + 1,
                from_page_id: Some(page.page_id),
                reason: "probe_careers",
            };
            frontier::enqueue(&mut *tx, &candidate).await?;
        }
        sqlx::query("UPDATE domains SET careers_probed = 1 WHERE id = ?")
            .bind(page.domain_id)
            .execute(&mut *tx)
            .await?;
        sitemap_scan = careers::main_home_url(page.url, page.domain).map(|home| SitemapScan {
            domain: page.domain.to_string(),
            home,
            depth: page.depth,
        });
    }

    return Ok(CareersUpdate {
        events,
        sitemap_scan,
    });
}

struct ClassifiedPage<'a> {
    domain_id: i64,
    domain: &'a str,
    page_id: i64,
    url: &'a Url,
    parsed: &'a ParsedPage,
    depth: u32,
    /// The domain's homepage: we requested or landed on `domain/` or `www.domain/`
    /// (`flutterwave.com/` redirects to `/us/`, which still counts).
    is_main_home: bool,
}

/// Folds one page's company assessment into its domain. The domain score is the best page
/// score so far, and a company never gets downgraded. Applies the side effects of a status
/// change (revive deferred URLs, shift queued scores, update budgets) and returns a
/// `DomainClassified` event if the status changed. Queues the homepage of a newly seen
/// domain whose first page wasn't it.
async fn classify_domain(
    tx: &mut SqliteConnection,
    page: &ClassifiedPage<'_>,
    budgets: &Budgets,
) -> anyhow::Result<Option<Event>> {
    let (previous, prev_score, home_seen, prev_name): (String, Option<f64>, bool, Option<String>) =
        sqlx::query_as("SELECT status, company_score, home_seen, name FROM domains WHERE id = ?")
            .bind(page.domain_id)
            .fetch_one(&mut *tx)
            .await?;
    let previous = DomainStatus::parse(&previous);
    let inbound: u32 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM edges WHERE dst_domain_id = ? AND src_domain_id <> ?",
    )
    .bind(page.domain_id)
    .bind(page.domain_id)
    .fetch_one(&mut *tx)
    .await?;

    let assessment = classify::assess(page.url, page.domain, page.parsed, inbound);
    let is_main_home = page.is_main_home;
    // Only a conclusive homepage can settle "not a company"; a JS shell proves nothing.
    let home_seen = home_seen || (is_main_home && assessment.conclusive);
    // The homepage's name wins; otherwise keep the first name we found.
    let name = if is_main_home {
        assessment.name.clone().or(prev_name)
    } else {
        prev_name.or_else(|| assessment.name.clone())
    };
    let best = prev_score.unwrap_or(0.0).max(assessment.score);

    let status = if previous == DomainStatus::Company {
        DomainStatus::Company
    } else if assessment.parked {
        DomainStatus::NotCompany
    } else if best >= COMPANY_THRESHOLD {
        DomainStatus::Company
    } else if home_seen && best < NOT_COMPANY_THRESHOLD {
        DomainStatus::NotCompany
    } else {
        DomainStatus::Probing
    };

    let reasons = (prev_score.is_none_or(|p| assessment.score >= p)).then(|| {
        serde_json::json!({
            "page": page.url.as_str(),
            "score": assessment.score,
            "signals": assessment.signals,
        })
        .to_string()
    });
    sqlx::query(
        "UPDATE domains SET company_score = ?, status = ?, home_seen = ?, name = ?,
                score_reasons = COALESCE(?, score_reasons)
         WHERE id = ?",
    )
    .bind(best)
    .bind(status.as_str())
    .bind(home_seen)
    .bind(&name)
    .bind(reasons)
    .bind(page.domain_id)
    .execute(&mut *tx)
    .await?;

    if previous == DomainStatus::Discovered
        && status == DomainStatus::Probing
        && !is_main_home
        && let Some(home) = careers::main_home_url(page.url, page.domain)
    {
        let candidate = Candidate {
            url: &home,
            score: PROBE_HOME_SCORE,
            depth: page.depth,
            from_page_id: Some(page.page_id),
            reason: "probe_home",
        };
        frontier::enqueue(&mut *tx, &candidate).await?;
    }

    if status == previous {
        return Ok(None);
    }
    budgets.set_status(page.domain, status);
    if previous == DomainStatus::NotCompany {
        frontier::adjust_domain_scores(&mut *tx, page.domain, NOT_COMPANY_PENALTY).await?;
    }
    match status {
        DomainStatus::Company => {
            frontier::adjust_domain_scores(&mut *tx, page.domain, COMPANY_DOMAIN).await?;
            frontier::revive_deferred(&mut *tx, page.domain).await?;
        }
        DomainStatus::NotCompany => {
            frontier::adjust_domain_scores(&mut *tx, page.domain, -NOT_COMPANY_PENALTY).await?;
        }
        DomainStatus::Discovered | DomainStatus::Probing => {}
    }
    return Ok(Some(Event::DomainClassified {
        domain: page.domain.to_string(),
        name,
        status: status.as_str().to_string(),
        previous: previous.as_str().to_string(),
        score: best,
    }));
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
