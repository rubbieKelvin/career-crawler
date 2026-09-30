mod fetcher;
mod metrics;
mod parse;
mod politeness;
mod robots;
mod visit;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use career_core::{config::Config, db, events, events::Event, seeds};
use clap::{Parser, Subcommand};
use url::Url;

use crate::metrics::Metrics;
use crate::visit::{Outcome, Visitor};

/// Crawls the web for company career pages and job postings.
#[derive(Debug, Parser)]
#[command(name = "crawler", version)]
struct Args {
    /// Config file (default: ./config.toml if it exists, otherwise built-in defaults).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override `db_path` from the config.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Override `seeds_path` from the config.
    #[arg(long, global = true)]
    seeds: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fetch and parse a single URL (robots.txt and redirects included) without touching the DB.
    Fetch {
        url: String,
        /// How many extracted links to print.
        #[arg(long, default_value_t = 20)]
        links: usize,
    },
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

    return match args.command {
        Some(Command::Fetch { url, links }) => fetch_one(&config, &url, links).await,
        None => run(&config).await,
    };
}

async fn run(config: &Config) -> anyhow::Result<()> {
    let pool = db::open(&config.db_path).await?;
    tracing::info!(db = %config.db_path.display(), "database ready");
    events::append(
        &pool,
        &Event::CrawlerStarted {
            pid: std::process::id(),
        },
    )
    .await?;

    load_seeds(&pool, config).await?;

    let stats = db::stats(&pool).await?;
    tracing::info!(
        queued = stats.frontier_queued,
        "frontier ready; crawl loop arrives in milestone 3"
    );

    events::append(
        &pool,
        &Event::CrawlerStopped {
            reason: "no crawl loop yet".into(),
        },
    )
    .await?;
    pool.close().await;
    return Ok(());
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
    return Ok(());
}

async fn fetch_one(config: &Config, url: &str, max_links: usize) -> anyhow::Result<()> {
    let url = if url.contains("://") {
        Url::parse(url)
    } else {
        Url::parse(&format!("https://{url}"))
    }
    .with_context(|| format!("invalid URL {url:?}"))?;

    let metrics = Arc::new(Metrics::default());
    let visitor = Visitor::new(&config.crawler, metrics.clone())?;
    let visit = visitor.visit(&url).await;

    println!("requested  {}", visit.requested);
    for hop in &visit.redirects {
        println!("  -> {hop}");
    }
    match &visit.outcome {
        Outcome::Page {
            status,
            parsed,
            bytes_wire,
            bytes_body,
            elapsed,
        } => {
            println!("status     {status}");
            println!("title      {}", parsed.title.as_deref().unwrap_or("-"));
            if let Some(canonical) = &parsed.canonical {
                println!("canonical  {canonical}");
            }
            println!(
                "robots     noindex={} nofollow={}",
                parsed.noindex, parsed.nofollow
            );
            println!(
                "size       {bytes_wire} B wire, {bytes_body} B decoded, fetched in {elapsed:.2?}"
            );
            println!("links      {} unique", parsed.links.len());
            for link in parsed.links.iter().take(max_links) {
                let nofollow = if link.nofollow { " [nofollow]" } else { "" };
                println!("  {}  {:?}{nofollow}", link.url, link.text);
            }
            if parsed.links.len() > max_links {
                println!("  … {} more (use --links)", parsed.links.len() - max_links);
            }
        }
        Outcome::NotHtml { content_type } => {
            println!(
                "skipped    not HTML ({})",
                content_type.as_deref().unwrap_or("no content-type")
            );
        }
        Outcome::HttpError { status } => println!("status     {status}"),
        Outcome::RobotsDenied => println!("denied     by robots.txt at {}", visit.final_url),
        Outcome::TooManyRedirects => println!("failed     too many redirects"),
        Outcome::Failed(e) => println!("failed     [{}] {e}", e.kind()),
    }

    let m = metrics.snapshot();
    println!(
        "metrics    {} requests ({} robots.txt), {} B rx wire, {} B rx body, ~{} B tx",
        m.requests, m.robots_fetches, m.bytes_rx_wire, m.bytes_rx_body, m.bytes_tx
    );
    return Ok(());
}
