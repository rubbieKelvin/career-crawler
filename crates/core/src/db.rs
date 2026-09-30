use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub use sqlx::SqlitePool as Pool;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Opens (creating if needed) the shared SQLite database and applies pending migrations.
///
/// Both the crawler and the UI call this, so whichever process starts first migrates.
/// WAL mode lets the UI read while the crawler writes; `busy_timeout` absorbs brief
/// lock contention between the two processes.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    return open_with(path, 8).await;
}

/// Like [`open`] with a fixed pool size. The crawler uses **one** connection, which makes
/// it a single writer: its writes queue up in-process instead of fighting over the SQLite
/// write lock.
pub async fn open_with(path: &Path, max_connections: u32) -> anyhow::Result<SqlitePool> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening database {}", path.display()))?;
    MIGRATOR.run(&pool).await.context("running migrations")?;
    return Ok(pool);
}

/// Row counts for a quick overview of the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub domains: i64,
    pub companies: i64,
    pub pages: i64,
    pub frontier_queued: i64,
    pub jobs: i64,
    pub events: i64,
}

pub async fn stats(pool: &SqlitePool) -> anyhow::Result<Stats> {
    let (domains, companies, pages, frontier_queued, jobs, events) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM domains),
                (SELECT COUNT(*) FROM domains WHERE status = 'company'),
                (SELECT COUNT(*) FROM pages),
                (SELECT COUNT(*) FROM frontier WHERE state = 'queued'),
                (SELECT COUNT(*) FROM jobs),
                (SELECT COUNT(*) FROM events)",
    )
    .fetch_one(pool)
    .await?;
    return Ok(Stats {
        domains,
        companies,
        pages,
        frontier_queued,
        jobs,
        events,
    });
}

#[cfg(test)]
pub(crate) async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let pool = open(&dir.path().join("nested/test.db")).await.unwrap();
    return (dir, pool);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_file_and_schema() {
        let (dir, pool) = test_pool().await;
        assert!(dir.path().join("nested/test.db").exists());
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for t in [
            "boards",
            "control_commands",
            "domains",
            "edges",
            "events",
            "frontier",
            "jobs",
            "metrics_samples",
            "page_links",
            "pages",
        ] {
            assert!(tables.iter().any(|n| n == t), "missing table {t}");
        }
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn stats_on_empty_db() {
        let (_dir, pool) = test_pool().await;
        assert_eq!(stats(&pool).await.unwrap(), Stats::default());
    }

    #[tokio::test]
    async fn reopening_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        open(&path).await.unwrap().close().await;
        open(&path).await.unwrap();
    }
}
