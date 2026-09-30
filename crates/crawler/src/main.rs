mod ats;
mod careers;
mod classify;
mod crawl;
mod fetcher;
mod metrics;
mod parse;
mod politeness;
mod robots;
mod scoring;
mod store;
mod visit;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use career_core::{config::Config, db, events, events::Event, frontier, seeds, urls};
use clap::{Parser, Subcommand};
use url::Url;

use crate::crawl::CrawlOptions;
use crate::metrics::Metrics;
use crate::store::LinkPolicy;
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
    /// Stop after this many pages in this run (default: until the frontier is exhausted or Ctrl-C).
    #[arg(long)]
    max_pages: Option<u64>,
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
        None => run(&config, args.max_pages).await,
    };
}

async fn run(config: &Config, max_pages: Option<u64>) -> anyhow::Result<()> {
    // One connection: the crawler is the database's single writer (see `db::open_with`).
    let pool = db::open_with(&config.db_path, 1).await?;
    tracing::info!(db = %config.db_path.display(), "database ready");
    events::append(
        &pool,
        &Event::CrawlerStarted {
            pid: std::process::id(),
        },
    )
    .await?;

    let requeued = frontier::reset_in_flight(&pool).await?;
    let backfilled = frontier::backfill_hosts(&pool).await?;
    if requeued + backfilled > 0 {
        tracing::info!(requeued, backfilled, "recovered frontier from previous run");
    }
    load_seeds(&pool, config).await?;

    let metrics = Arc::new(Metrics::default());
    let visitor = Arc::new(Visitor::new(&config.crawler, metrics.clone())?);
    let c = &config.crawler;
    let options = CrawlOptions {
        concurrency: c.max_concurrency.max(1),
        max_pages,
        discovery_pages_per_domain: c.discovery_pages_per_domain,
        harvest_pages_per_domain: c.harvest_pages_per_domain,
        links: LinkPolicy {
            max_depth: c.max_depth,
            min_link_score: c.min_link_score,
        },
    };
    let shutdown = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tracing::info!(
        concurrency = options.concurrency,
        ?max_pages,
        "crawl started; Ctrl-C to stop"
    );
    let result = crawl::run(pool.clone(), visitor, options, shutdown).await;

    let reason = match &result {
        Ok(summary) => summary.stop.as_str().to_string(),
        Err(e) => format!("error: {e}"),
    };
    let m = metrics.snapshot();
    let stats = db::stats(&pool).await?;
    tracing::info!(
        %reason,
        pages = result.as_ref().map(|s| s.dispatched).unwrap_or_default(),
        requests = m.requests,
        bytes_rx_wire = m.bytes_rx_wire,
        domains = stats.domains,
        companies = stats.companies,
        queued = stats.frontier_queued,
        "crawl stopped"
    );
    events::append(&pool, &Event::CrawlerStopped { reason }).await?;
    pool.close().await;
    result?;
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
            content_hash,
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
            println!("hash       {content_hash}");
            let domain = urls::registrable_domain(&visit.final_url).unwrap_or_default();
            let company = classify::assess(&visit.final_url, &domain, parsed, 0);
            println!(
                "company    {:.2} ({}) name={} [{}]",
                company.score,
                if company.score >= classify::COMPANY_THRESHOLD {
                    "company"
                } else if company.score < classify::NOT_COMPANY_THRESHOLD {
                    "not company if this is the homepage"
                } else {
                    "gray zone"
                },
                company.name.as_deref().unwrap_or("-"),
                company.signals.join(", ")
            );
            if careers::is_careers_page(&visit.final_url, &domain) {
                println!("careers    this looks like the careers page");
            }
            let careers_links = careers::careers_links(parsed, &domain);
            for link in careers_links.iter().take(3) {
                println!("careers    link {}  {:?}", link.url, link.text);
            }
            match careers::attributed_board(parsed, &domain) {
                Some((board, source)) => {
                    println!(
                        "ats        {} via {} -> {}",
                        board.key(),
                        source.as_str(),
                        board.url()
                    );
                }
                None if careers_links.is_empty() => {
                    println!(
                        "careers    none on this page; a company would get /careers, /jobs and sitemap probes"
                    );
                }
                None => {}
            }
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
