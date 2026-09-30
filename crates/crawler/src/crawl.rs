//! The crawl scheduler. It repeatedly takes the best queued URL per free host, runs up to
//! `concurrency` visits at once, and records each through `store`. It stops when the
//! frontier is exhausted, the page limit is hit, or `shutdown` resolves (Ctrl-C), letting
//! in-flight visits finish first.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use career_core::events::Event;
use career_core::frontier::{self, Item, State};
use career_core::urls;
use sqlx::SqlitePool;
use tokio::task::{Id, JoinSet};
use url::Url;

use crate::ats;

use crate::store::{self, LinkPolicy};
use crate::visit::Visitor;

/// How long the scheduler sleeps when every candidate host is busy or cooling down.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// What a page counts against for `max_pages_per_domain`: the company board for ATS URLs
/// (`jobs.ashbyhq.com/acme`), otherwise the registrable domain. Without this, every
/// company on a shared ATS domain would share one budget.
pub fn budget_key(url: &Url) -> String {
    if let Some(board) = ats::board_key(url) {
        return board;
    }
    return urls::registrable_domain(url).unwrap_or_else(|| urls::host_key(url));
}

/// Pages fetched (or dispatched) per [`budget_key`], enforcing `max_pages_per_domain`.
/// Seeded from the `pages` table so budgets hold across restarts.
#[derive(Debug)]
pub struct DomainPages {
    max: u32,
    counts: Mutex<HashMap<String, u32>>,
}

impl DomainPages {
    pub fn new(max: u32, counts: HashMap<String, u32>) -> Self {
        return Self {
            max,
            counts: Mutex::new(counts),
        };
    }

    pub async fn load(pool: &SqlitePool, max: u32) -> anyhow::Result<Self> {
        let page_urls: Vec<String> = sqlx::query_scalar("SELECT url FROM pages")
            .fetch_all(pool)
            .await?;
        let mut counts = HashMap::new();
        for url in page_urls.iter().filter_map(|u| Url::parse(u).ok()) {
            *counts.entry(budget_key(&url)).or_default() += 1;
        }
        return Ok(Self::new(max, counts));
    }

    pub fn get(&self, domain: &str) -> u32 {
        return self
            .counts
            .lock()
            .unwrap()
            .get(domain)
            .copied()
            .unwrap_or(0);
    }

    pub fn is_full(&self, domain: &str) -> bool {
        return self.get(domain) >= self.max;
    }

    /// Counts one page against `domain`'s budget. Returns false (counting nothing) if it's spent.
    pub fn try_take(&self, domain: &str) -> bool {
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(domain.to_string()).or_default();
        if *count >= self.max {
            return false;
        }
        *count += 1;
        return true;
    }
}

#[derive(Debug, Clone)]
pub struct CrawlOptions {
    pub concurrency: usize,
    /// Stop after dispatching this many URLs in this run.
    pub max_pages: Option<u64>,
    pub max_pages_per_domain: u32,
    pub links: LinkPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    FrontierExhausted,
    MaxPages,
    Interrupted,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        return match self {
            StopReason::FrontierExhausted => "frontier_exhausted",
            StopReason::MaxPages => "max_pages",
            StopReason::Interrupted => "interrupted",
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub dispatched: u64,
    pub stop: StopReason,
}

pub async fn run(
    pool: SqlitePool,
    visitor: Arc<Visitor>,
    options: CrawlOptions,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<Summary> {
    let domain_pages = Arc::new(DomainPages::load(&pool, options.max_pages_per_domain).await?);
    let links = Arc::new(options.links.clone());
    let max_reached = |dispatched: u64| options.max_pages.is_some_and(|max| dispatched >= max);

    let mut tasks = JoinSet::new();
    // Hosts of dispatched visits. The gate only marks a host busy once the visit
    // acquires it, so this closes the gap between dispatch and acquire.
    let mut task_hosts: HashMap<Id, String> = HashMap::new();
    let mut dispatched: u64 = 0;
    let mut stop = None;
    tokio::pin!(shutdown);

    loop {
        let mut progressed = false;
        if stop.is_none() && max_reached(dispatched) {
            stop = Some(StopReason::MaxPages);
        }

        if stop.is_none() && tasks.len() < options.concurrency {
            let mut exclude: HashSet<String> = visitor.gate().busy_hosts().into_iter().collect();
            exclude.extend(task_hosts.values().cloned());
            let exclude: Vec<String> = exclude.into_iter().collect();
            let batch =
                frontier::next_batch(&pool, &exclude, options.concurrency - tasks.len()).await?;

            if batch.is_empty() && tasks.is_empty() && frontier::queued_count(&pool).await? == 0 {
                stop = Some(StopReason::FrontierExhausted);
            }
            for item in batch {
                progressed = true;
                let key = budget_key(&item.url);
                if !domain_pages.try_take(&key) {
                    tracing::debug!(url = %item.url, budget = %key, "budget spent; skipping");
                    frontier::set_state(&pool, item.url.as_str(), State::Skipped).await?;
                    continue;
                }
                frontier::set_state(&pool, item.url.as_str(), State::InFlight).await?;
                dispatched += 1;
                let host = item.host.clone();
                let handle = tasks.spawn(process(
                    pool.clone(),
                    visitor.clone(),
                    item,
                    links.clone(),
                    domain_pages.clone(),
                ));
                task_hosts.insert(handle.id(), host);
                if max_reached(dispatched) {
                    break;
                }
            }
        }

        if stop.is_some() && tasks.is_empty() {
            break;
        }
        if progressed {
            while let Some(joined) = tasks.try_join_next_with_id() {
                finish(joined, &mut task_hosts);
            }
            continue;
        }
        tokio::select! {
            // Checked first so an interrupt is never starved by a stream of finishing tasks.
            biased;
            () = &mut shutdown, if stop.is_none() => {
                tracing::info!(in_flight = tasks.len(), "stopping; waiting for in-flight pages");
                stop = Some(StopReason::Interrupted);
            }
            Some(joined) = tasks.join_next_with_id(), if !tasks.is_empty() => finish(joined, &mut task_hosts),
            () = tokio::time::sleep(IDLE_TICK) => {}
        }
    }

    return Ok(Summary {
        dispatched,
        stop: stop.expect("loop only exits once a stop reason is set"),
    });
}

fn finish(joined: Result<(Id, ()), tokio::task::JoinError>, task_hosts: &mut HashMap<Id, String>) {
    let id = match joined {
        Ok((id, ())) => id,
        Err(e) => {
            tracing::error!(error = %e, "visit task panicked");
            e.id()
        }
    };
    task_hosts.remove(&id);
}

async fn process(
    pool: SqlitePool,
    visitor: Arc<Visitor>,
    item: Item,
    links: Arc<LinkPolicy>,
    domain_pages: Arc<DomainPages>,
) {
    let visit = visitor.visit(&item.url).await;
    match store::record(&pool, &item, &visit, &links, &domain_pages).await {
        Ok(Event::PageFetched {
            url,
            status,
            links,
            enqueued,
            duplicate,
            ..
        }) => tracing::info!(%url, status, links, enqueued, duplicate, "fetched"),
        Ok(Event::FetchFailed {
            url,
            reason,
            will_retry,
            ..
        }) => {
            tracing::info!(%url, %reason, will_retry, "not fetched");
        }
        Ok(_) => {}
        // The row stays `in_flight` and is re-queued on the next start.
        Err(e) => tracing::error!(url = %item.url, error = %e, "failed to record visit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use career_core::config::CrawlerConfig;
    use career_core::db;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::metrics::Metrics;

    fn options(max_pages_per_domain: u32) -> CrawlOptions {
        return CrawlOptions {
            concurrency: 1,
            max_pages: None,
            max_pages_per_domain,
            links: LinkPolicy {
                max_depth: 5,
                min_link_score: 1.0,
            },
        };
    }

    fn visitor() -> Arc<Visitor> {
        let config = CrawlerConfig {
            per_host_delay_ms: 0,
            ..CrawlerConfig::default()
        };
        return Arc::new(Visitor::new(&config, Arc::new(Metrics::default())).unwrap());
    }

    async fn page(server: &MockServer, route: &str, body: &str) {
        Mock::given(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/html"))
            .mount(server)
            .await;
    }

    /// A tiny site: home links to product, about and careers; careers links to a job.
    async fn site() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        page(
            &server,
            "/",
            "<a href='/product'>Product</a><a href='/about'>About</a><a href='/careers'>Careers</a>\
             <a href='/brochure.pdf'>Brochure</a>",
        )
        .await;
        page(&server, "/product", "<a href='/'>Home</a>product").await;
        page(&server, "/about", "<a href='/'>Home</a>about").await;
        page(
            &server,
            "/careers",
            "<a href='/careers/engineer'>Engineer</a>careers",
        )
        .await;
        page(&server, "/careers/engineer", "the job").await;
        return server;
    }

    async fn seed(pool: &SqlitePool, server: &MockServer) {
        let home = Url::parse(&server.uri()).unwrap();
        career_core::seeds::enqueue(pool, &[home]).await.unwrap();
    }

    async fn fetched_paths(pool: &SqlitePool) -> Vec<String> {
        let urls: Vec<String> = sqlx::query_scalar("SELECT url FROM pages ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        return urls
            .iter()
            .map(|u| Url::parse(u).unwrap().path().to_string())
            .collect();
    }

    #[tokio::test]
    async fn crawls_best_links_first_until_exhausted() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;

        let summary = run(
            pool.clone(),
            visitor(),
            options(100),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(summary.stop, StopReason::FrontierExhausted);
        assert_eq!(summary.dispatched, 5);
        assert_eq!(
            fetched_paths(&pool).await,
            ["/", "/careers", "/careers/engineer", "/about", "/product"],
            "careers first, then the company page, then the rest; the PDF is never fetched"
        );

        let (links, events, home_kind): (i64, i64, String) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM page_links),
                    (SELECT COUNT(*) FROM events WHERE kind = 'page_fetched'),
                    (SELECT kind FROM pages ORDER BY id LIMIT 1)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(links, 7, "the PDF link is recorded, just not crawled");
        assert_eq!(events, 5);
        assert_eq!(home_kind, "home");
        assert_eq!(frontier::queued_count(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn respects_domain_budget_and_page_limit() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;

        let summary = run(pool.clone(), visitor(), options(2), std::future::pending())
            .await
            .unwrap();
        assert_eq!(summary.dispatched, 2);
        assert_eq!(fetched_paths(&pool).await, ["/", "/careers"]);
        let skipped: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE state = 'skipped'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(skipped >= 2);

        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        let mut opts = options(100);
        opts.max_pages = Some(3);
        let summary = run(pool.clone(), visitor(), opts, std::future::pending())
            .await
            .unwrap();
        assert_eq!(
            summary,
            Summary {
                dispatched: 3,
                stop: StopReason::MaxPages
            }
        );
    }

    #[tokio::test]
    async fn retries_transient_failures_once() {
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;

        let summary = run(
            pool.clone(),
            visitor(),
            options(100),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(summary.dispatched, 2);
        let (error, attempts): (String, u32) = sqlx::query_as(
            "SELECT p.error, f.attempts FROM pages p JOIN frontier f ON f.url = p.url",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((error.as_str(), attempts), ("http_503", 1));
    }

    #[tokio::test]
    async fn interrupt_stops_dispatching() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        let summary = run(
            pool.clone(),
            visitor(),
            options(100),
            std::future::ready(()),
        )
        .await
        .unwrap();
        assert_eq!(summary.stop, StopReason::Interrupted);
        assert!(summary.dispatched <= 1);
        let in_flight: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE state = 'in_flight'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(in_flight, 0, "dispatched work finishes before returning");
    }

    #[test]
    fn budget_keys_split_shared_ats_domains() {
        let k = |s: &str| budget_key(&Url::parse(s).unwrap());
        assert_eq!(
            k("https://jobs.ashbyhq.com/atlys/1"),
            "jobs.ashbyhq.com/atlys"
        );
        assert_eq!(
            k("https://jobs.ashbyhq.com/harvey/2"),
            "jobs.ashbyhq.com/harvey"
        );
        assert_eq!(k("https://careers.acme.co.uk/x"), "acme.co.uk");
    }

    #[test]
    fn domain_budget_counts_up_to_max() {
        let pages = DomainPages::new(2, HashMap::from([("a.com".to_string(), 1)]));
        assert!(pages.try_take("a.com"));
        assert!(!pages.try_take("a.com"));
        assert!(pages.is_full("a.com"));
        assert_eq!(pages.get("b.com"), 0);
    }
}
