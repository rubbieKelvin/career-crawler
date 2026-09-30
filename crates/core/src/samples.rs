//! Metrics samples (see `brainstorms/09-metrics.md`): written by the crawler's sampler,
//! read by the UI for live and historical charts.

use serde::Serialize;
use sqlx::{SqliteExecutor, SqlitePool};

#[derive(Debug, Clone, PartialEq, Default, Serialize, sqlx::FromRow)]
pub struct Sample {
    #[sqlx(default)]
    pub id: i64,
    pub ts: i64,
    pub run_id: i64,
    pub requests: i64,
    pub fetch_errors: i64,
    pub bytes_rx_wire: i64,
    pub bytes_rx_body: i64,
    pub bytes_tx: i64,
    pub bytes_wasted: i64,
    pub cpu_pct: Option<f64>,
    pub cpu_time_ms: Option<i64>,
    pub rss_bytes: Option<i64>,
    pub db_bytes: Option<i64>,
    pub in_flight: i64,
    pub frontier_queued: i64,
    pub pages: i64,
    pub domains: i64,
    pub companies: i64,
    pub jobs: i64,
    pub llm_calls: i64,
    pub llm_cache_hits: i64,
    pub llm_errors: i64,
    pub llm_tokens_in: i64,
    pub llm_tokens_out: i64,
}

const COLUMNS: &str = "id, ts, run_id, requests, fetch_errors, bytes_rx_wire, bytes_rx_body, bytes_tx, bytes_wasted, \
                       cpu_pct, cpu_time_ms, rss_bytes, db_bytes, in_flight, frontier_queued, pages, domains, \
                       companies, jobs, llm_calls, llm_cache_hits, llm_errors, llm_tokens_in, llm_tokens_out";

pub async fn insert<'e>(exec: impl SqliteExecutor<'e>, s: &Sample) -> anyhow::Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO metrics_samples (ts, run_id, requests, fetch_errors, bytes_rx_wire, bytes_rx_body, bytes_tx,
                                      bytes_wasted, cpu_pct, cpu_time_ms, rss_bytes, db_bytes, in_flight,
                                      frontier_queued, pages, domains, companies, jobs, llm_calls, llm_cache_hits, llm_errors,
                                      llm_tokens_in, llm_tokens_out)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(s.ts)
    .bind(s.run_id)
    .bind(s.requests)
    .bind(s.fetch_errors)
    .bind(s.bytes_rx_wire)
    .bind(s.bytes_rx_body)
    .bind(s.bytes_tx)
    .bind(s.bytes_wasted)
    .bind(s.cpu_pct)
    .bind(s.cpu_time_ms)
    .bind(s.rss_bytes)
    .bind(s.db_bytes)
    .bind(s.in_flight)
    .bind(s.frontier_queued)
    .bind(s.pages)
    .bind(s.domains)
    .bind(s.companies)
    .bind(s.jobs)
    .bind(s.llm_calls)
    .bind(s.llm_cache_hits)
    .bind(s.llm_errors)
    .bind(s.llm_tokens_in)
    .bind(s.llm_tokens_out)
    .fetch_one(exec)
    .await?;
    return Ok(id);
}

/// The `n` most recent samples, newest first.
pub async fn latest(pool: &SqlitePool, n: i64) -> anyhow::Result<Vec<Sample>> {
    let rows = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM metrics_samples ORDER BY id DESC LIMIT ?"
    ))
    .bind(n)
    .fetch_all(pool)
    .await?;
    return Ok(rows);
}

/// Samples with id greater than `after_id`, oldest first (for tailing).
pub async fn since(pool: &SqlitePool, after_id: i64, limit: i64) -> anyhow::Result<Vec<Sample>> {
    let rows = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM metrics_samples WHERE id > ? ORDER BY id LIMIT ?"
    ))
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    return Ok(rows);
}

/// Samples in `[from, to]`, downsampled to the last sample of each `step`-ms bucket.
pub async fn history(
    pool: &SqlitePool,
    from: i64,
    to: i64,
    step: i64,
) -> anyhow::Result<Vec<Sample>> {
    let rows = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM metrics_samples
         WHERE id IN (SELECT MAX(id) FROM metrics_samples WHERE ts BETWEEN ? AND ? GROUP BY ts / ?)
         ORDER BY ts"
    ))
    .bind(from)
    .bind(to)
    .bind(step.max(1))
    .fetch_all(pool)
    .await?;
    return Ok(rows);
}

/// Per-second rates between two samples of the same run (`None` across runs or with no gap).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rates {
    pub bytes_rx_wire_per_sec: f64,
    pub requests_per_sec: f64,
    pub pages_per_sec: f64,
    pub jobs_per_sec: f64,
}

pub fn rates(previous: &Sample, current: &Sample) -> Option<Rates> {
    let secs = (current.ts - previous.ts) as f64 / 1000.0;
    if previous.run_id != current.run_id || secs <= 0.0 {
        return None;
    }
    let per_sec = |a: i64, b: i64| (b - a).max(0) as f64 / secs;
    return Some(Rates {
        bytes_rx_wire_per_sec: per_sec(previous.bytes_rx_wire, current.bytes_rx_wire),
        requests_per_sec: per_sec(previous.requests, current.requests),
        pages_per_sec: per_sec(previous.pages, current.pages),
        jobs_per_sec: per_sec(previous.jobs, current.jobs),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn sample(ts: i64, run_id: i64, bytes: i64) -> Sample {
        return Sample {
            ts,
            run_id,
            bytes_rx_wire: bytes,
            pages: ts / 1000,
            ..Sample::default()
        };
    }

    #[tokio::test]
    async fn insert_tail_and_downsample() {
        let (_dir, pool) = test_pool().await;
        for i in 0..10 {
            insert(&pool, &sample(i * 1000, 1, i * 100)).await.unwrap();
        }
        assert_eq!(latest(&pool, 1).await.unwrap()[0].ts, 9000);
        assert_eq!(since(&pool, 8, 100).await.unwrap().len(), 2);
        let buckets: Vec<i64> = history(&pool, 0, 9000, 5000)
            .await
            .unwrap()
            .iter()
            .map(|s| s.ts)
            .collect();
        assert_eq!(buckets, [4000, 9000], "last sample of each 5s bucket");
    }

    #[test]
    fn rates_only_within_a_run() {
        let r = rates(&sample(0, 1, 0), &sample(2000, 1, 1000)).unwrap();
        assert_eq!((r.bytes_rx_wire_per_sec, r.pages_per_sec), (500.0, 1.0));
        assert_eq!(rates(&sample(0, 1, 0), &sample(2000, 2, 1000)), None);
    }
}
