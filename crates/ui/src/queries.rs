//! Read-side queries for the UI. The UI only reads crawl data; its only writes are
//! control commands.

use career_core::samples::{self, Rates, Sample};
use std::collections::{HashMap, HashSet};

use career_core::time::now_ms;
use career_core::urls;
use serde::Serialize;
use sqlx::SqlitePool;
use url::Url;

/// A crawler that hasn't written a metrics sample for this long is considered gone, even
/// without a `crawler_stopped` event (it crashed or was killed).
const CRAWLER_STALE_MS: i64 = 15_000;

#[derive(Debug, Serialize, PartialEq)]
pub struct CrawlerStatus {
    pub running: bool,
    pub paused: bool,
    /// Id of the current or last run's `crawler_started` event.
    pub run_id: Option<i64>,
    pub started_at: Option<i64>,
    pub last_sample_at: Option<i64>,
}

pub async fn crawler_status(pool: &SqlitePool) -> anyhow::Result<CrawlerStatus> {
    let (started, started_at, stopped, last_sample_at): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT (SELECT MAX(id) FROM events WHERE kind = 'crawler_started'),
                    (SELECT ts FROM events WHERE kind = 'crawler_started' ORDER BY id DESC LIMIT 1),
                    (SELECT MAX(id) FROM events WHERE kind = 'crawler_stopped'),
                    (SELECT MAX(ts) FROM metrics_samples)",
    )
    .fetch_one(pool)
    .await?;
    let fresh = last_sample_at.is_some_and(|ts| ts > now_ms() - CRAWLER_STALE_MS);
    let running = started.is_some_and(|s| s > stopped.unwrap_or(0)) && fresh;
    let last_pause_command: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(payload, '$.command') FROM events
         WHERE kind = 'control_applied' AND id > ? AND json_extract(payload, '$.command') IN ('pause', 'resume')
         ORDER BY id DESC LIMIT 1",
    )
    .bind(started.unwrap_or(0))
    .fetch_optional(pool)
    .await?;
    return Ok(CrawlerStatus {
        running,
        paused: running && last_pause_command.as_deref() == Some("pause"),
        run_id: started,
        started_at,
        last_sample_at,
    });
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Counts {
    pub domains: i64,
    pub companies: i64,
    pub pages: i64,
    pub frontier_queued: i64,
    pub jobs: i64,
    pub open_jobs: i64,
    pub boards: i64,
    pub events: i64,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub counts: Counts,
    pub crawler: CrawlerStatus,
    pub metrics: LatestMetrics,
}

#[derive(Debug, Serialize)]
pub struct LatestMetrics {
    pub latest: Option<Sample>,
    pub rates: Option<Rates>,
}

pub async fn latest_metrics(pool: &SqlitePool) -> anyhow::Result<LatestMetrics> {
    let recent = samples::latest(pool, 2).await?;
    let rates = match recent.as_slice() {
        [current, previous] => samples::rates(previous, current),
        _ => None,
    };
    return Ok(LatestMetrics {
        latest: recent.into_iter().next(),
        rates,
    });
}

pub async fn stats(pool: &SqlitePool) -> anyhow::Result<Stats> {
    let db = career_core::db::stats(pool).await?;
    let (open_jobs, boards): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM jobs WHERE closed_at IS NULL), (SELECT COUNT(*) FROM boards)",
    )
    .fetch_one(pool)
    .await?;
    return Ok(Stats {
        counts: Counts {
            domains: db.domains,
            companies: db.companies,
            pages: db.pages,
            frontier_queued: db.frontier_queued,
            jobs: db.jobs,
            open_jobs,
            boards,
            events: db.events,
        },
        crawler: crawler_status(pool).await?,
        metrics: latest_metrics(pool).await?,
    });
}

#[derive(Debug, Serialize, PartialEq, sqlx::FromRow)]
pub struct Node {
    pub id: i64,
    pub host: String,
    pub name: Option<String>,
    pub status: String,
    pub company_score: Option<f64>,
    pub careers_url: Option<String>,
    pub ats: Option<String>,
    pub pages: i64,
    pub jobs: i64,
    pub degree: i64,
}

#[derive(Debug, Serialize, PartialEq, sqlx::FromRow)]
pub struct Edge {
    pub source: i64,
    pub target: i64,
    pub weight: i64,
}

#[derive(Debug, Serialize)]
pub struct Graph {
    /// The moment this graph describes (ms): `at`, or now.
    pub at: i64,
    pub nodes: Vec<Node>,
    /// Only edges between returned nodes.
    pub edges: Vec<Edge>,
    /// All domains that existed at `at`, including ones cut by `limit`.
    pub total_domains: i64,
    pub counts: GraphCounts,
}

#[derive(Debug, Serialize, PartialEq, sqlx::FromRow)]
pub struct GraphCounts {
    pub domains: i64,
    pub companies: i64,
    pub pages: i64,
    pub open_jobs: i64,
    pub boards: i64,
}

/// Each domain's latest classification at or before `?1`, for replay. (Live mode reads
/// `domains.status`, the authoritative current value.)
const STATUS_AT: &str = "cls AS (
       SELECT json_extract(payload, '$.domain') AS host, json_extract(payload, '$.status') AS status
       FROM events
       WHERE id IN (SELECT MAX(id) FROM events WHERE kind = 'domain_classified' AND ts <= ?1
                    GROUP BY json_extract(payload, '$.domain')))";

/// The domain graph as it was at `at` (ms; `None` = now), capped to the `limit` most
/// significant nodes (pages, open jobs, links) so the UI stays responsive.
/// `discovered` domains (linked but never fetched) can be left out.
///
/// History is rebuilt from stored timestamps: domains and edges by `first_seen`, pages by
/// `fetched_at`, jobs open between `first_seen` and `closed_at`, and status from
/// classification events. Careers URL, ATS and edge weights are current values.
pub async fn graph(
    pool: &SqlitePool,
    limit: i64,
    include_discovered: bool,
    at: Option<i64>,
) -> anyhow::Result<Graph> {
    let t = at.unwrap_or(i64::MAX);
    let nodes: Vec<Node> = sqlx::query_as(&format!(
        "WITH {STATUS_AT},
              pc AS (SELECT domain_id, COUNT(*) AS n FROM pages WHERE fetched_at <= ?1 GROUP BY domain_id),
              jc AS (SELECT domain_id, COUNT(*) AS n FROM jobs
                     WHERE domain_id IS NOT NULL AND first_seen <= ?1 AND (closed_at IS NULL OR closed_at > ?1)
                     GROUP BY domain_id),
              e AS (SELECT src_domain_id, dst_domain_id FROM edges WHERE first_seen <= ?1),
              dg AS (SELECT id, SUM(n) AS n FROM (
                       SELECT src_domain_id AS id, COUNT(*) AS n FROM e GROUP BY src_domain_id
                       UNION ALL
                       SELECT dst_domain_id, COUNT(*) FROM e GROUP BY dst_domain_id)
                     GROUP BY id)
         SELECT d.id, d.host, d.name,
                CASE WHEN ?4 THEN d.status ELSE COALESCE(cls.status, 'discovered') END AS status, d.company_score,
                d.careers_url, d.ats,
                COALESCE(pc.n, 0) AS pages, COALESCE(jc.n, 0) AS jobs, COALESCE(dg.n, 0) AS degree
         FROM domains d
         LEFT JOIN cls ON cls.host = d.host
         LEFT JOIN pc ON pc.domain_id = d.id
         LEFT JOIN jc ON jc.domain_id = d.id
         LEFT JOIN dg ON dg.id = d.id
         WHERE d.first_seen <= ?1
           AND (?2 OR (CASE WHEN ?4 THEN d.status ELSE COALESCE(cls.status, 'discovered') END) <> 'discovered')
         ORDER BY COALESCE(pc.n, 0) * 3 + COALESCE(jc.n, 0) + COALESCE(dg.n, 0) DESC, d.id
         LIMIT ?3"
    ))
    .bind(t)
    .bind(include_discovered)
    .bind(limit)
    .bind(at.is_none())
    .fetch_all(pool)
    .await?;
    let ids = serde_json::to_string(&nodes.iter().map(|n| n.id).collect::<Vec<_>>())?;
    let edges: Vec<Edge> = sqlx::query_as(
        "SELECT src_domain_id AS source, dst_domain_id AS target, weight FROM edges
         WHERE first_seen <= ?2
           AND src_domain_id IN (SELECT value FROM json_each(?1))
           AND dst_domain_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(&ids)
    .bind(t)
    .fetch_all(pool)
    .await?;
    let counts: GraphCounts = sqlx::query_as(&format!(
        "WITH {STATUS_AT}
         SELECT (SELECT COUNT(*) FROM domains WHERE first_seen <= ?1) AS domains,
                CASE WHEN ?2 THEN (SELECT COUNT(*) FROM domains WHERE status = 'company')
                     ELSE (SELECT COUNT(*) FROM cls WHERE status = 'company') END AS companies,
                (SELECT COUNT(*) FROM pages WHERE fetched_at <= ?1) AS pages,
                (SELECT COUNT(*) FROM jobs WHERE first_seen <= ?1 AND (closed_at IS NULL OR closed_at > ?1)) AS open_jobs,
                (SELECT COUNT(*) FROM boards WHERE first_seen <= ?1) AS boards"
    ))
    .bind(t)
    .bind(at.is_none())
    .fetch_one(pool)
    .await?;
    return Ok(Graph {
        at: at.unwrap_or_else(now_ms),
        nodes,
        edges,
        total_domains: counts.domains,
        counts,
    });
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Run {
    /// Id of the run's `crawler_started` event.
    pub id: i64,
    pub started_at: i64,
    /// `None` while running, or if the crawler died without a `crawler_stopped` event.
    pub stopped_at: Option<i64>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, sqlx::FromRow)]
pub struct Bucket {
    /// Bucket start (ms).
    pub t: i64,
    pub pages: i64,
    pub new_jobs: i64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct History {
    /// First event, or `None` for an empty database.
    pub start: Option<i64>,
    pub end: i64,
    pub runs: Vec<Run>,
    pub bucket_ms: i64,
    /// Pages fetched and new jobs per bucket; empty buckets are omitted.
    pub activity: Vec<Bucket>,
}

/// What the replay timeline shows: the time span, the crawler runs within it, and an
/// activity histogram.
pub async fn history(pool: &SqlitePool, buckets: i64) -> anyhow::Result<History> {
    let end = now_ms();
    let start: Option<i64> = sqlx::query_scalar("SELECT MIN(ts) FROM events")
        .fetch_one(pool)
        .await?;
    let marks: Vec<(i64, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, ts, kind, json_extract(payload, '$.reason') FROM events
         WHERE kind IN ('crawler_started', 'crawler_stopped') ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut runs: Vec<Run> = Vec::new();
    for (id, ts, kind, reason) in marks {
        match (kind.as_str(), runs.last_mut()) {
            ("crawler_started", _) => runs.push(Run {
                id,
                started_at: ts,
                stopped_at: None,
                reason: None,
            }),
            ("crawler_stopped", Some(run)) if run.stopped_at.is_none() => {
                run.stopped_at = Some(ts);
                run.reason = reason;
            }
            _ => {}
        }
    }
    let Some(start) = start else {
        return Ok(History {
            start: None,
            end,
            runs,
            bucket_ms: 0,
            activity: Vec::new(),
        });
    };
    let bucket_ms = ((end - start) / buckets.max(1)).max(1000);
    let activity = sqlx::query_as(
        "SELECT ?1 + ((ts - ?1) / ?2) * ?2 AS t,
                SUM(kind = 'page_fetched') AS pages,
                SUM(CASE WHEN kind = 'jobs_found' THEN json_extract(payload, '$.new') ELSE 0 END) AS new_jobs
         FROM events WHERE kind IN ('page_fetched', 'jobs_found')
         GROUP BY (ts - ?1) / ?2 ORDER BY t",
    )
    .bind(start)
    .bind(bucket_ms)
    .fetch_all(pool)
    .await?;
    return Ok(History {
        start: Some(start),
        end,
        runs,
        bucket_ms,
        activity,
    });
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DomainRow {
    pub id: i64,
    pub host: String,
    pub name: Option<String>,
    pub status: String,
    pub company_score: Option<f64>,
    /// Stored as JSON text; sent as JSON.
    #[serde(serialize_with = "raw_json")]
    pub score_reasons: Option<String>,
    pub careers_url: Option<String>,
    pub ats: Option<String>,
    pub ats_token: Option<String>,
    pub first_seen: i64,
    pub last_crawled: Option<i64>,
}

fn raw_json<S: serde::Serializer>(text: &Option<String>, serializer: S) -> Result<S::Ok, S::Error> {
    let value = text
        .as_deref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok());
    return serde::Serialize::serialize(&value, serializer);
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PageRow {
    pub url: String,
    pub kind: Option<String>,
    pub http_status: Option<i64>,
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct JobRow {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub location: Option<String>,
    pub remote_mode: Option<String>,
    pub department: Option<String>,
    pub salary_min: Option<f64>,
    pub salary_max: Option<f64>,
    pub salary_currency: Option<String>,
    pub salary_period: Option<String>,
    pub posted_at: Option<i64>,
    pub source: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BoardRow {
    pub key: String,
    pub name: Option<String>,
    pub last_status: Option<String>,
    pub last_fetched: Option<i64>,
    pub job_count: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct DomainDetail {
    pub domain: DomainRow,
    pub pages: Vec<PageRow>,
    pub page_count: i64,
    pub jobs: Vec<JobRow>,
    pub open_jobs: i64,
    pub boards: Vec<BoardRow>,
    pub inbound: i64,
    pub outbound: i64,
}

/// Up to this many pages and jobs are listed in a domain's detail.
const DETAIL_ROWS: i64 = 100;

pub async fn domain_detail(pool: &SqlitePool, host: &str) -> anyhow::Result<Option<DomainDetail>> {
    let Some(domain): Option<DomainRow> = sqlx::query_as(
        "SELECT id, host, name, status, company_score, score_reasons, careers_url, ats, ats_token,
                first_seen, last_crawled
         FROM domains WHERE host = ?",
    )
    .bind(host)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let pages = sqlx::query_as(
        "SELECT url, kind, http_status, fetched_at, error FROM pages WHERE domain_id = ?
         ORDER BY kind = 'careers' DESC, kind = 'home' DESC, id LIMIT ?",
    )
    .bind(domain.id)
    .bind(DETAIL_ROWS)
    .fetch_all(pool)
    .await?;
    let jobs = sqlx::query_as(
        "SELECT id, url, title, location, remote_mode, department, salary_min, salary_max, salary_currency,
                salary_period, posted_at, source
         FROM jobs WHERE domain_id = ? AND closed_at IS NULL ORDER BY posted_at DESC NULLS LAST, id LIMIT ?",
    )
    .bind(domain.id)
    .bind(DETAIL_ROWS)
    .fetch_all(pool)
    .await?;
    let boards = sqlx::query_as(
        "SELECT key, name, last_status, last_fetched, job_count FROM boards WHERE domain_id = ? ORDER BY key",
    )
    .bind(domain.id)
    .fetch_all(pool)
    .await?;
    let (page_count, open_jobs, inbound, outbound): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM pages WHERE domain_id = ?1),
                (SELECT COUNT(*) FROM jobs WHERE domain_id = ?1 AND closed_at IS NULL),
                (SELECT COUNT(*) FROM edges WHERE dst_domain_id = ?1),
                (SELECT COUNT(*) FROM edges WHERE src_domain_id = ?1)",
    )
    .bind(domain.id)
    .fetch_one(pool)
    .await?;
    return Ok(Some(DomainDetail {
        domain,
        pages,
        page_count,
        jobs,
        open_jobs,
        boards,
        inbound,
        outbound,
    }));
}

/// Most pages drawn in a domain's page graph; the rest are counted in `hidden_pages`.
const PAGE_GRAPH_PAGES: i64 = 500;
/// Internal links not fetched yet, drawn as small pending nodes.
const PAGE_GRAPH_PENDING: usize = 200;
/// External sites, collapsed to one node each, most-linked first.
const PAGE_GRAPH_EXTERNAL: usize = 40;

#[derive(Debug, Serialize, PartialEq)]
pub struct PageNode {
    /// `p:<page id>`, `u:<url>` (pending) or `d:<host>` (external site).
    pub id: String,
    /// A page kind (`home`, `careers`, `job`, `duplicate`, `other`), `failed` for pages
    /// with an error, `pending` for unfetched internal links, or `external`.
    pub kind: String,
    pub label: String,
    /// Page or link URL; for external nodes, the site's host.
    pub url: String,
    pub http_status: Option<i64>,
    pub error: Option<String>,
    /// Frontier state of a pending link (`queued`, `deferred`, `skipped`, …).
    pub state: Option<String>,
    /// Links from this domain's pages to an external site.
    pub links: i64,
}

#[derive(sqlx::FromRow)]
struct PageGraphRow {
    id: i64,
    url: String,
    kind: Option<String>,
    http_status: Option<i64>,
    error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PageEdge {
    pub source: String,
    pub target: String,
    pub weight: i64,
}

#[derive(Debug, Serialize)]
pub struct PageGraph {
    pub host: String,
    pub nodes: Vec<PageNode>,
    pub edges: Vec<PageEdge>,
    /// Fetched pages beyond the drawing cap.
    pub hidden_pages: i64,
}

/// One domain's pages and the links between them (from `page_links`). Unfetched internal
/// link targets become pending nodes; links out of the domain collapse into one node per
/// external site.
pub async fn page_graph(pool: &SqlitePool, host: &str) -> anyhow::Result<Option<PageGraph>> {
    let Some(domain_id): Option<i64> = sqlx::query_scalar("SELECT id FROM domains WHERE host = ?")
        .bind(host)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let pages: Vec<PageGraphRow> = sqlx::query_as(
        "SELECT id, url, kind, http_status, error FROM pages WHERE domain_id = ?
         ORDER BY kind = 'home' DESC, kind = 'careers' DESC, kind = 'job' DESC, id LIMIT ?",
    )
    .bind(domain_id)
    .bind(PAGE_GRAPH_PAGES)
    .fetch_all(pool)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pages WHERE domain_id = ?")
        .bind(domain_id)
        .fetch_one(pool)
        .await?;

    let mut nodes = Vec::new();
    let mut page_ids = HashMap::new();
    for page in &pages {
        page_ids.insert(page.url.clone(), format!("p:{}", page.id));
        let kind = if page.error.is_some() {
            "failed".to_string()
        } else {
            page.kind.clone().unwrap_or_else(|| "other".into())
        };
        nodes.push(PageNode {
            id: format!("p:{}", page.id),
            label: page_label(&page.url),
            kind,
            url: page.url.clone(),
            http_status: page.http_status,
            error: page.error.clone(),
            state: None,
            links: 0,
        });
    }

    let src_ids = serde_json::to_string(&pages.iter().map(|p| p.id).collect::<Vec<_>>())?;
    let links: Vec<(i64, String)> = sqlx::query_as(
        "SELECT src_page_id, dst_url FROM page_links WHERE src_page_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&src_ids)
    .fetch_all(pool)
    .await?;

    let mut edges: HashMap<(String, String), i64> = HashMap::new();
    let mut pending: Vec<String> = Vec::new();
    let mut external: HashMap<String, i64> = HashMap::new();
    for (src, dst) in &links {
        let source = format!("p:{src}");
        let Ok(dst_url) = Url::parse(dst) else {
            continue;
        };
        let dst_host =
            urls::registrable_domain(&dst_url).unwrap_or_else(|| urls::host_key(&dst_url));
        let target = if dst_host != host {
            *external.entry(dst_host.clone()).or_default() += 1;
            format!("d:{dst_host}")
        } else if let Some(page) = page_ids.get(dst) {
            page.clone()
        } else {
            if !pending.contains(dst) {
                if pending.len() >= PAGE_GRAPH_PENDING {
                    continue;
                }
                pending.push(dst.clone());
            }
            format!("u:{dst}")
        };
        if target != source {
            *edges.entry((source, target)).or_default() += 1;
        }
    }

    let mut sites: Vec<(String, i64)> = external.into_iter().collect();
    sites.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sites.truncate(PAGE_GRAPH_EXTERNAL);
    for (site, links) in &sites {
        nodes.push(PageNode {
            id: format!("d:{site}"),
            kind: "external".into(),
            label: site.clone(),
            url: site.clone(),
            http_status: None,
            error: None,
            state: None,
            links: *links,
        });
    }

    let states: HashMap<String, String> = sqlx::query_as(
        "SELECT url, state FROM frontier WHERE url IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&pending)?)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    for url in &pending {
        nodes.push(PageNode {
            id: format!("u:{url}"),
            kind: "pending".into(),
            label: page_label(url),
            url: url.clone(),
            http_status: None,
            error: None,
            state: Some(
                states
                    .get(url)
                    .cloned()
                    .unwrap_or_else(|| "not queued".into()),
            ),
            links: 0,
        });
    }

    let drawn: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut edges: Vec<PageEdge> = edges
        .into_iter()
        .filter(|((s, t), _)| drawn.contains(s.as_str()) && drawn.contains(t.as_str()))
        .map(|((source, target), weight)| PageEdge {
            source,
            target,
            weight,
        })
        .collect();
    edges.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
    return Ok(Some(PageGraph {
        host: host.to_string(),
        nodes,
        edges,
        hidden_pages: (total - pages.len() as i64).max(0),
    }));
}

/// A short label for a page: its path (with query), or the host for the root.
fn page_label(url: &str) -> String {
    let Ok(url) = Url::parse(url) else {
        return url.to_string();
    };
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    if path == "/" {
        return url.host_str().unwrap_or_default().to_string();
    }
    return path;
}
