//! Heuristic link scoring (v1, see `brainstorms/01-crawl-strategy.md`). Pure functions,
//! so the weights are easy to test and tune. A higher score means crawled sooner; `None`
//! means never crawl.
//! Seeds bypass scoring entirely; this is only for discovered links.

use career_core::domains::DomainStatus;
use url::Url;

use crate::ats;

const BASE: f64 = 10.0;
const ATS: f64 = 80.0;
const CAREERS_PATH: f64 = 50.0;
const CAREERS_ANCHOR: f64 = 40.0;
const CAREERS_BOTH: f64 = 60.0;
const EXTERNAL: f64 = 8.0;
const COMPANY_PAGE: f64 = 4.0;
const DEPTH_PENALTY: f64 = 2.0;
const SATURATION_PENALTY: f64 = 0.5;
const MAX_SATURATION_PENALTY: f64 = 10.0;
const NOFOLLOW_PENALTY: f64 = 5.0;
const QUERY_PENALTY: f64 = 2.0;
const ARCHIVE_PENALTY: f64 = 8.0;
const CONTENT_SECTION_PENALTY: f64 = 4.0;
pub const COMPANY_DOMAIN: f64 = 10.0;
pub const NOT_COMPANY_PENALTY: f64 = 15.0;

const MAX_URL_LEN: usize = 2000;
const MAX_PATH_SEGMENTS: usize = 12;

/// Registrable domains never worth crawling: social networks, job aggregators (their ToS
/// forbid scraping, and we want primary sources), link shorteners, code hosts and other
/// big platforms that are crawler traps.
const BLOCKED_DOMAINS: &[&str] = &[
    "facebook.com",
    "fb.com",
    "instagram.com",
    "twitter.com",
    "x.com",
    "t.co",
    "linkedin.com",
    "youtube.com",
    "youtu.be",
    "tiktok.com",
    "pinterest.com",
    "reddit.com",
    "snapchat.com",
    "whatsapp.com",
    "wa.me",
    "t.me",
    "telegram.me",
    "telegram.org",
    "discord.gg",
    "discord.com",
    "threads.net",
    "medium.com",
    "substack.com",
    "wikipedia.org",
    "wikimedia.org",
    "google.com",
    "goo.gl",
    "bit.ly",
    "github.com",
    "gitlab.com",
    "stackoverflow.com",
    "npmjs.com",
    "amazon.com",
    "indeed.com",
    "glassdoor.com",
    "ziprecruiter.com",
    "monster.com",
    "careerbuilder.com",
    "simplyhired.com",
    "wellfound.com",
    "angel.co",
    "jobberman.com",
    "calendly.com",
    "typeform.com",
    "zoom.us",
    "list-manage.com",
];
const BLOCKED_HOSTS: &[&str] = &["apps.apple.com", "itunes.apple.com", "play.google.com"];

const SKIPPED_EXTENSIONS: &[&str] = &[
    "pdf", "jpg", "jpeg", "png", "gif", "svg", "webp", "avif", "ico", "bmp", "tif", "tiff", "mp4",
    "mov", "avi", "webm", "mp3", "wav", "ogg", "zip", "gz", "tar", "rar", "7z", "dmg", "exe",
    "msi", "apk", "css", "js", "json", "xml", "rss", "atom", "woff", "woff2", "ttf", "otf", "eot",
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "txt", "ics",
];

/// Path segments that mean "not content": auth, commerce, CMS internals, share widgets.
const DROPPED_SEGMENTS: &[&str] = &[
    "login",
    "log-in",
    "signin",
    "sign-in",
    "signup",
    "sign-up",
    "register",
    "logout",
    "cart",
    "checkout",
    "basket",
    "account",
    "my-account",
    "wp-admin",
    "wp-login.php",
    "wp-json",
    "cdn-cgi",
    "xmlrpc.php",
    "feed",
    "share",
    "sharer",
    "print",
];

const ARCHIVE_SEGMENTS: &[&str] = &[
    "tag",
    "tags",
    "category",
    "categories",
    "author",
    "search",
    "page",
];
const ARCHIVE_PARAMS: &[&str] = &[
    "page",
    "p",
    "sort",
    "order",
    "filter",
    "replytocom",
    "s",
    "q",
];
const CONTENT_SECTIONS: &[&str] = &[
    "blog",
    "news",
    "press",
    "events",
    "articles",
    "resources",
    "podcast",
];
const COMPANY_SEGMENTS: &[&str] = &[
    "about",
    "about-us",
    "company",
    "team",
    "our-team",
    "our-story",
    "who-we-are",
    "contact",
    "contact-us",
];

/// Careers vocabulary, matched against whole path segments or their dash/underscore parts.
const CAREERS_TOKENS: &[&str] = &[
    "careers",
    "career",
    "jobs",
    "job",
    "joinus",
    "join-us",
    "work-with-us",
    "workwithus",
    "hiring",
    "we-are-hiring",
    "vacancies",
    "vacancy",
    "openings",
    "open-positions",
    "open-roles",
    "positions",
    "opportunities",
    "recruitment",
    "karriere",
    "emplois",
    "empleo",
    "vacatures",
    "stellenangebote",
    "lavora-con-noi",
    "trabalhe-conosco",
];
/// Careers phrases, matched as substrings of the lowercased anchor text.
const CAREERS_PHRASES: &[&str] = &[
    "career",
    "jobs",
    "job openings",
    "join us",
    "join our team",
    "we're hiring",
    "we are hiring",
    "work with us",
    "open roles",
    "open positions",
    "vacancies",
    "karriere",
];

#[derive(Debug, Clone, Copy)]
pub struct LinkInput<'a> {
    pub url: &'a Url,
    pub anchor_text: &'a str,
    pub nofollow: bool,
    pub source_domain: &'a str,
    pub target_domain: Option<&'a str>,
    /// Depth the target would have (source depth + 1).
    pub depth: u32,
    /// Pages already fetched or dispatched against the target's budget (see `crawl::budget_key`).
    pub target_domain_pages: u32,
    /// Classification of the target's registrable domain so far.
    pub target_status: DomainStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub score: f64,
    /// Which signals fired, stored in `frontier.reason` so scoring can be debugged.
    pub reasons: Vec<&'static str>,
}

impl Scored {
    pub fn reason(&self) -> String {
        return self.reasons.join(",");
    }
}

pub fn is_blocked(url: &Url, domain: Option<&str>) -> bool {
    let host = url.host_str().unwrap_or_default();
    return BLOCKED_HOSTS.contains(&host) || domain.is_some_and(|d| BLOCKED_DOMAINS.contains(&d));
}

pub fn score_link(input: &LinkInput) -> Option<Scored> {
    let url = input.url;
    if url.as_str().len() > MAX_URL_LEN || is_blocked(url, input.target_domain) {
        return None;
    }

    let segments: Vec<String> = url
        .path_segments()
        .map(|s| {
            s.filter(|seg| !seg.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    if segments.len() > MAX_PATH_SEGMENTS || has_repeated_segment(&segments) {
        return None;
    }
    let extension = segments
        .last()
        .and_then(|last| last.rsplit_once('.'))
        .map(|(_, e)| e);
    if extension.is_some_and(|ext| SKIPPED_EXTENSIONS.contains(&ext)) {
        return None;
    }
    if segments
        .iter()
        .any(|s| DROPPED_SEGMENTS.contains(&s.as_str()))
    {
        return None;
    }

    let mut score = BASE;
    let mut reasons = Vec::new();
    let mut add = |points: f64, reason: &'static str| {
        score += points;
        reasons.push(reason);
    };

    let board = ats::is_board(url);
    if board {
        add(ATS, "ats");
    }

    let (careers_path, careers_anchor) = careers_match(url, input.anchor_text);
    match (careers_path, careers_anchor) {
        (true, true) => add(CAREERS_BOTH, "careers_path+anchor"),
        (true, false) => add(CAREERS_PATH, "careers_path"),
        (false, true) => add(CAREERS_ANCHOR, "careers_anchor"),
        (false, false) => {}
    }

    let internal = input.target_domain == Some(input.source_domain);
    if !internal {
        add(EXTERNAL, "external");
    } else if segments
        .iter()
        .any(|s| COMPANY_SEGMENTS.contains(&s.as_str()))
    {
        add(COMPANY_PAGE, "company_page");
    }

    if segments
        .iter()
        .any(|s| ARCHIVE_SEGMENTS.contains(&s.as_str()))
        || looks_like_date_archive(&segments)
        || url
            .query_pairs()
            .any(|(k, _)| ARCHIVE_PARAMS.contains(&k.to_ascii_lowercase().as_str()))
    {
        add(-ARCHIVE_PENALTY, "archive");
    } else if url.query().is_some() {
        add(-QUERY_PENALTY, "query");
    }
    if segments
        .first()
        .is_some_and(|s| CONTENT_SECTIONS.contains(&s.as_str()))
    {
        add(-CONTENT_SECTION_PENALTY, "content_section");
    }
    // ATS boards live on the vendor's domain, so its classification says nothing about them.
    if !board {
        match input.target_status {
            DomainStatus::Company => add(COMPANY_DOMAIN, "company_domain"),
            DomainStatus::NotCompany => add(-NOT_COMPANY_PENALTY, "not_company"),
            DomainStatus::Discovered | DomainStatus::Probing => {}
        }
    }
    if input.nofollow {
        add(-NOFOLLOW_PENALTY, "nofollow");
    }
    if input.depth > 0 {
        add(-DEPTH_PENALTY * f64::from(input.depth), "depth");
    }
    if input.target_domain_pages > 0 {
        let penalty =
            (SATURATION_PENALTY * f64::from(input.target_domain_pages)).min(MAX_SATURATION_PENALTY);
        add(-penalty, "saturation");
    }

    return Some(Scored { score, reasons });
}

/// Whether a link looks like a careers link, by URL (path segment or `careers.`/`jobs.`
/// subdomain) and by anchor text: `(in_url, in_anchor)`.
pub fn careers_match(url: &Url, anchor_text: &str) -> (bool, bool) {
    let in_path = url
        .path_segments()
        .is_some_and(|mut segs| segs.any(|s| is_careers_segment(&s.to_ascii_lowercase())));
    let in_subdomain = url
        .host_str()
        .and_then(|h| h.split('.').next())
        .is_some_and(|sub| matches!(sub, "careers" | "jobs"));
    let anchor = anchor_text.to_lowercase();
    let in_anchor = CAREERS_PHRASES.iter().any(|p| anchor.contains(p));
    return (in_path || in_subdomain, in_anchor);
}

fn is_careers_segment(segment: &str) -> bool {
    if CAREERS_TOKENS.contains(&segment) {
        return true;
    }
    return segment
        .split(['-', '_', '.'])
        .any(|part| matches!(part, "careers" | "career" | "jobs" | "hiring" | "vacancies"));
}

/// `/a/b/a/b/a` style loops generated by relative-link bugs.
fn has_repeated_segment(segments: &[String]) -> bool {
    return segments
        .iter()
        .any(|s| segments.iter().filter(|other| *other == s).count() >= 3);
}

/// `/2023/05/...` blog archives.
fn looks_like_date_archive(segments: &[String]) -> bool {
    return segments.windows(2).any(|w| {
        w[0].len() == 4
            && w[0].chars().all(|c| c.is_ascii_digit())
            && w[1].len() <= 2
            && w[1].chars().all(|c| c.is_ascii_digit())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use career_core::urls::registrable_domain;

    fn score(url: &str, anchor: &str) -> Option<Scored> {
        return score_with(url, anchor, |_| {});
    }

    fn score_with(url: &str, anchor: &str, tweak: impl FnOnce(&mut LinkInput)) -> Option<Scored> {
        let url = Url::parse(url).unwrap();
        let domain = registrable_domain(&url);
        let mut input = LinkInput {
            url: &url,
            anchor_text: anchor,
            nofollow: false,
            source_domain: "acme.com",
            target_domain: domain.as_deref(),
            depth: 1,
            target_domain_pages: 0,
            target_status: DomainStatus::Discovered,
        };
        tweak(&mut input);
        return score_link(&input);
    }

    fn value(url: &str, anchor: &str) -> f64 {
        return score(url, anchor).unwrap().score;
    }

    #[test]
    fn careers_and_ats_links_rank_highest() {
        let ats = value("https://boards.greenhouse.io/acme", "Open roles");
        let ats_vendor_site = score("https://www.greenhouse.io/careers", "Careers").unwrap();
        assert!(!ats_vendor_site.reasons.contains(&"ats"));
        let careers = value("https://acme.com/careers", "Careers");
        let careers_path_only = value("https://acme.com/company/join-us", "Team");
        let anchor_only = value("https://acme.com/people", "We're hiring!");
        let about = value("https://acme.com/about", "About");
        let plain = value("https://acme.com/product", "Product");

        assert!(ats > careers, "{ats} > {careers}");
        assert!(careers > careers_path_only && careers > anchor_only);
        assert!(careers_path_only > about && anchor_only > about);
        assert!(about > plain);
        assert!(
            score("https://careers.acme.com/", "")
                .unwrap()
                .reasons
                .contains(&"careers_path")
        );
    }

    #[test]
    fn external_links_get_a_discovery_bonus() {
        let external = score("https://paystack.com/", "Paystack").unwrap();
        assert!(external.reasons.contains(&"external"));
        assert!(external.score > value("https://acme.com/product", "Product"));
    }

    #[test]
    fn drops_blocked_files_and_junk() {
        for url in [
            "https://www.linkedin.com/company/acme",
            "https://ng.indeed.com/jobs?q=acme",
            "https://apps.apple.com/app/acme/id1",
            "https://acme.com/brochure.PDF",
            "https://acme.com/logo.png",
            "https://acme.com/login",
            "https://acme.com/shop/cart",
            "https://acme.com/wp-admin/x",
            "https://acme.com/a/b/a/b/a/b",
        ] {
            assert_eq!(score(url, "Careers"), None, "{url}");
        }
        let long = format!("https://acme.com/{}", "x".repeat(3000));
        assert_eq!(score(&long, ""), None);
    }

    #[test]
    fn archives_queries_and_content_sections_are_penalised() {
        let plain = value("https://acme.com/product", "");
        assert!(value("https://acme.com/blog/tag/rust", "") < plain);
        assert!(value("https://acme.com/2023/05/post", "") < plain);
        assert!(value("https://acme.com/list?page=3", "") < plain);
        assert!(value("https://acme.com/product?color=red", "") < plain);
        assert!(value("https://acme.com/news/launch", "") < plain);
        // A careers page under /blog is still worth more than a plain page.
        assert!(value("https://acme.com/blog/careers", "Careers") > plain);
    }

    #[test]
    fn depth_saturation_and_nofollow_lower_the_score() {
        let base = value("https://acme.com/product", "");
        let deep = score_with("https://acme.com/product", "", |i| i.depth = 4)
            .unwrap()
            .score;
        let saturated = score_with("https://acme.com/product", "", |i| {
            i.target_domain_pages = 100
        })
        .unwrap()
        .score;
        let nofollow = score_with("https://acme.com/product", "", |i| i.nofollow = true)
            .unwrap()
            .score;
        assert_eq!(base - deep, 6.0);
        assert_eq!(base - saturated, MAX_SATURATION_PENALTY);
        assert_eq!(base - nofollow, NOFOLLOW_PENALTY);
    }

    #[test]
    fn domain_status_shifts_scores_except_for_ats_boards() {
        let base = value("https://acme.com/product", "");
        let company = score_with("https://acme.com/product", "", |i| {
            i.target_status = DomainStatus::Company
        })
        .unwrap();
        let not_company = score_with("https://acme.com/product", "", |i| {
            i.target_status = DomainStatus::NotCompany
        })
        .unwrap();
        assert_eq!(company.score - base, COMPANY_DOMAIN);
        assert_eq!(base - not_company.score, NOT_COMPANY_PENALTY);
        let board = score_with("https://jobs.lever.co/acme", "", |i| {
            i.target_status = DomainStatus::NotCompany
        })
        .unwrap();
        assert!(!board.reasons.contains(&"not_company"));
    }

    #[test]
    fn reason_lists_signals() {
        let s = score("https://jobs.lever.co/acme", "Careers").unwrap();
        assert_eq!(s.reason(), "ats,careers_path+anchor,external,depth");
    }
}
