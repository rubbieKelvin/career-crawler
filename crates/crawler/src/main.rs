use std::path::PathBuf;

use anyhow::Context;
use career_core::{config::Config, db, events, events::Event, seeds};
use clap::Parser;

/// Crawls the web for company career pages and job postings.
#[derive(Debug, Parser)]
#[command(name = "crawler", version)]
struct Args {
    /// Config file (default: ./config.toml if it exists, otherwise built-in defaults).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override `db_path` from the config.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Override `seeds_path` from the config.
    #[arg(long)]
    seeds: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    career_core::logging::init();
    let args = Args::parse();

    let mut config = Config::load(args.config.as_deref())?;
    if let Some(db) = args.db {
        config.db_path = db;
    }
    if let Some(seeds) = args.seeds {
        config.seeds_path = seeds;
    }

    let pool = db::open(&config.db_path).await?;
    tracing::info!(db = %config.db_path.display(), "database ready");
    events::append(
        &pool,
        &Event::CrawlerStarted {
            pid: std::process::id(),
        },
    )
    .await?;

    load_seeds(&pool, &config).await?;

    let stats = db::stats(&pool).await?;
    tracing::info!(
        queued = stats.frontier_queued,
        "frontier ready; crawl loop arrives in milestone 2"
    );

    events::append(
        &pool,
        &Event::CrawlerStopped {
            reason: "no crawl loop yet".into(),
        },
    )
    .await?;
    pool.close().await;
    Ok(())
}

async fn load_seeds(pool: &db::Pool, config: &Config) -> anyhow::Result<()> {
    if !config.seeds_path.exists() {
        tracing::warn!(path = %config.seeds_path.display(), "seeds file not found; skipping");
        return Ok(());
    }
    let parsed = seeds::read(&config.seeds_path)?;
    for bad in &parsed.invalid {
        tracing::warn!(line = bad.line, text = %bad.text, reason = %bad.reason, "invalid seed");
    }
    let enqueued = seeds::enqueue(pool, &parsed.urls)
        .await
        .context("enqueueing seeds")?;
    tracing::info!(
        parsed = parsed.urls.len(),
        enqueued,
        invalid = parsed.invalid.len(),
        "seeds loaded"
    );
    events::append(
        pool,
        &Event::SeedsLoaded {
            parsed: parsed.urls.len(),
            enqueued,
            invalid: parsed.invalid.len(),
        },
    )
    .await?;
    Ok(())
}
