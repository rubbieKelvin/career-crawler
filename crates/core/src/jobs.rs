//! Job postings and the ATS boards they come from. The crawler writes these; the UI will
//! search them. Fields that need enrichment (category, seniority, geo, USD salary) are
//! filled later (milestone 10).

use sqlx::{SqliteConnection, SqliteExecutor};

/// A posting as extracted from one source, before storage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Job {
    /// Canonical posting URL: the job's identity.
    pub url: String,
    pub external_id: Option<String>,
    pub title: String,
    /// Hiring company as the source names it.
    pub company: Option<String>,
    pub location: Option<String>,
    /// ISO 3166-1 alpha-2, only when the source gives one.
    pub country_code: Option<String>,
    /// `onsite`, `hybrid` or `remote`.
    pub remote_mode: Option<String>,
    pub department: Option<String>,
    /// `full_time`, `part_time`, `contract`, `internship`, `temporary`, …
    pub employment_type: Option<String>,
    pub salary_min: Option<f64>,
    pub salary_max: Option<f64>,
    pub salary_currency: Option<String>,
    /// `year`, `month`, `week`, `day` or `hour`.
    pub salary_period: Option<String>,
    /// Unix epoch milliseconds.
    pub posted_at: Option<i64>,
    /// Plain text.
    pub description: Option<String>,
    /// `ats:<vendor>` or `jsonld`.
    pub source: String,
}

/// Inserts or refreshes a posting (reopening it if it was closed). Returns whether it's new.
/// Known `domain_id` / `board_key` values are never overwritten with NULL.
pub async fn upsert(
    conn: &mut SqliteConnection,
    job: &Job,
    domain_id: Option<i64>,
    board_key: Option<&str>,
    now: i64,
) -> anyhow::Result<bool> {
    let first_seen: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (url, domain_id, board_key, external_id, title, company, location, country_code,
                           remote_mode, department, employment_type, salary_min, salary_max, salary_currency,
                           salary_period, posted_at, description, source, first_seen, last_seen)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(url) DO UPDATE SET
           domain_id = COALESCE(excluded.domain_id, jobs.domain_id),
           board_key = COALESCE(excluded.board_key, jobs.board_key),
           external_id = excluded.external_id, title = excluded.title, company = excluded.company,
           location = excluded.location, country_code = excluded.country_code,
           remote_mode = excluded.remote_mode, department = excluded.department,
           employment_type = excluded.employment_type, salary_min = excluded.salary_min,
           salary_max = excluded.salary_max, salary_currency = excluded.salary_currency,
           salary_period = excluded.salary_period, posted_at = excluded.posted_at,
           description = excluded.description, source = excluded.source,
           last_seen = excluded.last_seen, closed_at = NULL
         RETURNING first_seen",
    )
    .bind(&job.url)
    .bind(domain_id)
    .bind(board_key)
    .bind(&job.external_id)
    .bind(&job.title)
    .bind(&job.company)
    .bind(&job.location)
    .bind(&job.country_code)
    .bind(&job.remote_mode)
    .bind(&job.department)
    .bind(&job.employment_type)
    .bind(job.salary_min)
    .bind(job.salary_max)
    .bind(&job.salary_currency)
    .bind(&job.salary_period)
    .bind(job.posted_at)
    .bind(&job.description)
    .bind(&job.source)
    .bind(now)
    .bind(now)
    .fetch_one(conn)
    .await?;
    return Ok(first_seen == now);
}

/// After a full board fetch, closes the board's open jobs that weren't in it. Returns how many.
pub async fn close_missing<'e>(
    exec: impl SqliteExecutor<'e>,
    board_key: &str,
    seen_urls: &[String],
    now: i64,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE jobs SET closed_at = ?
         WHERE board_key = ? AND closed_at IS NULL AND url NOT IN (SELECT value FROM json_each(?))",
    )
    .bind(now)
    .bind(board_key)
    .bind(serde_json::to_string(seen_urls)?)
    .execute(exec)
    .await?;
    return Ok(result.rows_affected());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRef<'a> {
    pub key: &'a str,
    pub vendor: &'a str,
    pub token: &'a str,
    pub host: &'a str,
}

pub async fn ensure_board<'e>(
    exec: impl SqliteExecutor<'e>,
    board: &BoardRef<'_>,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO boards (key, vendor, token, host, first_seen) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(key) DO NOTHING",
    )
    .bind(board.key)
    .bind(board.vendor)
    .bind(board.token)
    .bind(board.host)
    .bind(now)
    .execute(exec)
    .await?;
    return Ok(());
}

/// Attributes a board to its company domain, and its already-stored jobs with it.
pub async fn attach_board(
    conn: &mut SqliteConnection,
    key: &str,
    domain_id: i64,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE boards SET domain_id = ? WHERE key = ?")
        .bind(domain_id)
        .bind(key)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE jobs SET domain_id = ? WHERE board_key = ? AND domain_id IS NULL")
        .bind(domain_id)
        .bind(key)
        .execute(&mut *conn)
        .await?;
    return Ok(());
}

pub async fn board_domain<'e>(
    exec: impl SqliteExecutor<'e>,
    key: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    let row = sqlx::query_as(
        "SELECT d.id, d.host FROM boards b JOIN domains d ON d.id = b.domain_id WHERE b.key = ?",
    )
    .bind(key)
    .fetch_optional(exec)
    .await?;
    return Ok(row);
}

/// Finds the company domain for a board nobody has attributed yet, from what the board
/// itself says. Only exact evidence counts: the ATS's company name equals a known domain's
/// name (`Anduril Industries`), or the token equals a domain's first label (`kong` →
/// `kong.com`). Only `company` and `probing` domains are considered.
pub async fn find_board_company(
    conn: &mut SqliteConnection,
    token: &str,
    company_name: Option<&str>,
) -> anyhow::Result<Option<(i64, String)>> {
    let simplify = |s: &str| -> String {
        return s
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase();
    };
    let token = simplify(token);
    let name = company_name.map(simplify).filter(|n| n.len() >= 3);
    let candidates: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, host, name FROM domains WHERE status IN ('company', 'probing') ORDER BY status = 'company' DESC, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let by_name = name.as_ref().and_then(|n| {
        candidates
            .iter()
            .find(|(_, _, domain_name)| domain_name.as_deref().map(simplify).as_ref() == Some(n))
    });
    let by_token = || {
        candidates
            .iter()
            .find(|(_, host, _)| host.split('.').next().map(simplify) == Some(token.clone()))
    };
    return Ok(by_name
        .or_else(by_token)
        .map(|(id, host, _)| (*id, host.clone())));
}

/// Whether the board was fetched at or after `since` (ms), whatever the outcome.
pub async fn board_fetched_since<'e>(
    exec: impl SqliteExecutor<'e>,
    key: &str,
    since: i64,
) -> anyhow::Result<bool> {
    let fetched: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM boards WHERE key = ? AND last_fetched >= ?)",
    )
    .bind(key)
    .bind(since)
    .fetch_one(exec)
    .await?;
    return Ok(fetched);
}

pub async fn record_board_fetch<'e>(
    exec: impl SqliteExecutor<'e>,
    key: &str,
    status: &str,
    job_count: Option<usize>,
    name: Option<&str>,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE boards SET last_fetched = ?, last_status = ?, job_count = COALESCE(?, job_count),
                           name = COALESCE(?, name)
         WHERE key = ?",
    )
    .bind(now)
    .bind(status)
    .bind(job_count.map(|n| n as i64))
    .bind(name)
    .bind(key)
    .execute(exec)
    .await?;
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn job(url: &str, title: &str) -> Job {
        return Job {
            url: url.into(),
            title: title.into(),
            source: "ats:lever".into(),
            ..Job::default()
        };
    }

    const BOARD: BoardRef<'static> = BoardRef {
        key: "lever/acme",
        vendor: "lever",
        token: "acme",
        host: "jobs.lever.co",
    };

    #[tokio::test]
    async fn upsert_reports_new_and_refreshes() {
        let (_dir, pool) = test_pool().await;
        let mut conn = pool.acquire().await.unwrap();
        ensure_board(&mut *conn, &BOARD, 1).await.unwrap();
        assert!(
            upsert(
                &mut conn,
                &job("https://j/1", "Engineer"),
                None,
                Some("lever/acme"),
                10
            )
            .await
            .unwrap()
        );
        assert!(
            !upsert(
                &mut conn,
                &job("https://j/1", "Senior Engineer"),
                None,
                None,
                20
            )
            .await
            .unwrap()
        );
        let (title, board, first, last): (String, Option<String>, i64, i64) =
            sqlx::query_as("SELECT title, board_key, first_seen, last_seen FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (title.as_str(), board.as_deref(), first, last),
            ("Senior Engineer", Some("lever/acme"), 10, 20)
        );
    }

    #[tokio::test]
    async fn closes_missing_jobs_and_reopens_returning_ones() {
        let (_dir, pool) = test_pool().await;
        let mut conn = pool.acquire().await.unwrap();
        ensure_board(&mut *conn, &BOARD, 1).await.unwrap();
        for url in ["https://j/1", "https://j/2"] {
            upsert(&mut conn, &job(url, "x"), None, Some("lever/acme"), 10)
                .await
                .unwrap();
        }
        assert_eq!(
            close_missing(&mut *conn, "lever/acme", &["https://j/1".into()], 20)
                .await
                .unwrap(),
            1
        );
        upsert(
            &mut conn,
            &job("https://j/2", "x"),
            None,
            Some("lever/acme"),
            30,
        )
        .await
        .unwrap();
        let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE closed_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open, 2);
    }

    #[tokio::test]
    async fn finds_board_company_by_exact_name_or_label() {
        let (_dir, pool) = test_pool().await;
        let mut conn = pool.acquire().await.unwrap();
        for (host, name, status) in [
            ("anduril.com", Some("Anduril Industries"), "probing"),
            ("kong.com", None, "company"),
            ("harvey.ai", None, "not_company"),
        ] {
            sqlx::query("INSERT INTO domains (host, name, status, first_seen) VALUES (?, ?, ?, 0)")
                .bind(host)
                .bind(name)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
        async fn found(
            conn: &mut SqliteConnection,
            token: &str,
            name: Option<&str>,
        ) -> Option<String> {
            return find_board_company(conn, token, name)
                .await
                .unwrap()
                .map(|(_, host)| host);
        }
        assert_eq!(
            found(&mut conn, "andurilindustries", Some("Anduril Industries"))
                .await
                .as_deref(),
            Some("anduril.com")
        );
        assert_eq!(
            found(&mut conn, "kong", None).await.as_deref(),
            Some("kong.com")
        );
        assert_eq!(
            found(&mut conn, "harvey", None).await,
            None,
            "not_company domains don't count"
        );
        assert_eq!(
            found(&mut conn, "konghq", None).await,
            None,
            "no fuzzy matching"
        );
    }

    #[tokio::test]
    async fn attaching_a_board_assigns_its_jobs() {
        let (_dir, pool) = test_pool().await;
        let mut conn = pool.acquire().await.unwrap();
        ensure_board(&mut *conn, &BOARD, 1).await.unwrap();
        upsert(
            &mut conn,
            &job("https://j/1", "x"),
            None,
            Some("lever/acme"),
            10,
        )
        .await
        .unwrap();
        let domain_id: i64 = sqlx::query_scalar(
            "INSERT INTO domains (host, first_seen) VALUES ('acme.com', 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(board_domain(&pool, "lever/acme").await.unwrap(), None);
        attach_board(&mut conn, "lever/acme", domain_id)
            .await
            .unwrap();
        assert_eq!(
            board_domain(&pool, "lever/acme").await.unwrap(),
            Some((domain_id, "acme.com".into()))
        );
        let job_domain: Option<i64> = sqlx::query_scalar("SELECT domain_id FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(job_domain, Some(domain_id));

        assert!(!board_fetched_since(&pool, "lever/acme", 50).await.unwrap());
        record_board_fetch(&pool, "lever/acme", "ok", Some(1), Some("Acme"), 60)
            .await
            .unwrap();
        assert!(board_fetched_since(&pool, "lever/acme", 50).await.unwrap());
    }
}
