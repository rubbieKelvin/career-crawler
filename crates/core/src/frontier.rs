//! The crawl frontier: a priority queue of URLs persisted in the `frontier` table, so a
//! crawl survives restarts. Scoring lives in the crawler; this module only stores and
//! selects URLs.

use sqlx::{SqliteConnection, SqliteExecutor, SqlitePool};
use url::Url;

use crate::time::now_ms;
use crate::urls;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    InFlight,
    Done,
    /// Never fetched on purpose: robots.txt, domain budget, …
    Skipped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        return match self {
            State::Queued => "queued",
            State::InFlight => "in_flight",
            State::Done => "done",
            State::Skipped => "skipped",
        };
    }
}

#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub url: &'a Url,
    pub score: f64,
    pub depth: u32,
    pub from_page_id: Option<i64>,
    pub reason: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub url: Url,
    pub score: f64,
    pub depth: u32,
    pub from_page_id: Option<i64>,
    pub host: String,
    pub domain: Option<String>,
    pub attempts: u32,
}

#[derive(sqlx::FromRow)]
struct Row {
    url: String,
    score: f64,
    depth: u32,
    from_page_id: Option<i64>,
    host: String,
    domain: Option<String>,
    attempts: u32,
}

/// Adds a URL, or raises the score of one still queued if this path to it scores higher.
/// URLs already fetched or skipped are left alone. Returns whether a row changed.
pub async fn enqueue<'e>(exec: impl SqliteExecutor<'e>, c: &Candidate<'_>) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO frontier (url, score, depth, from_page_id, reason, state, enqueued_at, host, domain)
         VALUES (?, ?, ?, ?, ?, 'queued', ?, ?, ?)
         ON CONFLICT(url) DO UPDATE SET score = excluded.score, reason = excluded.reason
         WHERE frontier.state = 'queued' AND excluded.score > frontier.score",
    )
    .bind(c.url.as_str())
    .bind(c.score)
    .bind(c.depth)
    .bind(c.from_page_id)
    .bind(c.reason)
    .bind(now_ms())
    .bind(urls::host_key(c.url))
    .bind(urls::registrable_domain(c.url))
    .execute(exec)
    .await?;
    return Ok(result.rows_affected() > 0);
}

/// The best queued URL for each host, best first, skipping `exclude_hosts` (busy or
/// cooling down). One URL per host means one big site can't crowd out everyone else.
pub async fn next_batch(
    pool: &SqlitePool,
    exclude_hosts: &[String],
    limit: usize,
) -> anyhow::Result<Vec<Item>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT url, score, depth, from_page_id, host, domain, attempts FROM (
             SELECT url, score, depth, from_page_id, host, domain, attempts, enqueued_at,
                    ROW_NUMBER() OVER (PARTITION BY host ORDER BY score DESC, enqueued_at) AS rn
             FROM frontier
             WHERE state = 'queued' AND host NOT IN (SELECT value FROM json_each(?))
         )
         WHERE rn = 1
         ORDER BY score DESC, enqueued_at
         LIMIT ?",
    )
    .bind(serde_json::to_string(exclude_hosts)?)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        match Url::parse(&row.url) {
            Ok(url) => items.push(Item {
                url,
                score: row.score,
                depth: row.depth,
                from_page_id: row.from_page_id,
                host: row.host,
                domain: row.domain,
                attempts: row.attempts,
            }),
            Err(e) => {
                tracing::warn!(url = %row.url, error = %e, "dropping unparseable frontier URL");
                set_state(pool, &row.url, State::Skipped).await?;
            }
        }
    }
    return Ok(items);
}

pub async fn set_state<'e>(
    exec: impl SqliteExecutor<'e>,
    url: &str,
    state: State,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE frontier SET state = ? WHERE url = ?")
        .bind(state.as_str())
        .bind(url)
        .execute(exec)
        .await?;
    return Ok(());
}

/// Puts a URL back in the queue after a transient failure, at half its score.
pub async fn retry<'e>(exec: impl SqliteExecutor<'e>, url: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE frontier SET state = 'queued', attempts = attempts + 1, score = score / 2 WHERE url = ?")
        .bind(url)
        .execute(exec)
        .await?;
    return Ok(());
}

/// Records a URL we reached without dequeuing it (a redirect target) as done, so it is
/// never fetched again on its own.
pub async fn mark_visited<'e>(
    exec: impl SqliteExecutor<'e>,
    url: &Url,
    depth: u32,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO frontier (url, score, depth, reason, state, enqueued_at, host, domain)
         VALUES (?, 0, ?, 'redirect', 'done', ?, ?, ?)
         ON CONFLICT(url) DO UPDATE SET state = 'done'",
    )
    .bind(url.as_str())
    .bind(depth)
    .bind(now_ms())
    .bind(urls::host_key(url))
    .bind(urls::registrable_domain(url))
    .execute(exec)
    .await?;
    return Ok(());
}

/// URLs left `in_flight` by a crash or kill go back in the queue. Run at crawler startup.
pub async fn reset_in_flight(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query("UPDATE frontier SET state = 'queued' WHERE state = 'in_flight'")
        .execute(pool)
        .await?;
    return Ok(result.rows_affected());
}

/// Fills `host`/`domain` for rows inserted before those columns existed.
pub async fn backfill_hosts(pool: &SqlitePool) -> anyhow::Result<u64> {
    let urls_missing: Vec<String> =
        sqlx::query_scalar("SELECT url FROM frontier WHERE host IS NULL")
            .fetch_all(pool)
            .await?;
    let mut tx = pool.begin().await?;
    for raw in &urls_missing {
        update_host(&mut tx, raw).await?;
    }
    tx.commit().await?;
    return Ok(urls_missing.len() as u64);
}

async fn update_host(conn: &mut SqliteConnection, raw: &str) -> anyhow::Result<()> {
    let (host, domain) = match Url::parse(raw) {
        Ok(url) => (urls::host_key(&url), urls::registrable_domain(&url)),
        Err(_) => (String::new(), None),
    };
    sqlx::query("UPDATE frontier SET host = ?, domain = ? WHERE url = ?")
        .bind(host)
        .bind(domain)
        .bind(raw)
        .execute(conn)
        .await?;
    return Ok(());
}

pub async fn queued_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let n = sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE state = 'queued'")
        .fetch_one(pool)
        .await?;
    return Ok(n);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn url(s: &str) -> Url {
        return Url::parse(s).unwrap();
    }

    async fn add(pool: &SqlitePool, u: &str, score: f64) -> bool {
        let u = url(u);
        let c = Candidate {
            url: &u,
            score,
            depth: 1,
            from_page_id: None,
            reason: "test",
        };
        return enqueue(pool, &c).await.unwrap();
    }

    async fn state_of(pool: &SqlitePool, u: &str) -> (String, f64, u32) {
        return sqlx::query_as("SELECT state, score, attempts FROM frontier WHERE url = ?")
            .bind(u)
            .fetch_one(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn enqueue_keeps_best_score_while_queued() {
        let (_dir, pool) = test_pool().await;
        assert!(add(&pool, "https://a.com/x", 10.0).await);
        assert!(!add(&pool, "https://a.com/x", 5.0).await);
        assert!(add(&pool, "https://a.com/x", 20.0).await);
        assert_eq!(state_of(&pool, "https://a.com/x").await.1, 20.0);

        set_state(&pool, "https://a.com/x", State::Done)
            .await
            .unwrap();
        assert!(!add(&pool, "https://a.com/x", 99.0).await);
        assert_eq!(
            state_of(&pool, "https://a.com/x").await,
            ("done".into(), 20.0, 0)
        );
    }

    #[tokio::test]
    async fn next_batch_picks_best_per_host_and_skips_excluded() {
        let (_dir, pool) = test_pool().await;
        add(&pool, "https://a.com/low", 1.0).await;
        add(&pool, "https://a.com/high", 50.0).await;
        add(&pool, "https://b.com/", 30.0).await;
        add(&pool, "https://jobs.b.com/", 40.0).await;
        add(&pool, "https://c.com/", 60.0).await;

        let batch = next_batch(&pool, &["c.com".to_string()], 10).await.unwrap();
        let got: Vec<&str> = batch.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            got,
            [
                "https://a.com/high",
                "https://jobs.b.com/",
                "https://b.com/"
            ]
        );
        assert_eq!(batch[1].domain.as_deref(), Some("b.com"));
        assert_eq!(batch[1].host, "jobs.b.com");

        assert_eq!(next_batch(&pool, &[], 2).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn retry_and_crash_recovery() {
        let (_dir, pool) = test_pool().await;
        add(&pool, "https://a.com/", 40.0).await;
        set_state(&pool, "https://a.com/", State::InFlight)
            .await
            .unwrap();
        retry(&pool, "https://a.com/").await.unwrap();
        assert_eq!(
            state_of(&pool, "https://a.com/").await,
            ("queued".into(), 20.0, 1)
        );

        set_state(&pool, "https://a.com/", State::InFlight)
            .await
            .unwrap();
        assert_eq!(reset_in_flight(&pool).await.unwrap(), 1);
        assert_eq!(queued_count(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn mark_visited_inserts_or_completes() {
        let (_dir, pool) = test_pool().await;
        add(&pool, "https://a.com/queued", 5.0).await;
        mark_visited(&pool, &url("https://a.com/queued"), 1)
            .await
            .unwrap();
        mark_visited(&pool, &url("https://a.com/new"), 2)
            .await
            .unwrap();
        assert_eq!(state_of(&pool, "https://a.com/queued").await.0, "done");
        assert_eq!(state_of(&pool, "https://a.com/new").await.0, "done");
        assert_eq!(queued_count(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn backfills_legacy_rows() {
        let (_dir, pool) = test_pool().await;
        sqlx::query(
            "INSERT INTO frontier (url, score, depth, state, enqueued_at) VALUES ('https://careers.acme.co.uk/', 1, 0, 'queued', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(backfill_hosts(&pool).await.unwrap(), 1);
        let batch = next_batch(&pool, &[], 1).await.unwrap();
        assert_eq!(batch[0].host, "careers.acme.co.uk");
        assert_eq!(batch[0].domain.as_deref(), Some("acme.co.uk"));
    }
}
