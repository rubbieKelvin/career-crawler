//! Read-side queries for the UI. The UI only reads crawl data; its only writes are
//! control commands.

use career_core::samples::{self, Rates, Sample};
use career_core::time::now_ms;
use serde::Serialize;
use sqlx::SqlitePool;

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
    pub nodes: Vec<Node>,
    /// Only edges between returned nodes.
    pub edges: Vec<Edge>,
    /// All domains, including ones cut by `limit`.
    pub total_domains: i64,
}

/// The domain graph, capped to the `limit` most significant nodes (pages, open jobs and
/// links), so the UI stays responsive on large crawls. `discovered` domains (linked but
/// never fetched) can be left out.
pub async fn graph(
    pool: &SqlitePool,
    limit: i64,
    include_discovered: bool,
) -> anyhow::Result<Graph> {
    let nodes: Vec<Node> = sqlx::query_as(
        "WITH pc AS (SELECT domain_id, COUNT(*) AS n FROM pages GROUP BY domain_id),
              jc AS (SELECT domain_id, COUNT(*) AS n FROM jobs
                     WHERE closed_at IS NULL AND domain_id IS NOT NULL GROUP BY domain_id),
              dg AS (SELECT id, SUM(n) AS n FROM (
                       SELECT src_domain_id AS id, COUNT(*) AS n FROM edges GROUP BY src_domain_id
                       UNION ALL
                       SELECT dst_domain_id, COUNT(*) FROM edges GROUP BY dst_domain_id)
                     GROUP BY id)
         SELECT d.id, d.host, d.name, d.status, d.company_score, d.careers_url, d.ats,
                COALESCE(pc.n, 0) AS pages, COALESCE(jc.n, 0) AS jobs, COALESCE(dg.n, 0) AS degree
         FROM domains d
         LEFT JOIN pc ON pc.domain_id = d.id
         LEFT JOIN jc ON jc.domain_id = d.id
         LEFT JOIN dg ON dg.id = d.id
         WHERE ? OR d.status <> 'discovered'
         ORDER BY COALESCE(pc.n, 0) * 3 + COALESCE(jc.n, 0) + COALESCE(dg.n, 0) DESC, d.id
         LIMIT ?",
    )
    .bind(include_discovered)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let ids = serde_json::to_string(&nodes.iter().map(|n| n.id).collect::<Vec<_>>())?;
    let edges: Vec<Edge> = sqlx::query_as(
        "SELECT src_domain_id AS source, dst_domain_id AS target, weight FROM edges
         WHERE src_domain_id IN (SELECT value FROM json_each(?1))
           AND dst_domain_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let total_domains = sqlx::query_scalar("SELECT COUNT(*) FROM domains")
        .fetch_one(pool)
        .await?;
    return Ok(Graph {
        nodes,
        edges,
        total_domains,
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
    let (open_jobs, inbound, outbound): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM jobs WHERE domain_id = ?1 AND closed_at IS NULL),
                (SELECT COUNT(*) FROM edges WHERE dst_domain_id = ?1),
                (SELECT COUNT(*) FROM edges WHERE src_domain_id = ?1)",
    )
    .bind(domain.id)
    .fetch_one(pool)
    .await?;
    return Ok(Some(DomainDetail {
        domain,
        pages,
        jobs,
        open_jobs,
        boards,
        inbound,
        outbound,
    }));
}
