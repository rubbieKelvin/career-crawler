//! The crawl scheduler. It repeatedly takes the best queued URL per free host, runs up to
//! `concurrency` visits at once, and records each through `store`. It stops when the
//! frontier is exhausted, the page limit is hit, or `shutdown` resolves (Ctrl-C), letting
//! in-flight visits finish first.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use areer_core::domains::DomainStatus;
use areer_core::events::Event;
use areer_core::frontier::{self, Candidate, Item, State};
use areer_core::jobs;
use areer_core::time::now_ms;
use areer_core::urls;
use areer_llm::Llm;
use sqlx::SqlitePool;
use tokio::task::{Id, JoinSet};
use url::Url;

use crate::ats::{self, Board};
use crate::careers;
use crate::control::CrawlControl;
use crate::extract;
use crate::llm_classify;
use crate::metrics::Metrics;
use crate::store::{self, LinkPolicy};
use crate::visit::Visitor;

/// How long the scheduler sleeps when every candidate host is busy or cooling down.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// What a page counts against for page budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetKey {
    /// The company board for ATS URLs (`ashby/acme`), otherwise the registrable
    /// domain. Without this, every company on a shared ATS domain would share one budget.
    pub key: String,
    /// ATS boards are job listings by definition, so they always get the harvest budget.
    pub board: bool,
}

pub fn budget_key(url: &Url) -> BudgetKey {
    if let Some(board) = ats::board(url) {
        return BudgetKey {
            key: board.key(),
            board: true,
        };
    }
    let key = urls::registrable_domain(url).unwrap_or_else(|| urls::host_key(url));
    return BudgetKey { key, board: false };
}

/// Page budgets. A domain gets `discovery` pages while we work out what it is, and
/// `harvest` pages once it is classified as a company. Counts are seeded from `pages` and
/// statuses from `domains`, so budgets hold across restarts.
#[derive(Debug)]
pub struct Budgets {
    discovery: u32,
    harvest: u32,
    counts: Mutex<HashMap<String, u32>>,
    statuses: Mutex<HashMap<String, DomainStatus>>,
}

impl Budgets {
    pub fn new(discovery: u32, harvest: u32) -> Self {
        return Self {
            discovery,
            harvest: harvest.max(discovery),
            counts: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
        };
    }

    pub async fn load(pool: &SqlitePool, discovery: u32, harvest: u32) -> anyhow::Result<Self> {
        let budgets = Self::new(discovery, harvest);
        let page_urls: Vec<String> = sqlx::query_scalar("SELECT url FROM pages")
            .fetch_all(pool)
            .await?;
        {
            let mut counts = budgets.counts.lock().unwrap();
            for url in page_urls.iter().filter_map(|u| Url::parse(u).ok()) {
                *counts.entry(budget_key(&url).key).or_default() += 1;
            }
        }
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT host, status FROM domains WHERE status <> 'discovered'")
                .fetch_all(pool)
                .await?;
        for (host, status) in statuses {
            budgets.set_status(&host, DomainStatus::parse(&status));
        }
        return Ok(budgets);
    }

    /// Classification of a registrable domain as far as this crawl knows.
    pub fn status(&self, domain: &str) -> DomainStatus {
        return self
            .statuses
            .lock()
            .unwrap()
            .get(domain)
            .copied()
            .unwrap_or_default();
    }

    pub fn set_status(&self, domain: &str, status: DomainStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(domain.to_string(), status);
    }

    pub fn count(&self, key: &BudgetKey) -> u32 {
        return self
            .counts
            .lock()
            .unwrap()
            .get(&key.key)
            .copied()
            .unwrap_or(0);
    }

    fn limit(&self, key: &BudgetKey) -> u32 {
        if key.board || self.status(&key.key) == DomainStatus::Company {
            return self.harvest;
        }
        return self.discovery;
    }

    /// No budget could ever admit another page here, so new links to it aren't worth
    /// enqueuing. Short of this, over-budget URLs are enqueued and deferred at dispatch,
    /// so they can be revived if the domain turns out to be a company.
    pub fn is_exhausted(&self, key: &BudgetKey) -> bool {
        return self.count(key) >= self.harvest;
    }

    /// Counts one page against `key`'s current budget. Returns false (counting nothing) if it's spent.
    pub fn try_take(&self, key: &BudgetKey) -> bool {
        let limit = self.limit(key);
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(key.key.clone()).or_default();
        if *count >= limit {
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
    pub discovery_pages_per_domain: u32,
    pub harvest_pages_per_domain: u32,
    pub links: LinkPolicy,
    /// How long an ATS board's API listing stays fresh.
    pub board_refresh: Duration,
    /// Pause/resume/stop, from the UI or budgets.
    pub control: Arc<CrawlControl>,
    /// Where the scheduler reports its in-flight gauge.
    pub metrics: Arc<Metrics>,
    /// The optional LLM tier; `None` crawls on heuristics alone.
    pub llm: Option<Arc<Llm>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    FrontierExhausted,
    MaxPages,
    /// Ctrl-C.
    Interrupted,
    /// A `stop` control command (from the UI or a budget).
    Stopped,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        return match self {
            StopReason::FrontierExhausted => "frontier_exhausted",
            StopReason::MaxPages => "max_pages",
            StopReason::Interrupted => "interrupted",
            StopReason::Stopped => "stopped",
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
    let budgets = Arc::new(
        Budgets::load(
            &pool,
            options.discovery_pages_per_domain,
            options.harvest_pages_per_domain,
        )
        .await?,
    );
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

        if stop.is_none() && !options.control.is_paused() && tasks.len() < options.concurrency {
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
                if !budgets.try_take(&key) {
                    tracing::debug!(url = %item.url, budget = %key.key, "budget spent; deferring");
                    frontier::set_state(&pool, item.url.as_str(), State::Deferred).await?;
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
                    budgets.clone(),
                    options.board_refresh,
                    options.control.clone(),
                    options.llm.clone(),
                ));
                task_hosts.insert(handle.id(), host);
                options.metrics.in_flight.store(tasks.len() as u64, Relaxed);
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
            // Checked first so a stop is never starved by a stream of finishing tasks.
            biased;
            () = &mut shutdown, if stop.is_none() => {
                tracing::info!(in_flight = tasks.len(), "stopping; waiting for in-flight pages");
                stop = Some(StopReason::Interrupted);
            }
            () = options.control.stopped(), if stop.is_none() => {
                tracing::info!(in_flight = tasks.len(), "stop requested; waiting for in-flight pages");
                stop = Some(StopReason::Stopped);
            }
            Some(joined) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                finish(joined, &mut task_hosts);
                options.metrics.in_flight.store(tasks.len() as u64, Relaxed);
            }
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

#[allow(clippy::too_many_arguments)]
async fn process(
    pool: SqlitePool,
    visitor: Arc<Visitor>,
    item: Item,
    links: Arc<LinkPolicy>,
    budgets: Arc<Budgets>,
    board_refresh: Duration,
    control: Arc<CrawlControl>,
    llm: Option<Arc<Llm>>,
) {
    // Any URL on a board we can read by API (its landing page, a posting, an application
    // form) means: fetch the whole board's listing once, instead of crawling its HTML.
    if let Some(board) = ats::board(&item.url).filter(|b| extract::has_api(b.vendor)) {
        harvest_board(&pool, &visitor, &item, &board, board_refresh).await;
        return;
    }

    let visit = visitor.visit(&item.url).await;
    let recorded = match store::record(&pool, &item, &visit, &links, &budgets).await {
        Ok(recorded) => recorded,
        // The row stays `in_flight` and is re-queued on the next start.
        Err(e) => {
            tracing::error!(url = %item.url, error = %e, "failed to record visit");
            return;
        }
    };
    recorded.events.into_iter().for_each(log_event);
    let mut sitemap_scan = recorded.sitemap_scan;

    // A gray-zone homepage: the LLM gets to break the tie (if there is one). Whatever it
    // says only ever settles the domain; failures leave it as the heuristics left it.
    if let (Some(request), Some(llm)) =
        (recorded.classify, llm.filter(|_| !control.stop_requested()))
    {
        match llm_classify::ask(&llm, &request).await {
            Ok(verdict) => {
                match store::record_verdict(&pool, &request, &verdict, &links, &budgets).await {
                    Ok(verdict_recorded) => {
                        tracing::info!(
                            domain = %request.domain,
                            is_company = verdict.is_company,
                            confidence = verdict.confidence,
                            reason = %verdict.reason,
                            "LLM classified"
                        );
                        verdict_recorded.events.into_iter().for_each(log_event);
                        sitemap_scan = verdict_recorded.sitemap_scan.or(sitemap_scan);
                    }
                    Err(e) => {
                        tracing::error!(domain = %request.domain, error = %e, "failed to record LLM verdict");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(domain = %request.domain, error = %e, "LLM classification failed; keeping heuristic status");
            }
        }
    }

    // Sitemap scans are optional follow-up work; don't hold up a stop with them.
    if let Some(scan) = sitemap_scan.filter(|_| !control.stop_requested()) {
        let found = careers::discover_via_sitemap(&visitor, &scan.home, &scan.domain).await;
        let mut enqueued = 0;
        for url in &found {
            let candidate = Candidate {
                url,
                score: careers::SITEMAP_SCORE,
                depth: scan.depth + 1,
                from_page_id: None,
                reason: "sitemap",
            };
            match frontier::enqueue(&pool, &candidate).await {
                Ok(true) => enqueued += 1,
                Ok(false) => {}
                Err(e) => tracing::error!(%url, error = %e, "failed to enqueue sitemap URL"),
            }
        }
        tracing::info!(domain = %scan.domain, found = found.len(), enqueued, "sitemap scanned for careers");
    }
}

async fn harvest_board(
    pool: &SqlitePool,
    visitor: &Visitor,
    item: &Item,
    board: &Board,
    refresh: Duration,
) {
    let key = board.key();
    let since = now_ms() - refresh.as_millis() as i64;
    match jobs::board_fetched_since(pool, &key, since).await {
        Ok(false) => {}
        Ok(true) => {
            tracing::debug!(board = %key, url = %item.url, "board listing still fresh; skipping");
            if let Err(e) = frontier::set_state(pool, item.url.as_str(), State::Done).await {
                tracing::error!(url = %item.url, error = %e, "failed to update frontier");
            }
            return;
        }
        Err(e) => {
            tracing::error!(board = %key, error = %e, "failed to check board freshness");
            return;
        }
    }
    let Some(api) = extract::api_url(board) else {
        return;
    };
    let result = match visitor.fetch_resource(&api).await {
        Ok(body) => extract::parse_board(board, &body).map_err(|e| {
            tracing::warn!(board = %key, error = %e, "unparseable board API response");
            return "parse_error".to_string();
        }),
        Err(reason) => Err(reason),
    };
    match store::record_board(pool, item, board, &result).await {
        Ok(events) => events.into_iter().for_each(log_event),
        Err(e) => tracing::error!(board = %key, error = %e, "failed to record board"),
    }
}

fn log_event(event: Event) {
    match event {
        Event::PageFetched {
            url,
            status,
            links,
            enqueued,
            duplicate,
            ..
        } => tracing::info!(%url, status, links, enqueued, duplicate, "fetched"),
        Event::FetchFailed {
            url,
            reason,
            will_retry,
            ..
        } => tracing::info!(%url, %reason, will_retry, "not fetched"),
        Event::DomainClassified {
            domain,
            name,
            status,
            previous,
            score,
        } => tracing::info!(%domain, ?name, %status, %previous, score, "classified"),
        Event::CareersFound {
            domain,
            url,
            source,
            ats,
        } => tracing::info!(%domain, %url, %source, ?ats, "careers found"),
        Event::JobsFound {
            domain,
            board,
            source,
            url,
            total,
            new,
            closed,
        } => tracing::info!(?domain, ?board, %source, %url, total, new, closed, "jobs found"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use areer_core::config::CrawlerConfig;
    use areer_core::db;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn options(discovery: u32, harvest: u32) -> CrawlOptions {
        return CrawlOptions {
            concurrency: 1,
            max_pages: None,
            discovery_pages_per_domain: discovery,
            harvest_pages_per_domain: harvest,
            links: LinkPolicy::new(5, 1.0),
            board_refresh: Duration::from_secs(3600),
            control: Arc::new(CrawlControl::default()),
            metrics: Arc::new(Metrics::default()),
            llm: None,
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

    async fn server_without_robots() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        return server;
    }

    /// A tiny company site: home links to product, about and careers; careers links to a job.
    async fn site() -> MockServer {
        let server = server_without_robots().await;
        let home = format!(
            "<script type='application/ld+json'>{{\"@type\":\"Organization\",\"name\":\"Acme Ltd\"}}</script>\
             <a href='/product'>Product</a><a href='/about'>About</a><a href='/careers'>Careers</a>\
             <a href='/brochure.pdf'>Brochure</a><p>{}</p><footer>© 2026 Acme Ltd</footer>",
            "We build payment tools for businesses. ".repeat(10)
        );
        page(&server, "/", &home).await;
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
        areer_core::seeds::enqueue(pool, &[home]).await.unwrap();
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
            options(100, 100),
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

        // The home page classifies the site as a company, so the harvest budget (2) applies.
        let summary = run(
            pool.clone(),
            visitor(),
            options(1, 2),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(summary.dispatched, 2);
        assert_eq!(fetched_paths(&pool).await, ["/", "/careers"]);
        let deferred: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE state = 'deferred'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            deferred, 2,
            "about and product wait in case the budget grows"
        );

        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        let mut opts = options(100, 100);
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
            options(100, 100),
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
            options(100, 100),
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

    async fn domain_row(pool: &SqlitePool) -> (String, Option<String>, f64) {
        return sqlx::query_as(
            "SELECT status, name, company_score FROM domains WHERE host = '127.0.0.1'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn classifies_company_from_home_page() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        run(
            pool.clone(),
            visitor(),
            options(1, 100),
            std::future::pending(),
        )
        .await
        .unwrap();

        let (status, name, score) = domain_row(&pool).await;
        assert_eq!(
            (status.as_str(), name.as_deref()),
            ("company", Some("Acme Ltd"))
        );
        assert!(score >= crate::classify::COMPANY_THRESHOLD);
        assert_eq!(
            fetched_paths(&pool).await.len(),
            5,
            "harvest budget, not the discovery budget of 1"
        );
        let classified: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 'domain_classified'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(classified, 1);
    }

    #[tokio::test]
    async fn uncertain_sites_get_the_discovery_budget() {
        let server = server_without_robots().await;
        let home = format!(
            "<a href='/about'>About</a><a href='/contact'>Contact</a><a href='/privacy'>Privacy</a>\
             <a href='/a'>A</a><a href='/b'>B</a><p>{}</p><footer>© 2026 Jane</footer>",
            "Notes and thoughts. ".repeat(20)
        );
        page(&server, "/", &home).await;
        for route in ["/about", "/contact", "/privacy", "/a", "/b"] {
            page(&server, route, &format!("{route} {}", "text ".repeat(80))).await;
        }
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        run(
            pool.clone(),
            visitor(),
            options(2, 100),
            std::future::pending(),
        )
        .await
        .unwrap();

        assert_eq!(domain_row(&pool).await.0, "probing");
        assert_eq!(fetched_paths(&pool).await.len(), 2);
        let deferred: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM frontier WHERE state = 'deferred'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(deferred, 4);
    }

    #[tokio::test]
    async fn the_llm_settles_a_gray_zone_homepage_and_the_crawl_carries_on_as_a_company() {
        use areer_core::config::LlmConfig;
        use areer_llm::testing::FakeProvider;

        let server = server_without_robots().await;
        let home = format!(
            "<a href='/about'>About</a><a href='/contact'>Contact</a><a href='/privacy'>Privacy</a>\
             <p>{}</p><footer>© 2026 Jane</footer>",
            "Notes and thoughts. ".repeat(20)
        );
        page(&server, "/", &home).await;
        for route in ["/about", "/contact", "/privacy", "/careers"] {
            page(&server, route, &format!("{route} {}", "text ".repeat(80))).await;
        }
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;

        let provider = Arc::new(FakeProvider::replying(
            r#"{"is_company": true, "company_name": "Jane Studio", "industry": "design",
                "hq_country": "GB", "confidence": 0.92, "reason": "a design studio with clients"}"#,
        ));
        let llm = Arc::new(Llm::new(
            &LlmConfig::default(),
            pool.clone(),
            provider.clone(),
        ));
        let mut opts = options(3, 100);
        opts.llm = Some(llm);
        run(pool.clone(), visitor(), opts, std::future::pending())
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 1, "one question per domain");
        let sent = &provider.requests()[0].messages[1].content;
        assert!(sent.contains("heuristic company score"), "{sent}");
        let (status, industry): (String, Option<String>) =
            sqlx::query_as("SELECT status, industry FROM domains")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (status.as_str(), industry.as_deref()),
            ("company", Some("design"))
        );
        assert!(
            fetched_paths(&pool).await.iter().any(|p| p == "/careers"),
            "the careers probes ran after the promotion: {:?}",
            fetched_paths(&pool).await
        );
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE kind = 'domain_classified' AND json_extract(payload, '$.status') = 'company'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(logged, 1);
    }

    #[tokio::test]
    async fn a_failing_llm_leaves_the_crawl_on_heuristics() {
        use areer_core::config::LlmConfig;
        use areer_llm::provider::ProviderError;
        use areer_llm::testing::FakeProvider;

        let server = server_without_robots().await;
        let home = format!(
            "<a href='/about'>About</a><a href='/contact'>Contact</a><a href='/privacy'>Privacy</a>\
             <p>{}</p><footer>© 2026 Jane</footer>",
            "Notes and thoughts. ".repeat(20)
        );
        page(&server, "/", &home).await;
        for route in ["/about", "/contact", "/privacy"] {
            page(&server, route, &format!("{route} {}", "text ".repeat(80))).await;
        }
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        let provider = Arc::new(FakeProvider::new(|_| {
            return Err(ProviderError::Http {
                status: 401,
                body: "bad key".into(),
            });
        }));
        let mut opts = options(3, 100);
        opts.llm = Some(Arc::new(Llm::new(
            &LlmConfig::default(),
            pool.clone(),
            provider,
        )));
        run(pool.clone(), visitor(), opts, std::future::pending())
            .await
            .unwrap();
        assert_eq!(domain_row(&pool).await.0, "probing");
        let checked: bool = sqlx::query_scalar("SELECT llm_checked_at IS NOT NULL FROM domains")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!checked, "a failed question isn't recorded as asked");
    }

    #[tokio::test]
    async fn deep_first_page_queues_the_homepage_probe() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        let careers = Url::parse(&server.uri()).unwrap().join("/careers").unwrap();
        areer_core::seeds::enqueue(&pool, &[careers])
            .await
            .unwrap();
        run(
            pool.clone(),
            visitor(),
            options(3, 100),
            std::future::pending(),
        )
        .await
        .unwrap();

        let paths = fetched_paths(&pool).await;
        assert_eq!(
            &paths[..2],
            ["/careers", "/"],
            "the homepage probe outranks ordinary links"
        );
        let probe: String = sqlx::query_scalar("SELECT reason FROM frontier WHERE url = ?")
            .bind(Url::parse(&server.uri()).unwrap().as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(probe, "probe_home");
        assert_eq!(domain_row(&pool).await.0, "company");
    }

    fn company_home(links: &str) -> String {
        return format!(
            "<script type='application/ld+json'>{{\"@type\":\"Organization\",\"name\":\"Acme Ltd\"}}</script>\
             {links}<p>{}</p><footer>© 2026 Acme Ltd</footer>",
            "We build payment tools for businesses. ".repeat(10)
        );
    }

    async fn domain_careers(pool: &SqlitePool) -> (Option<String>, bool) {
        let (url, probed): (Option<String>, bool) = sqlx::query_as(
            "SELECT careers_url, careers_probed FROM domains WHERE host = '127.0.0.1'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        return (
            url.map(|u| Url::parse(&u).unwrap().path().to_string()),
            probed,
        );
    }

    #[tokio::test]
    async fn records_the_careers_page() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        run(
            pool.clone(),
            visitor(),
            options(3, 100),
            std::future::pending(),
        )
        .await
        .unwrap();

        assert_eq!(
            domain_careers(&pool).await,
            (Some("/careers".into()), false),
            "linked, so never probed"
        );
        let kinds: Vec<(String, String)> =
            sqlx::query_as("SELECT url, kind FROM pages WHERE kind = 'careers'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            kinds.len(),
            1,
            "only the landing page, not the posting under it: {kinds:?}"
        );
        let found: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 'careers_found'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(found, 1);
    }

    #[tokio::test]
    async fn company_without_careers_link_gets_probed_once() {
        let server = server_without_robots().await;
        let base = server.uri();
        page(
            &server,
            "/",
            &company_home("<a href='/product'>Product</a><a href='/about'>About us</a>"),
        )
        .await;
        for route in ["/product", "/about"] {
            page(&server, route, &format!("{route} {}", "text ".repeat(80))).await;
        }
        page(
            &server,
            "/jobs",
            &format!("Open roles {}", "text ".repeat(80)),
        )
        .await;
        Mock::given(path("/sitemap.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("<urlset><url><loc>{base}/company/join-us</loc></url></urlset>"),
                "application/xml",
            ))
            .mount(&server)
            .await;
        page(
            &server,
            "/company/join-us",
            &format!("Join us {}", "text ".repeat(80)),
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        run(
            pool.clone(),
            visitor(),
            options(3, 100),
            std::future::pending(),
        )
        .await
        .unwrap();

        assert_eq!(domain_row(&pool).await.0, "company");
        assert_eq!(
            domain_careers(&pool).await,
            (Some("/jobs".into()), true),
            "/careers 404s, /jobs exists"
        );
        let reasons: Vec<(String, String)> = sqlx::query_as(
            "SELECT reason, state FROM frontier WHERE reason IN ('probe_careers', 'sitemap') ORDER BY reason, url",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            reasons,
            [
                ("probe_careers".to_string(), "done".to_string()),
                ("probe_careers".to_string(), "done".to_string()),
                ("sitemap".to_string(), "done".to_string()),
            ],
            "/careers and /jobs probed (no careers. subdomain for an IP), and the sitemap hit fetched"
        );
    }

    #[tokio::test]
    async fn paused_crawl_waits_and_stop_command_ends_it() {
        let server = site().await;
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        seed(&pool, &server).await;
        let opts = options(3, 100);
        let control = opts.control.clone();
        control.set_paused(true);
        let crawl = tokio::spawn(run(pool.clone(), visitor(), opts, std::future::pending()));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            fetched_paths(&pool).await.is_empty(),
            "nothing dispatched while paused"
        );
        control.request_stop();
        let summary = tokio::time::timeout(Duration::from_secs(5), crawl)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            summary,
            Summary {
                dispatched: 0,
                stop: StopReason::Stopped
            }
        );
    }

    #[test]
    fn budget_keys_split_shared_ats_domains() {
        let k = |s: &str| budget_key(&Url::parse(s).unwrap());
        let board = |key: &str| BudgetKey {
            key: key.into(),
            board: true,
        };
        assert_eq!(k("https://jobs.ashbyhq.com/atlys/1"), board("ashby/atlys"));
        assert_eq!(
            k("https://jobs.ashbyhq.com/harvey/2"),
            board("ashby/harvey")
        );
        assert_eq!(
            k("https://careers.acme.co.uk/x"),
            BudgetKey {
                key: "acme.co.uk".into(),
                board: false
            }
        );
    }

    #[test]
    fn budget_depends_on_status_and_boards() {
        let budgets = Budgets::new(1, 3);
        let acme = BudgetKey {
            key: "acme.com".into(),
            board: false,
        };
        let board = BudgetKey {
            key: "jobs.lever.co/acme".into(),
            board: true,
        };
        assert!(budgets.try_take(&acme));
        assert!(!budgets.try_take(&acme), "discovery budget is 1");
        assert!(
            !budgets.is_exhausted(&acme),
            "could still grow to the harvest budget"
        );

        budgets.set_status("acme.com", DomainStatus::Company);
        assert!(budgets.try_take(&acme));
        assert!(budgets.try_take(&acme));
        assert!(!budgets.try_take(&acme));
        assert!(budgets.is_exhausted(&acme));

        for _ in 0..3 {
            assert!(
                budgets.try_take(&board),
                "boards always get the harvest budget"
            );
        }
        assert_eq!(budgets.status("unknown.com"), DomainStatus::Discovered);
    }
}
