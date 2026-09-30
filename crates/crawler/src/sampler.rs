//! The metrics sampler: every interval it combines the crawler's counters, the process's
//! CPU and memory, the DB size and table counts into a `metrics_samples` row, which the
//! UI charts live. It also enforces the `max_bytes` data cap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use career_core::control::Command;
use career_core::db;
use career_core::samples::{self, Sample};
use career_core::time::now_ms;
use sqlx::SqlitePool;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::control::CrawlControl;
use crate::metrics::Metrics;

pub struct Sampler {
    pub pool: SqlitePool,
    pub metrics: Arc<Metrics>,
    pub control: Arc<CrawlControl>,
    pub run_id: i64,
    pub db_path: PathBuf,
    pub interval: Duration,
    /// Stop the crawl at this many received bytes (0 = no cap).
    pub max_bytes: u64,
}

impl Sampler {
    /// Samples every interval until the crawl stops, then writes one final sample.
    pub async fn run(self) {
        let mut process = ProcessStats::new();
        let mut budget_hit = false;
        loop {
            tokio::select! {
                () = self.control.stopped() => break,
                () = tokio::time::sleep(self.interval) => {}
            }
            match self.sample_once(&mut process).await {
                Ok(sample) => {
                    if !budget_hit
                        && self.max_bytes > 0
                        && sample.bytes_rx_wire as u64 >= self.max_bytes
                    {
                        budget_hit = true;
                        tracing::warn!(max_bytes = self.max_bytes, "data cap reached; stopping");
                        if let Err(e) = self
                            .control
                            .apply(&self.pool, Command::Stop, "budget:max_bytes")
                            .await
                        {
                            tracing::error!(error = %e, "failed to stop at data cap");
                        }
                    }
                }
                Err(e) => tracing::error!(error = %e, "failed to write metrics sample"),
            }
        }
        if let Err(e) = self.sample_once(&mut process).await {
            tracing::error!(error = %e, "failed to write final metrics sample");
        }
    }

    pub async fn sample_once(&self, process: &mut ProcessStats) -> anyhow::Result<Sample> {
        let m = self.metrics.snapshot();
        let stats = db::stats(&self.pool).await?;
        let (cpu_pct, cpu_time_ms, rss_bytes) = process.refresh();
        let sample = Sample {
            id: 0,
            ts: now_ms(),
            run_id: self.run_id,
            requests: m.requests as i64,
            fetch_errors: m.fetch_errors as i64,
            bytes_rx_wire: m.bytes_rx_wire as i64,
            bytes_rx_body: m.bytes_rx_body as i64,
            bytes_tx: m.bytes_tx as i64,
            bytes_wasted: m.bytes_wasted as i64,
            cpu_pct,
            cpu_time_ms,
            rss_bytes,
            db_bytes: Some(db_file_bytes(&self.db_path) as i64),
            in_flight: m.in_flight as i64,
            frontier_queued: stats.frontier_queued,
            pages: stats.pages,
            domains: stats.domains,
            companies: stats.companies,
            jobs: stats.jobs,
        };
        samples::insert(&self.pool, &sample).await?;
        return Ok(sample);
    }
}

/// This process's CPU and memory via `sysinfo`. CPU % needs two refreshes, so the first
/// sample reports 0.
pub struct ProcessStats {
    system: System,
    pid: Option<Pid>,
}

impl ProcessStats {
    pub fn new() -> Self {
        return Self {
            system: System::new(),
            pid: sysinfo::get_current_pid().ok(),
        };
    }

    /// `(cpu %, accumulated CPU ms, RSS bytes)`, or all `None` if the process can't be read.
    fn refresh(&mut self) -> (Option<f64>, Option<i64>, Option<i64>) {
        let Some(pid) = self.pid else {
            return (None, None, None);
        };
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let Some(process) = self.system.process(pid) else {
            return (None, None, None);
        };
        return (
            Some(f64::from(process.cpu_usage())),
            Some(process.accumulated_cpu_time() as i64),
            Some(process.memory() as i64),
        );
    }
}

/// The database plus its WAL and shared-memory files.
fn db_file_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    }
    return total;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    use super::*;

    async fn sampler(max_bytes: u64) -> (tempfile::TempDir, Sampler) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let pool = db::open(&db_path).await.unwrap();
        return (
            dir,
            Sampler {
                pool,
                metrics: Arc::new(Metrics::default()),
                control: Arc::new(CrawlControl::default()),
                run_id: 7,
                db_path,
                interval: Duration::from_millis(10),
                max_bytes,
            },
        );
    }

    #[tokio::test]
    async fn writes_samples_with_process_stats() {
        let (_dir, s) = sampler(0).await;
        s.metrics.requests.fetch_add(3, Relaxed);
        let mut process = ProcessStats::new();
        let sample = s.sample_once(&mut process).await.unwrap();
        assert_eq!((sample.run_id, sample.requests), (7, 3));
        assert!(
            sample.rss_bytes.unwrap_or(0) > 0,
            "RSS should be readable for our own process"
        );
        assert!(sample.db_bytes.unwrap() > 0);
        assert_eq!(samples::latest(&s.pool, 1).await.unwrap()[0].requests, 3);
    }

    #[tokio::test]
    async fn stops_the_crawl_at_the_data_cap() {
        let (_dir, s) = sampler(1000).await;
        s.metrics.bytes_rx_wire.fetch_add(5000, Relaxed);
        let control = s.control.clone();
        let pool = s.pool.clone();
        tokio::time::timeout(Duration::from_secs(5), s.run())
            .await
            .expect("sampler exits once stopped");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), control.stopped())
                .await
                .is_ok()
        );
        let source: String = sqlx::query_scalar(
            "SELECT json_extract(payload, '$.source') FROM events WHERE kind = 'control_applied'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source, "budget:max_bytes");
    }
}
