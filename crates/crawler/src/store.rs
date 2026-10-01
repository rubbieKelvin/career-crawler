//! Persists one visit atomically: domain (with its company classification, careers page and
//! ATS board), page, links, domain edges, newly discovered frontier URLs, the frontier state
//! change, and the events describing it, all in one transaction. The UI never sees a
//! half-recorded page.

use areer_core::domains::DomainStatus;
use areer_core::enrich;
use areer_core::events::{self, Event};
use areer_core::frontier::{self, Candidate, Item, State};
use areer_core::jobs::{self, BoardRef, Job};
use areer_core::matching;
use areer_core::time::now_ms;
use areer_core::urls;
use areer_llm::tasks::DomainVerdict;
use sqlx::{SqliteConnection, SqlitePool};
use url::Url;

use crate::ats::{self, Board};
use crate::careers;
use crate::classify::{self, COMPANY_THRESHOLD, NOT_COMPANY_THRESHOLD};
use crate::crawl::{Budgets, budget_key};
use crate::extract::{self, BoardJobs};
use crate::fetcher::FetchError;
use crate::parse::ParsedPage;
use crate::scoring::{self, COMPANY_DOMAIN, LinkInput, NOT_COMPANY_PENALTY};
use crate::steer::{self, SharedProfile};
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
    /// A conclusive homepage left the domain in the gray zone (`probing`). The caller may
    /// ask the LLM about it and hand the answer to `record_verdict`.
    pub classify: Option<ClassifyRequest>,
}

impl Recorded {
    fn none() -> Self {
        return Self {
            events: Vec::new(),
            sitemap_scan: None,
            classify: None,
        };
    }
}

/// Everything needed to ask the LLM about a gray-zone domain, and afterwards to carry on
/// as if the heuristics had been sure (the careers probes a company's homepage triggers).
#[derive(Debug, Clone)]
pub struct ClassifyRequest {
    pub domain: String,
    pub domain_id: i64,
    pub page_id: i64,
    pub url: Url,
    pub depth: u32,
    pub parsed: ParsedPage,
    /// The heuristic verdict, as evidence for the LLM.
    pub score: f64,
    pub signals: Vec<&'static str>,
}

/// The LLM must be at least this sure to overrule the heuristics' "don't know".
pub const LLM_CONFIDENCE: f64 = 0.7;

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
    /// The active CV profile, which adds to link scores (crawl steering).
    pub profile: SharedProfile,
}

impl LinkPolicy {
    pub fn new(max_depth: u32, min_link_score: f64) -> Self {
        return Self {
            max_depth,
            min_link_score,
            profile: SharedProfile::default(),
        };
    }
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
    let mut classify_request = None;

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
                let classified = classify_domain(&mut tx, &page, budgets).await?;
                extra_events.extend(classified.event);
                classify_request = classified.request;
                let status = budgets.status(&domain);
                if status != DomainStatus::NotCompany {
                    let found = record_careers(&mut tx, &page, status, policy).await?;
                    extra_events.extend(found.events);
                    sitemap_scan = found.sitemap_scan;
                }
            }

            if !duplicate {
                let postings = extract::json_ld::job_postings(parsed, &visit.final_url);
                if !postings.is_empty() {
                    extra_events.push(
                        record_page_jobs(
                            &mut tx,
                            &visit.final_url,
                            domain_id,
                            &domain,
                            page_id,
                            &postings,
                            now,
                        )
                        .await?,
                    );
                }
            }

            let mut links = 0;
            let mut enqueued = 0;
            if !duplicate && !parsed.nofollow {
                let steering = policy.profile.steering();
                // A source that already yielded well-matching jobs is worth following further.
                let yield_bonus = match policy.profile.active() {
                    Some((profile_id, _)) => steer::yield_points(
                        matching::domain_hits(&mut *tx, profile_id, &domain).await?,
                    ),
                    None => 0.0,
                };
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
                    let steer = steering.boost(&link.url, &link.text);
                    let total_score = scored.score + steer.points + yield_bonus;
                    if total_score < policy.min_link_score
                        || depth > policy.max_depth
                        || !budget_left
                    {
                        continue;
                    }
                    let mut reason = scored.reason();
                    for extra in &steer.reasons {
                        reason.push(',');
                        reason.push_str(extra);
                    }
                    if yield_bonus > 0.0 {
                        reason.push_str(",profile_yield");
                    }
                    // Postings on a board we can read by API collapse into the board itself.
                    let target = match ats::board(&link.url) {
                        Some(board) if extract::has_api(board.vendor) => board.url(),
                        _ => link.url.clone(),
                    };
                    let candidate = Candidate {
                        url: &target,
                        score: total_score,
                        depth,
                        from_page_id: Some(page_id),
                        reason: &reason,
                    };
                    if frontier::enqueue(&mut *tx, &candidate).await? {
                        enqueued += 1;
                        if steer.points > 0.0 {
                            frontier::set_profile_boost(&mut *tx, target.as_str(), steer.points)
                                .await?;
                        }
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
        classify: classify_request,
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
        let key = board.key();
        jobs::ensure_board(&mut *tx, &board_ref(&board, &key), now_ms()).await?;
        jobs::attach_board(tx, &key, page.domain_id).await?;
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
) -> anyhow::Result<Classified> {
    let (previous, prev_score, home_seen, prev_name, llm_checked): (
        String,
        Option<f64>,
        bool,
        Option<String>,
        bool,
    ) = sqlx::query_as(
        "SELECT status, company_score, home_seen, name, llm_checked_at IS NOT NULL FROM domains WHERE id = ?",
    )
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

    // The heuristics can't tell: a real homepage, no company signals strong enough either way.
    let request =
        (status == DomainStatus::Probing && is_main_home && assessment.conclusive && !llm_checked)
            .then(|| ClassifyRequest {
                domain: page.domain.to_string(),
                domain_id: page.domain_id,
                page_id: page.page_id,
                url: page.url.clone(),
                depth: page.depth,
                parsed: page.parsed.clone(),
                score: best,
                signals: assessment.signals.clone(),
            });

    if status == previous {
        return Ok(Classified {
            event: None,
            request,
        });
    }
    apply_status_change(tx, page.domain, previous, status, budgets).await?;
    return Ok(Classified {
        event: Some(Event::DomainClassified {
            domain: page.domain.to_string(),
            name,
            status: status.as_str().to_string(),
            previous: previous.as_str().to_string(),
            score: best,
        }),
        request,
    });
}

struct Classified {
    event: Option<Event>,
    request: Option<ClassifyRequest>,
}

/// The side effects of a domain changing status: budgets, and the queued and deferred URLs
/// that were scored or held back for the old status.
async fn apply_status_change(
    tx: &mut SqliteConnection,
    domain: &str,
    previous: DomainStatus,
    status: DomainStatus,
    budgets: &Budgets,
) -> anyhow::Result<()> {
    budgets.set_status(domain, status);
    if previous == DomainStatus::NotCompany {
        frontier::adjust_domain_scores(&mut *tx, domain, NOT_COMPANY_PENALTY).await?;
    }
    match status {
        DomainStatus::Company => {
            frontier::adjust_domain_scores(&mut *tx, domain, COMPANY_DOMAIN).await?;
            frontier::revive_deferred(&mut *tx, domain).await?;
        }
        DomainStatus::NotCompany => {
            frontier::adjust_domain_scores(&mut *tx, domain, -NOT_COMPANY_PENALTY).await?;
        }
        DomainStatus::Discovered | DomainStatus::Probing => {}
    }
    return Ok(());
}

/// Cleans up what the LLM said about a domain: it may be well-formed JSON with junk in it.
pub fn sanitize_verdict(v: &DomainVerdict) -> DomainVerdict {
    let text = |s: &Option<String>, max: usize| -> Option<String> {
        return s
            .as_deref()
            .map(|s| s.trim().chars().take(max).collect::<String>())
            .filter(|s| !s.is_empty());
    };
    return DomainVerdict {
        is_company: v.is_company,
        company_name: text(&v.company_name, 120),
        industry: text(&v.industry, 40).map(|s| s.to_lowercase()),
        hq_country: enrich::country_code(v.hq_country.as_deref()),
        confidence: if v.confidence.is_finite() {
            v.confidence.clamp(0.0, 1.0)
        } else {
            0.0
        },
        reason: v.reason.trim().chars().take(300).collect(),
    };
}

/// Records the LLM's opinion of a gray-zone domain. A confident answer settles the status
/// like the heuristics would have (budgets, queued URLs, and for a company the careers
/// probes its homepage triggers); an unsure one only keeps the notes (industry, country)
/// and the domain stays `probing`. The domain is asked about once.
pub async fn record_verdict(
    pool: &SqlitePool,
    request: &ClassifyRequest,
    verdict: &DomainVerdict,
    policy: &LinkPolicy,
    budgets: &Budgets,
) -> anyhow::Result<Recorded> {
    let verdict = sanitize_verdict(verdict);
    let mut tx = pool.begin().await?;
    let now = now_ms();
    let (previous, name, score): (String, Option<String>, Option<f64>) =
        sqlx::query_as("SELECT status, name, company_score FROM domains WHERE id = ?")
            .bind(request.domain_id)
            .fetch_one(&mut *tx)
            .await?;
    let previous = DomainStatus::parse(&previous);
    let name = name.or_else(|| verdict.company_name.clone());

    let status = match previous {
        DomainStatus::Probing if verdict.confidence >= LLM_CONFIDENCE => {
            if verdict.is_company {
                DomainStatus::Company
            } else {
                DomainStatus::NotCompany
            }
        }
        other => other,
    };
    sqlx::query(
        "UPDATE domains SET status = ?, name = ?, industry = COALESCE(?, industry),
                hq_country = COALESCE(?, hq_country), llm_checked_at = ?, llm_verdict = ?
         WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(&name)
    .bind(&verdict.industry)
    .bind(&verdict.hq_country)
    .bind(now)
    .bind(serde_json::to_string(&verdict)?)
    .bind(request.domain_id)
    .execute(&mut *tx)
    .await?;

    let mut recorded = Recorded::none();
    if status != previous {
        apply_status_change(&mut tx, &request.domain, previous, status, budgets).await?;
        recorded.events.push(Event::DomainClassified {
            domain: request.domain.clone(),
            name,
            status: status.as_str().to_string(),
            previous: previous.as_str().to_string(),
            score: score.unwrap_or(request.score),
        });
    }
    if status == DomainStatus::Company && previous != DomainStatus::Company {
        let page = ClassifiedPage {
            domain_id: request.domain_id,
            domain: &request.domain,
            page_id: request.page_id,
            url: &request.url,
            parsed: &request.parsed,
            depth: request.depth,
            is_main_home: true,
        };
        let found = record_careers(&mut tx, &page, status, policy).await?;
        recorded.events.extend(found.events);
        recorded.sitemap_scan = found.sitemap_scan;
    }
    for event in &recorded.events {
        events::append(&mut *tx, event).await?;
    }
    tx.commit().await?;
    return Ok(recorded);
}

fn board_ref<'a>(board: &'a Board, key: &'a str) -> BoardRef<'a> {
    return BoardRef {
        key,
        vendor: board.vendor.as_str(),
        token: &board.token,
        host: &board.host,
    };
}

/// Stores JSON-LD postings found on a page. On an ATS board page they belong to that
/// board (and its company, if attributed); otherwise to the page's domain. A page that
/// is exactly one posting becomes `kind = 'job'`.
async fn record_page_jobs(
    tx: &mut SqliteConnection,
    page_url: &Url,
    domain_id: i64,
    domain: &str,
    page_id: i64,
    postings: &[Job],
    now: i64,
) -> anyhow::Result<Event> {
    let board = ats::board(page_url);
    let board_key = board.as_ref().map(Board::key);
    let (job_domain, domain_name) = match (&board, &board_key) {
        (Some(board), Some(key)) => {
            jobs::ensure_board(&mut *tx, &board_ref(board, key), now).await?;
            match jobs::board_domain(&mut *tx, key).await? {
                Some((id, host)) => (Some(id), Some(host)),
                None => (None, None),
            }
        }
        _ => (Some(domain_id), Some(domain.to_string())),
    };
    let mut new = 0;
    for posting in postings {
        if jobs::upsert(tx, posting, job_domain, board_key.as_deref(), now).await? {
            new += 1;
        }
    }
    if let [only] = postings
        && only.url == page_url.as_str()
    {
        sqlx::query("UPDATE pages SET kind = 'job' WHERE id = ?")
            .bind(page_id)
            .execute(&mut *tx)
            .await?;
    }
    return Ok(Event::JobsFound {
        domain: domain_name,
        board: board_key,
        source: "jsonld".into(),
        url: page_url.to_string(),
        total: postings.len(),
        new,
        closed: 0,
    });
}

/// Records one ATS board API fetch: every open job (new, updated, reopened), jobs missing
/// from the listing closed, the board's fetch status, and the frontier URL that led here.
pub async fn record_board(
    pool: &SqlitePool,
    item: &Item,
    board: &Board,
    result: &Result<BoardJobs, String>,
) -> anyhow::Result<Vec<Event>> {
    let mut tx = pool.begin().await?;
    let now = now_ms();
    let key = board.key();
    jobs::ensure_board(&mut *tx, &board_ref(board, &key), now).await?;
    let mut company = jobs::board_domain(&mut *tx, &key).await?;
    if company.is_none()
        && let Ok(listing) = result
    {
        company =
            jobs::find_board_company(&mut tx, &board.token, listing.company.as_deref()).await?;
        if let Some((id, _)) = &company {
            jobs::attach_board(&mut tx, &key, *id).await?;
        }
    }
    let (domain_id, domain) = company.map_or((None, None), |(id, host)| (Some(id), Some(host)));

    let event = match result {
        Ok(listing) => {
            let mut new = 0;
            let mut seen = Vec::with_capacity(listing.jobs.len());
            for job in &listing.jobs {
                if jobs::upsert(&mut tx, job, domain_id, Some(&key), now).await? {
                    new += 1;
                }
                seen.push(job.url.clone());
            }
            let closed = jobs::close_missing(&mut *tx, &key, &seen, now).await?;
            jobs::record_board_fetch(
                &mut *tx,
                &key,
                "ok",
                Some(listing.jobs.len()),
                listing.company.as_deref(),
                now,
            )
            .await?;
            Event::JobsFound {
                domain,
                board: Some(key.clone()),
                source: format!("ats:{}", board.vendor.as_str()),
                url: board.url().to_string(),
                total: listing.jobs.len(),
                new,
                closed,
            }
        }
        Err(reason) => {
            let status = if reason == "http_404" {
                "not_found"
            } else {
                reason.as_str()
            };
            jobs::record_board_fetch(&mut *tx, &key, status, None, None, now).await?;
            if status == "not_found" {
                // A dead board (a name-matched token that doesn't exist, or a closed
                // account) shouldn't block the company's real board from being attributed.
                jobs::detach_board(&mut tx, &key).await?;
            }
            Event::FetchFailed {
                url: board.url().to_string(),
                domain,
                reason: format!("board_{status}"),
                will_retry: false,
            }
        }
    };
    frontier::set_state(&mut *tx, item.url.as_str(), State::Done).await?;
    events::append(&mut *tx, &event).await?;
    tx.commit().await?;
    return Ok(vec![event]);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ats::Vendor;
    use crate::parse::parse_html;
    use areer_core::db;

    static POLICY: std::sync::LazyLock<LinkPolicy> =
        std::sync::LazyLock::new(|| LinkPolicy::new(5, 1.0));

    fn item(url: &str) -> Item {
        let url = Url::parse(url).unwrap();
        return Item {
            host: urls::host_key(&url),
            domain: urls::registrable_domain(&url),
            url,
            score: 10.0,
            depth: 0,
            from_page_id: None,
            attempts: 0,
        };
    }

    fn page_visit(url: &str, html: &str) -> Visit {
        let url = Url::parse(url).unwrap();
        return Visit {
            requested: url.clone(),
            final_url: url.clone(),
            redirects: Vec::new(),
            outcome: Outcome::Page {
                status: reqwest::StatusCode::OK,
                parsed: Box::new(parse_html(&url, html)),
                bytes_wire: html.len() as u64,
                bytes_body: html.len() as u64,
                content_hash: blake3::hash(html.as_bytes()).to_hex()[..32].to_string(),
                elapsed: Duration::ZERO,
            },
        };
    }

    fn job(url: &str) -> Job {
        return Job {
            url: url.into(),
            title: "Engineer".into(),
            source: "ats:lever".into(),
            ..Job::default()
        };
    }

    fn lever(token: &str) -> Board {
        return Board {
            vendor: Vendor::Lever,
            token: token.into(),
            host: "jobs.lever.co".into(),
        };
    }

    async fn open_jobs(pool: &SqlitePool) -> Vec<(String, Option<i64>)> {
        return sqlx::query_as(
            "SELECT url, domain_id FROM jobs WHERE closed_at IS NULL ORDER BY url",
        )
        .fetch_all(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn board_listing_upserts_and_closes_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        let board = lever("acme");
        let listing = |urls: &[&str]| -> Result<BoardJobs, String> {
            return Ok(BoardJobs {
                company: Some("Acme".into()),
                jobs: urls.iter().map(|u| job(u)).collect(),
            });
        };

        let events = record_board(
            &pool,
            &item("https://jobs.lever.co/acme"),
            &board,
            &listing(&["https://j/1", "https://j/2"]),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                &events[0],
                Event::JobsFound {
                    total: 2,
                    new: 2,
                    closed: 0,
                    ..
                }
            ),
            "{events:?}"
        );

        let events = record_board(
            &pool,
            &item("https://jobs.lever.co/acme/1"),
            &board,
            &listing(&["https://j/2", "https://j/3"]),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                &events[0],
                Event::JobsFound {
                    total: 2,
                    new: 1,
                    closed: 1,
                    ..
                }
            ),
            "{events:?}"
        );
        assert_eq!(
            open_jobs(&pool).await,
            [("https://j/2".into(), None), ("https://j/3".into(), None)]
        );

        let (status, count, name): (String, i64, String) = sqlx::query_as(
            "SELECT last_status, job_count, name FROM boards WHERE key = 'lever/acme'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), count, name.as_str()), ("ok", 2, "Acme"));
    }

    #[tokio::test]
    async fn missing_board_is_recorded_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        let events = record_board(
            &pool,
            &item("https://jobs.lever.co/gone"),
            &lever("gone"),
            &Err("http_404".into()),
        )
        .await
        .unwrap();
        assert!(
            matches!(&events[0], Event::FetchFailed { reason, .. } if reason == "board_not_found")
        );
        let status: String = sqlx::query_scalar("SELECT last_status FROM boards")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "not_found");
    }

    #[tokio::test]
    async fn attributing_a_board_assigns_its_jobs_and_posting_links_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        // The board is harvested first, from some portfolio page: no company yet.
        record_board(
            &pool,
            &item("https://jobs.lever.co/acme"),
            &lever("acme"),
            &Ok(BoardJobs {
                company: None,
                jobs: vec![job("https://jobs.lever.co/acme/1")],
            }),
        )
        .await
        .unwrap();
        assert_eq!(open_jobs(&pool).await[0].1, None);

        // Then acme.com's homepage links a posting on it.
        let home = format!(
            "<a href='https://jobs.lever.co/acme/2'>Careers</a><p>{}</p><footer>© Acme Ltd</footer>",
            "We make things. ".repeat(30)
        );
        let budgets = Budgets::new(3, 30);
        record(
            &pool,
            &item("https://acme.com/"),
            &page_visit("https://acme.com/", &home),
            &POLICY,
            &budgets,
        )
        .await
        .unwrap();

        let acme: i64 = sqlx::query_scalar("SELECT id FROM domains WHERE host = 'acme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            open_jobs(&pool).await[0].1,
            Some(acme),
            "earlier board jobs now belong to acme.com"
        );
        let queued: Vec<String> =
            sqlx::query_scalar("SELECT url FROM frontier WHERE state = 'queued' ORDER BY url")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            queued,
            ["https://jobs.lever.co/acme"],
            "the posting link was queued as its board"
        );
    }

    #[tokio::test]
    async fn json_ld_postings_become_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        let html = r#"<script type="application/ld+json">{"@type":"JobPosting","title":"Backend Engineer",
            "jobLocation":{"address":{"addressLocality":"Lagos","addressCountry":"NG"}}}</script>"#;
        let url = "https://paystack.com/careers/backend-engineer";
        let recorded = record(
            &pool,
            &item(url),
            &page_visit(url, html),
            &POLICY,
            &Budgets::new(3, 30),
        )
        .await
        .unwrap();
        assert!(recorded.events.iter().any(|e| matches!(
            e,
            Event::JobsFound {
                total: 1,
                new: 1,
                ..
            }
        )));
        let (title, location, country, kind): (String, String, String, String) = sqlx::query_as(
            "SELECT j.title, j.location, j.country_code, p.kind FROM jobs j JOIN pages p ON p.url = j.url",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (
                title.as_str(),
                location.as_str(),
                country.as_str(),
                kind.as_str()
            ),
            ("Backend Engineer", "Lagos, NG", "NG", "job")
        );
    }

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        return (dir, pool);
    }

    /// A real homepage with a few company signals but not enough: the heuristics say "probing".
    fn gray_home(extra: &str) -> String {
        return format!(
            "<title>Acme</title><a href='/about'>About</a><a href='/contact'>Contact</a>\
             <a href='/privacy'>Privacy</a><a href='/a'>A</a><a href='/b'>B</a><p>{}</p>{extra}\
             <footer>© 2026 Jane</footer>",
            "Notes and thoughts. ".repeat(20)
        );
    }

    fn verdict(is_company: bool, confidence: f64) -> DomainVerdict {
        return DomainVerdict {
            is_company,
            company_name: Some(" Acme Freight ".into()),
            industry: Some("Logistics".into()),
            hq_country: Some("ng".into()),
            confidence,
            reason: "sells freight".into(),
        };
    }

    async fn gray_domain(pool: &SqlitePool, budgets: &Budgets) -> ClassifyRequest {
        let recorded = record(
            pool,
            &item("https://acme.example/"),
            &page_visit("https://acme.example/", &gray_home("")),
            &POLICY,
            budgets,
        )
        .await
        .unwrap();
        return recorded
            .classify
            .expect("a gray-zone homepage asks for a verdict");
    }

    async fn domain_state(
        pool: &SqlitePool,
    ) -> (String, Option<String>, Option<String>, Option<String>, bool) {
        return sqlx::query_as(
            "SELECT status, name, industry, hq_country, llm_checked_at IS NOT NULL FROM domains WHERE host = 'acme.example'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn only_a_conclusive_homepage_in_the_gray_zone_asks_the_llm() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let request = gray_domain(&pool, &budgets).await;
        assert_eq!(request.domain, "acme.example");
        assert!(request.score >= NOT_COMPANY_THRESHOLD && request.score < COMPANY_THRESHOLD);

        // A deep page of another gray domain: not the homepage, so nothing to ask.
        let recorded = record(
            &pool,
            &item("https://other.example/blog/post"),
            &page_visit("https://other.example/blog/post", &gray_home("")),
            &POLICY,
            &budgets,
        )
        .await
        .unwrap();
        assert!(recorded.classify.is_none());

        // A homepage that clearly is a company (score above the threshold) needs no help.
        let recorded = record(
            &pool,
            &item("https://sure.example/"),
            &page_visit(
                "https://sure.example/",
                &format!(
                    "<title>Sure Ltd</title><a href='/careers'>Careers</a><a href='/about'>About</a>\
                     <a href='/privacy'>Privacy</a><a href='/contact'>Contact</a><p>{}</p><footer>© 2026 Sure Ltd</footer>",
                    "We build software. ".repeat(30)
                ),
            ),
            &POLICY,
            &budgets,
        )
        .await
        .unwrap();
        assert!(recorded.classify.is_none());
    }

    #[tokio::test]
    async fn a_confident_company_verdict_promotes_the_domain_and_starts_the_careers_probes() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let request = gray_domain(&pool, &budgets).await;
        assert_eq!(budgets.status("acme.example"), DomainStatus::Probing);
        let queued_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE reason = 'probe_careers'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            queued_before, 0,
            "a probing domain doesn't probe for careers"
        );

        let recorded = record_verdict(&pool, &request, &verdict(true, 0.9), &POLICY, &budgets)
            .await
            .unwrap();
        let (status, name, industry, country, checked) = domain_state(&pool).await;
        assert_eq!(status, "company");
        assert_eq!(name.as_deref(), Some("Acme Freight"), "the name is trimmed");
        assert_eq!(
            (industry.as_deref(), country.as_deref()),
            (Some("logistics"), Some("NG"))
        );
        assert!(checked);
        assert_eq!(budgets.status("acme.example"), DomainStatus::Company);
        assert!(matches!(
            recorded.events.as_slice(),
            [Event::DomainClassified { status, previous, .. }] if status == "company" && previous == "probing"
        ));
        let probes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE reason = 'probe_careers'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            probes > 0,
            "the careers probes run as if the heuristics had been sure"
        );
        assert!(recorded.sitemap_scan.is_some());
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 'domain_classified' AND json_extract(payload, '$.status') = 'company'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[tokio::test]
    async fn a_confident_not_company_verdict_demotes_the_domain() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let request = gray_domain(&pool, &budgets).await;
        let recorded = record_verdict(&pool, &request, &verdict(false, 0.85), &POLICY, &budgets)
            .await
            .unwrap();
        assert_eq!(domain_state(&pool).await.0, "not_company");
        assert_eq!(budgets.status("acme.example"), DomainStatus::NotCompany);
        assert_eq!(recorded.events.len(), 1);
        assert!(recorded.sitemap_scan.is_none());
        let probes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE reason = 'probe_careers'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(probes, 0);
    }

    #[tokio::test]
    async fn an_unsure_verdict_keeps_the_notes_but_not_the_status_and_is_not_asked_twice() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let request = gray_domain(&pool, &budgets).await;
        let recorded = record_verdict(&pool, &request, &verdict(true, 0.5), &POLICY, &budgets)
            .await
            .unwrap();
        assert!(recorded.events.is_empty());
        let (status, _, industry, _, checked) = domain_state(&pool).await;
        assert_eq!(
            (status.as_str(), industry.as_deref(), checked),
            ("probing", Some("logistics"), true)
        );

        // Another main-homepage visit (different content, so not a duplicate) doesn't ask again.
        let recorded = record(
            &pool,
            &item("https://www.acme.example/"),
            &page_visit(
                "https://www.acme.example/",
                &gray_home("<p>New announcement.</p>"),
            ),
            &POLICY,
            &budgets,
        )
        .await
        .unwrap();
        assert!(recorded.classify.is_none());
    }

    #[test]
    fn verdicts_are_sanitized() {
        let v = sanitize_verdict(&DomainVerdict {
            is_company: true,
            company_name: Some("   ".into()),
            industry: Some("A".repeat(100)),
            hq_country: Some("Nigeria".into()),
            confidence: f64::NAN,
            reason: "x".repeat(1000),
        });
        assert_eq!(v.company_name, None);
        assert_eq!(v.industry.as_deref().map(str::len), Some(40));
        assert_eq!(v.hq_country, None);
        assert_eq!(v.confidence, 0.0);
        assert_eq!(v.reason.len(), 300);
        assert_eq!(
            sanitize_verdict(&DomainVerdict {
                confidence: 7.0,
                ..verdict(true, 0.0)
            })
            .confidence,
            1.0
        );
    }

    async fn queued(pool: &SqlitePool, url: &str) -> Option<(f64, f64, String)> {
        return sqlx::query_as("SELECT score, profile_boost, reason FROM frontier WHERE url = ?")
            .bind(url)
            .fetch_optional(pool)
            .await
            .unwrap();
    }

    fn fintech_profile() -> areer_core::profile::Profile {
        return areer_core::profile::Profile {
            industries: vec!["fintech".into()],
            ..Default::default()
        };
    }

    const LINKS: &str = "<title>Directory</title><a href='https://other.example/lists/fintech'>Top fintech companies</a>\
                         <a href='https://third.example/lists/recipes'>Top recipes</a>";

    #[tokio::test]
    async fn profile_topics_raise_the_score_of_matching_links_and_are_remembered() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let plain = LinkPolicy::new(5, 1.0);
        record(
            &pool,
            &item("https://dir.example/"),
            &page_visit("https://dir.example/", LINKS),
            &plain,
            &budgets,
        )
        .await
        .unwrap();
        let (base, boost, _) = queued(&pool, "https://other.example/lists/fintech")
            .await
            .unwrap();
        assert_eq!(boost, 0.0);

        let (_dir2, pool2) = test_pool().await;
        let steered = LinkPolicy::new(5, 1.0);
        steered.profile.set(1, 1, fintech_profile());
        record(
            &pool2,
            &item("https://dir.example/"),
            &page_visit("https://dir.example/", LINKS),
            &steered,
            &budgets,
        )
        .await
        .unwrap();
        let (score, boost, reason) = queued(&pool2, "https://other.example/lists/fintech")
            .await
            .unwrap();
        assert!(
            boost > 0.0 && (score - base - boost).abs() < 1e-9,
            "{score} vs {base} + {boost}"
        );
        assert!(reason.contains("profile_topic"), "{reason}");
        let (recipes, recipes_boost, _) = queued(&pool2, "https://third.example/lists/recipes")
            .await
            .unwrap();
        assert_eq!(recipes_boost, 0.0);
        assert!(score > recipes);
    }

    #[tokio::test]
    async fn a_source_that_yielded_good_jobs_gets_its_links_boosted() {
        let (_dir, pool) = test_pool().await;
        let budgets = Budgets::new(3, 30);
        let policy = LinkPolicy::new(5, 1.0);
        let profile = areer_core::profile::Profile {
            titles: vec!["Backend Engineer".into()],
            ..Default::default()
        };
        let pid = areer_core::profile::insert(&pool, "p", "parser", "h", "t", &profile)
            .await
            .unwrap();
        policy.profile.set(pid, 1, profile);
        let domain_id: i64 = sqlx::query_scalar(
            "INSERT INTO domains (host, first_seen) VALUES ('dir.example', 1) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (url, domain_id, title, source, first_seen, last_seen) VALUES ('https://j/1', ?, 'Backend Engineer', 'jsonld', 1, 1) RETURNING id",
        )
        .bind(domain_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO job_matches (profile_id, job_id, score, computed_at) VALUES (?, ?, 0.9, 1)")
            .bind(pid)
            .bind(job_id)
            .execute(&pool)
            .await
            .unwrap();

        record(
            &pool,
            &item("https://dir.example/"),
            &page_visit("https://dir.example/", LINKS),
            &policy,
            &budgets,
        )
        .await
        .unwrap();
        let (with_yield, _, reason) = queued(&pool, "https://third.example/lists/recipes")
            .await
            .unwrap();
        assert!(reason.contains("profile_yield"), "{reason}");

        let (_d2, pool2) = test_pool().await;
        record(
            &pool2,
            &item("https://dir.example/"),
            &page_visit("https://dir.example/", LINKS),
            &LinkPolicy::new(5, 1.0),
            &budgets,
        )
        .await
        .unwrap();
        let (without, _, _) = queued(&pool2, "https://third.example/lists/recipes")
            .await
            .unwrap();
        assert!((with_yield - without - crate::steer::yield_points(1)).abs() < 1e-9);
    }
}
