//! One logical page visit: robots.txt check, per-host politeness, fetch, redirect
//! following (re-checking robots and politeness on every hop), then parse.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use areer_core::config::CrawlerConfig;
use areer_core::urls;
use reqwest::StatusCode;
use url::Url;

use crate::fetcher::{FetchError, Fetcher, Response, Want};
use crate::metrics::Metrics;
use crate::parse::{self, ParsedPage};
use crate::politeness::HostGate;
use crate::robots::RobotsCache;

#[derive(Debug)]
pub enum Outcome {
    Page {
        status: StatusCode,
        parsed: Box<ParsedPage>,
        bytes_wire: u64,
        bytes_body: u64,
        /// BLAKE3 of the decoded body (hex, 128 bits), for duplicate detection.
        content_hash: String,
        elapsed: Duration,
    },
    NotHtml {
        content_type: Option<String>,
    },
    HttpError {
        status: StatusCode,
    },
    RobotsDenied,
    TooManyRedirects,
    Failed(FetchError),
}

#[derive(Debug)]
pub struct Visit {
    pub requested: Url,
    /// Where the visit ended: after redirects, or where it was denied or failed.
    pub final_url: Url,
    /// Each redirect target, in order.
    pub redirects: Vec<Url>,
    pub outcome: Outcome,
}

pub struct Visitor {
    fetcher: Arc<Fetcher>,
    robots: RobotsCache,
    gate: HostGate,
    max_redirects: u8,
}

impl Visitor {
    pub fn new(config: &CrawlerConfig, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        let fetcher = Arc::new(Fetcher::new(config, metrics.clone())?);
        let robots = RobotsCache::new(
            fetcher.clone(),
            metrics,
            config.robots_agent(),
            Duration::from_secs(config.robots_ttl_secs),
        );
        return Ok(Self {
            fetcher,
            robots,
            gate: HostGate::new(Duration::from_millis(config.per_host_delay_ms)),
            max_redirects: config.max_redirects,
        });
    }

    /// The shared per-host gate, so the scheduler can avoid dispatching to busy hosts.
    pub fn gate(&self) -> &HostGate {
        return &self.gate;
    }

    pub async fn visit(&self, url: &Url) -> Visit {
        let requested = urls::normalize(url);
        let (final_url, redirects, fetched) = self.fetch_following(&requested, Want::Html).await;
        let outcome = match fetched {
            Fetched::RobotsDenied => Outcome::RobotsDenied,
            Fetched::TooManyRedirects => Outcome::TooManyRedirects,
            Fetched::Failed(e) => Outcome::Failed(e),
            Fetched::Response(resp) if !resp.status.is_success() => Outcome::HttpError {
                status: resp.status,
            },
            Fetched::Response(resp) if resp.skipped => Outcome::NotHtml {
                content_type: resp.content_type().map(str::to_string),
            },
            Fetched::Response(resp) => {
                let html = parse::decode_html(&resp.body, resp.content_type());
                Outcome::Page {
                    status: resp.status,
                    parsed: Box::new(parse::parse_html(&final_url, &html)),
                    bytes_wire: resp.bytes_wire,
                    bytes_body: resp.body.len() as u64,
                    content_hash: content_hash(&resp.body),
                    elapsed: resp.elapsed,
                }
            }
        };
        return Visit {
            requested,
            final_url,
            redirects,
            outcome,
        };
    }

    /// Fetches a non-page resource (sitemaps, ATS APIs) under the same robots.txt,
    /// politeness and redirect rules as pages. Returns the body of a 2xx response, or a
    /// short reason: `http_<status>`, `robots_denied`, `too_many_redirects`, or a fetch
    /// error kind.
    pub async fn fetch_resource(&self, url: &Url) -> Result<Bytes, String> {
        return match self.fetch_following(url, Want::Any).await.2 {
            Fetched::Response(resp) if resp.status.is_success() => Ok(resp.body),
            Fetched::Response(resp) => Err(format!("http_{}", resp.status.as_u16())),
            Fetched::RobotsDenied => Err("robots_denied".into()),
            Fetched::TooManyRedirects => Err("too_many_redirects".into()),
            Fetched::Failed(e) => Err(e.kind().into()),
        };
    }

    /// `Sitemap:` URLs from the robots.txt of `url`'s origin.
    pub async fn robots_sitemaps(&self, url: &Url) -> Vec<Url> {
        return self.robots.sitemaps(url).await;
    }

    /// The request loop shared by pages and resources: per hop, check robots.txt, wait for
    /// the host gate, fetch, and follow redirects (up to `max_redirects`). Returns the final
    /// URL, the redirect chain, and how it ended.
    async fn fetch_following(&self, start: &Url, want: Want) -> (Url, Vec<Url>, Fetched) {
        let mut current = start.clone();
        let mut redirects = Vec::new();
        let fetched = loop {
            let host = urls::host_key(&current);
            let robots = self.robots.check(&current).await;
            self.gate.set_crawl_delay(&host, robots.crawl_delay);
            if !robots.allowed {
                break Fetched::RobotsDenied;
            }

            let permit = self.gate.acquire(&host).await;
            let result = self.fetcher.fetch(&current, want).await;
            drop(permit);

            let resp = match result {
                Ok(resp) => resp,
                Err(e) => break Fetched::Failed(e),
            };
            if let Some(target) = resp.redirect_target() {
                if redirects.len() >= self.max_redirects as usize {
                    break Fetched::TooManyRedirects;
                }
                current = urls::normalize(&target);
                redirects.push(current.clone());
                continue;
            }
            break Fetched::Response(Box::new(resp));
        };
        return (current, redirects, fetched);
    }
}

enum Fetched {
    Response(Box<Response>),
    RobotsDenied,
    TooManyRedirects,
    Failed(FetchError),
}

fn content_hash(body: &[u8]) -> String {
    return blake3::hash(body).to_hex()[..32].to_string();
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn visitor() -> Visitor {
        let config = CrawlerConfig {
            per_host_delay_ms: 0,
            max_redirects: 2,
            ..CrawlerConfig::default()
        };
        return Visitor::new(&config, Arc::new(Metrics::default())).unwrap();
    }

    async fn server_with_robots(robots: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(robots))
            .mount(&server)
            .await;
        return server;
    }

    fn html(body: &str) -> ResponseTemplate {
        return ResponseTemplate::new(200).set_body_raw(body, "text/html");
    }

    fn redirect(to: &str) -> ResponseTemplate {
        return ResponseTemplate::new(302).insert_header("location", to);
    }

    #[tokio::test]
    async fn follows_redirect_and_parses() {
        let server = server_with_robots("User-agent: *\nAllow: /\n").await;
        Mock::given(path("/jobs"))
            .respond_with(redirect("/careers"))
            .mount(&server)
            .await;
        Mock::given(path("/careers"))
            .respond_with(html(
                "<title>Careers</title><a href='/careers/eng'>Engineering</a>",
            ))
            .mount(&server)
            .await;

        let start = Url::parse(&server.uri()).unwrap().join("/jobs").unwrap();
        let visit = visitor().visit(&start).await;
        assert_eq!(visit.final_url.path(), "/careers");
        assert_eq!(visit.redirects.len(), 1);
        let Outcome::Page { parsed, .. } = visit.outcome else {
            panic!("expected page, got {:?}", visit.outcome);
        };
        assert_eq!(parsed.title.as_deref(), Some("Careers"));
        assert_eq!(parsed.links[0].url.path(), "/careers/eng");
    }

    #[tokio::test]
    async fn robots_is_rechecked_after_redirect() {
        let server = server_with_robots("User-agent: *\nDisallow: /secret\n").await;
        Mock::given(path("/open"))
            .respond_with(redirect("/secret/page"))
            .mount(&server)
            .await;
        Mock::given(path("/secret/page"))
            .respond_with(html("should not be fetched"))
            .expect(0)
            .mount(&server)
            .await;

        let start = Url::parse(&server.uri()).unwrap().join("/open").unwrap();
        let visit = visitor().visit(&start).await;
        assert!(matches!(visit.outcome, Outcome::RobotsDenied));
        assert_eq!(visit.final_url.path(), "/secret/page");
    }

    #[tokio::test]
    async fn stops_redirect_loops() {
        let server = server_with_robots("").await;
        Mock::given(path("/a"))
            .respond_with(redirect("/b"))
            .mount(&server)
            .await;
        Mock::given(path("/b"))
            .respond_with(redirect("/a"))
            .mount(&server)
            .await;
        let start = Url::parse(&server.uri()).unwrap().join("/a").unwrap();
        let visit = visitor().visit(&start).await;
        assert!(matches!(visit.outcome, Outcome::TooManyRedirects));
        assert_eq!(visit.redirects.len(), 2);
    }

    #[tokio::test]
    async fn reports_http_errors_and_non_html() {
        let server = server_with_robots("").await;
        Mock::given(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(path("/doc"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("content-type", "application/pdf"),
            )
            .mount(&server)
            .await;
        let base = Url::parse(&server.uri()).unwrap();
        let v = visitor();

        let gone = v.visit(&base.join("/gone").unwrap()).await;
        assert!(
            matches!(gone.outcome, Outcome::HttpError { status } if status == StatusCode::NOT_FOUND)
        );
        let doc = v.visit(&base.join("/doc").unwrap()).await;
        assert!(
            matches!(doc.outcome, Outcome::NotHtml { content_type: Some(ref ct) } if ct == "application/pdf")
        );
    }
}
