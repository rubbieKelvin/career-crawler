//! robots.txt cache, keyed by origin (scheme + host + port), following RFC 9309:
//! - 2xx: obey the rules
//! - 4xx (including 401/403): no restrictions
//! - 5xx or unreachable: assume full disallow, and retry after a short TTL
//! - up to 5 redirects are followed

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use texting_robots::Robot;
use tokio::time::Instant;
use url::Url;

use crate::fetcher::{Fetcher, Want};
use crate::metrics::Metrics;

const MAX_ROBOTS_REDIRECTS: usize = 5;
/// How long a "server error / unreachable → disallow" verdict is cached before retrying.
const UNREACHABLE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
enum Rules {
    AllowAll,
    DisallowAll,
    Parsed(Robot),
}

struct Entry {
    rules: Arc<Rules>,
    expires: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RobotsDecision {
    pub allowed: bool,
    pub crawl_delay: Option<Duration>,
}

pub struct RobotsCache {
    fetcher: Arc<Fetcher>,
    metrics: Arc<Metrics>,
    agent: String,
    ttl: Duration,
    entries: Mutex<HashMap<String, Entry>>,
}

impl RobotsCache {
    pub fn new(fetcher: Arc<Fetcher>, metrics: Arc<Metrics>, agent: &str, ttl: Duration) -> Self {
        return Self {
            fetcher,
            metrics,
            agent: agent.to_string(),
            ttl,
            entries: Mutex::new(HashMap::new()),
        };
    }

    /// Whether `url` may be fetched, fetching and caching robots.txt for its origin if needed.
    pub async fn check(&self, url: &Url) -> RobotsDecision {
        let rules = self.rules(url).await;
        let decision = match rules.as_ref() {
            Rules::AllowAll => RobotsDecision {
                allowed: true,
                crawl_delay: None,
            },
            Rules::DisallowAll => RobotsDecision {
                allowed: false,
                crawl_delay: None,
            },
            Rules::Parsed(robot) => RobotsDecision {
                allowed: robot.allowed(url.as_str()),
                crawl_delay: robot
                    .delay
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .map(Duration::from_secs_f32),
            },
        };
        if !decision.allowed {
            self.metrics.robots_denied.fetch_add(1, Relaxed);
        }
        return decision;
    }

    /// `Sitemap:` URLs declared in the origin's robots.txt.
    pub async fn sitemaps(&self, url: &Url) -> Vec<Url> {
        return match self.rules(url).await.as_ref() {
            Rules::Parsed(robot) => robot
                .sitemaps
                .iter()
                .filter_map(|s| url.join(s.trim()).ok())
                .collect(),
            Rules::AllowAll | Rules::DisallowAll => Vec::new(),
        };
    }

    /// Cached rules for `url`'s origin, fetching robots.txt if needed. Two tasks racing on
    /// an uncached origin may both fetch it; that's harmless.
    async fn rules(&self, url: &Url) -> Arc<Rules> {
        let origin = url.origin().ascii_serialization();
        let cached = {
            let entries = self.entries.lock().unwrap();
            entries
                .get(&origin)
                .filter(|e| e.expires > Instant::now())
                .map(|e| e.rules.clone())
        };
        let rules = match cached {
            Some(rules) => rules,
            None => {
                let (rules, ttl) = self.fetch_rules(url).await;
                let rules = Arc::new(rules);
                self.entries.lock().unwrap().insert(
                    origin,
                    Entry {
                        rules: rules.clone(),
                        expires: Instant::now() + ttl,
                    },
                );
                rules
            }
        };
        return rules;
    }

    async fn fetch_rules(&self, url: &Url) -> (Rules, Duration) {
        let Ok(mut target) = url.join("/robots.txt") else {
            return (Rules::AllowAll, self.ttl);
        };
        for _ in 0..=MAX_ROBOTS_REDIRECTS {
            self.metrics.robots_fetches.fetch_add(1, Relaxed);
            let resp = match self.fetcher.fetch(&target, Want::Any).await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::debug!(%target, error = %e, "robots.txt unreachable; disallowing for now");
                    return (Rules::DisallowAll, UNREACHABLE_TTL);
                }
            };
            if let Some(next) = resp.redirect_target() {
                target = next;
                continue;
            }
            let status = resp.status;
            if status.is_success() {
                return match Robot::new(&self.agent, &resp.body) {
                    Ok(robot) => (Rules::Parsed(robot), self.ttl),
                    Err(e) => {
                        tracing::debug!(%target, error = %e, "unparseable robots.txt; allowing");
                        (Rules::AllowAll, self.ttl)
                    }
                };
            }
            if status.is_client_error() {
                return (Rules::AllowAll, self.ttl);
            }
            return (Rules::DisallowAll, UNREACHABLE_TTL);
        }
        // Too many redirects: RFC 9309 says treat as unavailable, i.e. allow.
        return (Rules::AllowAll, self.ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use career_core::config::CrawlerConfig;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn cache_for(server: &MockServer) -> (RobotsCache, Arc<Metrics>, Url) {
        let metrics = Arc::new(Metrics::default());
        let config = CrawlerConfig::default();
        let fetcher = Arc::new(Fetcher::new(&config, metrics.clone()).unwrap());
        let cache = RobotsCache::new(
            fetcher,
            metrics.clone(),
            config.robots_agent(),
            Duration::from_secs(60),
        );
        return (cache, metrics, Url::parse(&server.uri()).unwrap());
    }

    fn robots(status: u16, body: &str) -> Mock {
        return Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body));
    }

    #[tokio::test]
    async fn obeys_rules_and_caches() {
        let server = MockServer::start().await;
        robots(
            200,
            "User-agent: *\nDisallow: /private\n\nUser-agent: career-crawler\nDisallow: /admin\nCrawl-delay: 2\n",
        )
        .expect(1)
        .mount(&server)
        .await;
        let (cache, metrics, base) = cache_for(&server).await;

        let admin = cache.check(&base.join("/admin/x").unwrap()).await;
        assert!(!admin.allowed);
        let careers = cache.check(&base.join("/careers").unwrap()).await;
        assert_eq!(
            careers,
            RobotsDecision {
                allowed: true,
                crawl_delay: Some(Duration::from_secs(2))
            }
        );
        // Our own group replaces `*`, so /private is allowed for us.
        assert!(cache.check(&base.join("/private").unwrap()).await.allowed);
        assert_eq!(metrics.snapshot().robots_fetches, 1);
        assert_eq!(metrics.snapshot().robots_denied, 1);
    }

    #[tokio::test]
    async fn exposes_declared_sitemaps() {
        let server = MockServer::start().await;
        robots(200, "User-agent: *\nAllow: /\nSitemap: /sitemap_index.xml\nSitemap: https://cdn.example.com/s.xml\n")
            .mount(&server)
            .await;
        let (cache, _, base) = cache_for(&server).await;
        let sitemaps: Vec<String> = cache
            .sitemaps(&base)
            .await
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            sitemaps,
            [
                format!("{}/sitemap_index.xml", server.uri()),
                "https://cdn.example.com/s.xml".into()
            ]
        );
    }

    #[tokio::test]
    async fn client_error_allows_everything() {
        let server = MockServer::start().await;
        robots(404, "").mount(&server).await;
        let (cache, _, base) = cache_for(&server).await;
        assert!(cache.check(&base.join("/anything").unwrap()).await.allowed);
    }

    #[tokio::test]
    async fn server_error_disallows() {
        let server = MockServer::start().await;
        robots(503, "").mount(&server).await;
        let (cache, _, base) = cache_for(&server).await;
        assert!(!cache.check(&base.join("/anything").unwrap()).await.allowed);
    }

    #[tokio::test]
    async fn follows_redirects() {
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/real-robots.txt"))
            .mount(&server)
            .await;
        Mock::given(path("/real-robots.txt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n"),
            )
            .mount(&server)
            .await;
        let (cache, _, base) = cache_for(&server).await;
        assert!(!cache.check(&base.join("/x").unwrap()).await.allowed);
    }
}
