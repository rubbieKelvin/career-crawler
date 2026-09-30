//! Finding a company's careers page and ATS board (see `brainstorms/02-career-page-detection.md`),
//! in order of preference:
//! 1. careers links on its own pages (these already score high in the frontier)
//! 2. an ATS board embedded in or linked from its pages, attributed only when it plausibly
//!    belongs to this company
//! 3. probing well-known paths (`/careers`, `/jobs`, `careers.<domain>`)
//! 4. scanning its sitemap for careers URLs

use std::collections::{HashSet, VecDeque};

use career_core::urls;
use url::Url;

use crate::ats::{self, Board};
use crate::fetcher;
use crate::parse::{Link, ParsedPage};
use crate::scoring;
use crate::visit::Visitor;

/// Frontier score for an attributed ATS board: the best lead to actual jobs.
pub const BOARD_SCORE: f64 = 100.0;
/// Frontier score for well-known careers paths probed on a company without a careers link.
pub const PROBE_SCORE: f64 = 50.0;
/// Frontier score for careers URLs found in a sitemap.
pub const SITEMAP_SCORE: f64 = 55.0;

/// A careers landing page is shallow; deeper careers URLs are usually single postings.
const MAX_CAREERS_PATH_SEGMENTS: usize = 3;
const MAX_ROOT_SITEMAPS: usize = 2;
const MAX_CHILD_SITEMAPS: usize = 3;
const MAX_SITEMAP_RESULTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSource {
    Embed,
    Link,
}

impl BoardSource {
    pub fn as_str(self) -> &'static str {
        return match self {
            BoardSource::Embed => "ats_embed",
            BoardSource::Link => "ats_link",
        };
    }
}

fn is_internal(url: &Url, domain: &str) -> bool {
    return urls::registrable_domain(url).as_deref() == Some(domain);
}

/// Links on `page` that stay on `domain` and look like careers links.
pub fn careers_links<'a>(page: &'a ParsedPage, domain: &str) -> Vec<&'a Link> {
    return page
        .links
        .iter()
        .filter(|l| is_internal(&l.url, domain))
        .filter(|l| {
            let (in_url, in_anchor) = scoring::careers_match(&l.url, &l.text);
            return in_url || in_anchor;
        })
        .collect();
}

/// Whether `url` looks like `domain`'s careers landing page: on the domain itself (not
/// an ATS), with a careers path segment or a `careers.`/`jobs.` subdomain, shallow, and
/// not a single posting (`/jobs/8242245/...`, `/postings/<uuid>`).
pub fn is_careers_page(url: &Url, domain: &str) -> bool {
    if !is_internal(url, domain) || ats::is_board(url) {
        return false;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map_or(Vec::new(), |s| s.filter(|seg| !seg.is_empty()).collect());
    return segments.len() <= MAX_CAREERS_PATH_SEGMENTS
        && !segments.iter().any(|s| looks_like_id(s))
        && scoring::careers_match(url, "").0;
}

/// Posting identifiers: 5+ digits in a row (`8242245`, `job-12345`) or a UUID.
fn looks_like_id(segment: &str) -> bool {
    let mut run = 0;
    for c in segment.chars() {
        run = if c.is_ascii_digit() { run + 1 } else { 0 };
        if run >= 5 {
            return true;
        }
    }
    let groups: Vec<&str> = segment.split('-').collect();
    return groups.len() == 5
        && groups
            .iter()
            .all(|g| !g.is_empty() && g.chars().all(|c| c.is_ascii_hexdigit()));
}

/// Whether `url` is `domain`'s root page (`domain/` or `www.domain/`).
pub fn is_main_home(url: &Url, domain: &str) -> bool {
    let host = url.host_str().unwrap_or_default();
    return url.path() == "/" && (host == domain || host.strip_prefix("www.") == Some(domain));
}

/// Prefer the shallowest careers URL: `/careers` over `/careers/engineering`.
pub fn is_better_careers_url(existing: Option<&str>, candidate: &Url) -> bool {
    let Some(existing) = existing.and_then(|e| Url::parse(e).ok()) else {
        return true;
    };
    let depth = |u: &Url| {
        u.path_segments()
            .map_or(0, |s| s.filter(|seg| !seg.is_empty()).count())
    };
    return depth(candidate) < depth(&existing);
}

/// The ATS board this page shows to belong to `domain`, if any.
/// - Embedded boards: accepted if there's exactly one, or if its token matches the domain.
/// - Linked boards: accepted if the token matches the domain, or if it's the page's only
///   board and the link text says careers. A portfolio page linking to forty companies'
///   boards attributes none of them.
pub fn attributed_board(page: &ParsedPage, domain: &str) -> Option<(Board, BoardSource)> {
    let embedded = distinct_boards(page.embeds.iter());
    if let Some(board) = pick(&embedded, domain, |_| true) {
        return Some((board, BoardSource::Embed));
    }

    let linked = distinct_boards(page.links.iter().map(|l| &l.url));
    let careers_anchor = |board: &Board| {
        page.links.iter().any(|l| {
            ats::board(&l.url).is_some_and(|b| b.key() == board.key())
                && scoring::careers_match(&l.url, &l.text).1
        })
    };
    return pick(&linked, domain, careers_anchor).map(|b| (b, BoardSource::Link));
}

fn distinct_boards<'a>(urls: impl Iterator<Item = &'a Url>) -> Vec<Board> {
    let mut seen = HashSet::new();
    return urls
        .filter_map(ats::board)
        .filter(|b| seen.insert(b.key()))
        .collect();
}

fn pick(boards: &[Board], domain: &str, sole_ok: impl Fn(&Board) -> bool) -> Option<Board> {
    if let Some(own) = boards.iter().find(|b| b.matches_domain(domain)) {
        return Some(own.clone());
    }
    if let [only] = boards
        && sole_ok(only)
    {
        return Some(only.clone());
    }
    return None;
}

/// `scheme://domain/` for the page's registrable domain. Keeps the port for IP hosts and
/// keeps `www.` if the page was on it.
pub fn main_home_url(url: &Url, domain: &str) -> Option<Url> {
    let mut home = url.clone();
    home.set_path("/");
    home.set_query(None);
    home.set_fragment(None);
    let host = url.host_str()?;
    if host != domain && host.strip_prefix("www.") != Some(domain) {
        home.set_host(Some(domain)).ok()?;
        home.set_port(None).ok()?;
    }
    return Some(home);
}

/// Well-known careers locations to try on a company with no careers link.
pub fn probe_urls(page_url: &Url, domain: &str) -> Vec<Url> {
    let Some(home) = main_home_url(page_url, domain) else {
        return Vec::new();
    };
    let mut probes: Vec<Url> = ["/careers", "/jobs"]
        .iter()
        .filter_map(|p| home.join(p).ok())
        .collect();
    // A `careers.` subdomain only makes sense for a DNS name.
    if home.domain().is_some()
        && let Ok(sub) = Url::parse(&format!("https://careers.{domain}/"))
    {
        probes.push(sub);
    }
    return probes;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sitemap {
    pub locs: Vec<String>,
    /// A `<sitemapindex>`: its locs are more sitemaps, not pages.
    pub is_index: bool,
}

/// Extracts `<loc>` values from a sitemap or sitemap index. It does a plain text scan
/// rather than full XML parsing, which copes with the many slightly broken sitemaps out there.
pub fn parse_sitemap(xml: &str) -> Sitemap {
    let lower = xml.to_ascii_lowercase();
    let mut locs = Vec::new();
    let mut rest = 0;
    while let Some(start) = lower[rest..]
        .find("<loc>")
        .map(|i| rest + i + "<loc>".len())
    {
        let Some(end) = lower[start..].find("</loc>").map(|i| start + i) else {
            break;
        };
        let raw = xml[start..end].trim();
        let raw = raw
            .strip_prefix("<![CDATA[")
            .and_then(|r| r.strip_suffix("]]>"))
            .unwrap_or(raw)
            .trim();
        locs.push(raw.replace("&amp;", "&"));
        rest = end;
    }
    return Sitemap {
        locs,
        is_index: lower.contains("<sitemapindex"),
    };
}

/// Careers URLs among sitemap entries, shallowest first.
pub fn careers_from_sitemap(locs: &[String], domain: &str) -> Vec<Url> {
    let mut found: Vec<Url> = locs
        .iter()
        .filter_map(|l| Url::parse(l).ok())
        .map(|u| urls::normalize(&u))
        .filter(|u| is_careers_page(u, domain))
        .collect();
    found.sort_by_key(|u| {
        (
            u.path_segments()
                .map_or(0, |s| s.filter(|seg| !seg.is_empty()).count()),
            u.as_str().len(),
        )
    });
    found.dedup();
    found.truncate(MAX_SITEMAP_RESULTS);
    return found;
}

/// Scans `domain`'s sitemaps for careers URLs: those declared in robots.txt, else
/// `/sitemap.xml`, following one level of sitemap index. Fetches are polite and bounded.
pub async fn discover_via_sitemap(visitor: &Visitor, home: &Url, domain: &str) -> Vec<Url> {
    let mut roots = visitor.robots_sitemaps(home).await;
    if roots.is_empty() {
        roots.extend(home.join("/sitemap.xml").ok());
    }
    roots.truncate(MAX_ROOT_SITEMAPS);

    let mut queue: VecDeque<Url> = roots.into();
    let mut children_taken = 0;
    let mut locs = Vec::new();
    while let Some(sitemap_url) = queue.pop_front() {
        let Some(body) = visitor.fetch_resource(&sitemap_url).await else {
            continue;
        };
        // `.xml.gz` sitemaps arrive as gzip files rather than with Content-Encoding.
        let body = if body.starts_with(&[0x1f, 0x8b]) {
            match fetcher::decompress(Some("gzip"), body.to_vec(), 50 * 1024 * 1024) {
                Ok(b) => b,
                Err(_) => continue,
            }
        } else {
            body.to_vec()
        };
        let sitemap = parse_sitemap(&String::from_utf8_lossy(&body));
        if !sitemap.is_index {
            locs.extend(sitemap.locs);
            continue;
        }
        // Children named like careers/jobs first, then pages; posts and products last.
        let mut children: Vec<Url> = sitemap
            .locs
            .iter()
            .filter_map(|l| Url::parse(l).ok())
            .collect();
        children.sort_by_key(|u| {
            let path = u.path().to_ascii_lowercase();
            return if path.contains("career") || path.contains("job") {
                0
            } else if path.contains("page") {
                1
            } else {
                2
            };
        });
        for child in children {
            if children_taken >= MAX_CHILD_SITEMAPS {
                break;
            }
            children_taken += 1;
            queue.push_back(child);
        }
    }
    return careers_from_sitemap(&locs, domain);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::metrics::Metrics;
    use crate::parse::parse_html;
    use career_core::config::CrawlerConfig;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn page(url: &str, html: &str) -> ParsedPage {
        return parse_html(&Url::parse(url).unwrap(), html);
    }

    fn u(s: &str) -> Url {
        return Url::parse(s).unwrap();
    }

    #[test]
    fn finds_internal_careers_links_only() {
        let p = page(
            "https://acme.com/",
            r#"<a href="/careers">Careers</a><a href="/about">About</a>
               <a href="https://news.site/jobs">Jobs report</a><a href="/team">Join our team</a>"#,
        );
        let found: Vec<&str> = careers_links(&p, "acme.com")
            .iter()
            .map(|l| l.url.path())
            .collect();
        assert_eq!(found, ["/careers", "/team"]);
    }

    #[test]
    fn careers_page_detection() {
        assert!(is_careers_page(&u("https://acme.com/careers"), "acme.com"));
        assert!(is_careers_page(
            &u("https://www.acme.com/company/join-us/"),
            "acme.com"
        ));
        assert!(is_careers_page(&u("https://careers.acme.com/"), "acme.com"));
        assert!(!is_careers_page(
            &u("https://acme.com/careers/eng/backend/senior-engineer"),
            "acme.com"
        ));
        assert!(!is_careers_page(
            &u("https://other.com/careers"),
            "acme.com"
        ));
        assert!(!is_careers_page(&u("https://acme.com/about"), "acme.com"));
        assert!(!is_careers_page(
            &u("https://www.pinterestcareers.com/jobs/8242245/sr-partner/"),
            "pinterestcareers.com"
        ));
        assert!(!is_careers_page(
            &u("https://careers.fenris.com/en/postings/d7634664-ad84-48e6-b0c9-f5eb3afe90b7"),
            "fenris.com"
        ));
        assert!(is_careers_page(
            &u("https://careers.fenris.com/en"),
            "fenris.com"
        ));
        assert!(is_main_home(&u("https://www.acme.com/"), "acme.com"));
        assert!(!is_main_home(&u("https://blog.acme.com/"), "acme.com"));
        assert!(!is_main_home(&u("https://acme.com/us/"), "acme.com"));

        assert!(is_better_careers_url(
            None,
            &u("https://acme.com/careers/eng")
        ));
        assert!(is_better_careers_url(
            Some("https://acme.com/careers/eng"),
            &u("https://acme.com/careers")
        ));
        assert!(!is_better_careers_url(
            Some("https://acme.com/careers"),
            &u("https://acme.com/jobs")
        ));
    }

    #[test]
    fn embedded_board_is_attributed() {
        let p = page(
            "https://acme.com/careers",
            r#"<div id="grnhse_app"></div><script src="https://boards.greenhouse.io/embed/job_board/js?for=acmehq"></script>"#,
        );
        let (board, source) = attributed_board(&p, "acme.com").unwrap();
        assert_eq!(
            (board.key().as_str(), source),
            ("greenhouse/acmehq", BoardSource::Embed)
        );
    }

    #[test]
    fn linked_board_needs_a_name_match_or_careers_text() {
        let own = page(
            "https://stripe.com/",
            r#"<a href="https://jobs.lever.co/stripe">Team</a>"#,
        );
        assert_eq!(
            attributed_board(&own, "stripe.com").unwrap().0.key(),
            "lever/stripe"
        );

        // No name match, but it's the only board and the link text says careers.
        let sole = page(
            "https://acme.com/",
            r#"<a href="https://jobs.ashbyhq.com/zeta-labs">We're hiring</a>"#,
        );
        assert_eq!(
            attributed_board(&sole, "acme.com").unwrap().0.key(),
            "ashby/zeta-labs"
        );

        let unrelated = page(
            "https://news.com/",
            r#"<a href="https://jobs.lever.co/kong">Kong raises $50M</a>"#,
        );
        assert_eq!(attributed_board(&unrelated, "news.com"), None);

        let portfolio = page(
            "https://a16z.com/portfolio",
            r#"<a href="https://jobs.ashbyhq.com/kong">Jobs</a><a href="https://jobs.lever.co/harvey">Jobs</a>"#,
        );
        assert_eq!(attributed_board(&portfolio, "a16z.com"), None);
    }

    #[test]
    fn probes_and_home() {
        let probes: Vec<String> = probe_urls(&u("https://www.acme.com/about?x=1"), "acme.com")
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            probes,
            [
                "https://www.acme.com/careers",
                "https://www.acme.com/jobs",
                "https://careers.acme.com/"
            ]
        );
        assert_eq!(
            main_home_url(&u("https://blog.acme.com/p"), "acme.com")
                .unwrap()
                .as_str(),
            "https://acme.com/"
        );
        let ip: Vec<String> = probe_urls(&u("http://127.0.0.1:8080/x"), "127.0.0.1")
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            ip,
            [
                "http://127.0.0.1:8080/careers",
                "http://127.0.0.1:8080/jobs"
            ]
        );
    }

    #[test]
    fn parses_sitemaps_and_indexes() {
        let urlset = r#"<?xml version="1.0"?><urlset>
            <url><loc> https://acme.com/about </loc></url>
            <url><LOC><![CDATA[https://acme.com/careers/engineering?a=1&amp;b=2]]></LOC></url>
            <url><loc>https://acme.com/careers</loc></url></urlset>"#;
        let sitemap = parse_sitemap(urlset);
        assert!(!sitemap.is_index);
        assert_eq!(
            sitemap.locs[1],
            "https://acme.com/careers/engineering?a=1&b=2"
        );
        let found = careers_from_sitemap(&sitemap.locs, "acme.com");
        let paths: Vec<&str> = found.iter().map(Url::path).collect();
        assert_eq!(paths, ["/careers", "/careers/engineering"]);

        let index = parse_sitemap(
            "<sitemapindex><sitemap><loc>https://acme.com/s1.xml</loc></sitemap></sitemapindex>",
        );
        assert!(index.is_index);
        assert_eq!(index.locs, ["https://acme.com/s1.xml"]);
    }

    #[tokio::test]
    async fn sitemap_discovery_follows_robots_and_indexes() {
        let server = MockServer::start().await;
        let base = server.uri();
        let xml = |body: String| ResponseTemplate::new(200).set_body_raw(body, "application/xml");
        Mock::given(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("User-agent: *\nSitemap: {base}/index.xml\n")),
            )
            .mount(&server)
            .await;
        Mock::given(path("/index.xml"))
            .respond_with(xml(format!(
                "<sitemapindex><sitemap><loc>{base}/posts.xml</loc></sitemap><sitemap><loc>{base}/pages.xml</loc></sitemap></sitemapindex>"
            )))
            .mount(&server)
            .await;
        Mock::given(path("/pages.xml"))
            .respond_with(xml(format!("<urlset><url><loc>{base}/about</loc></url><url><loc>{base}/work-with-us</loc></url></urlset>")))
            .mount(&server)
            .await;
        Mock::given(path("/posts.xml"))
            .respond_with(xml(format!(
                "<urlset><url><loc>{base}/2024/01/post</loc></url></urlset>"
            )))
            .mount(&server)
            .await;

        let config = CrawlerConfig {
            per_host_delay_ms: 0,
            ..CrawlerConfig::default()
        };
        let visitor = Visitor::new(&config, Arc::new(Metrics::default())).unwrap();
        let found = discover_via_sitemap(&visitor, &u(&base), "127.0.0.1").await;
        assert_eq!(
            found.iter().map(Url::path).collect::<Vec<_>>(),
            ["/work-with-us"]
        );
    }
}
