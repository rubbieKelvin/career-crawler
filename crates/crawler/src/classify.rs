//! Heuristic "is this a company site?" classifier (see `brainstorms/02-career-page-detection.md`).
//! It scores a single parsed page in [0, 1]. The store keeps the best page score per domain
//! and turns it into a `DomainStatus`. Pages in the gray zone between the thresholds are
//! what the LLM will look at in milestone 10.

use career_core::urls;
use serde_json::Value;
use url::Url;

use crate::ats;
use crate::parse::ParsedPage;
use crate::scoring;

/// At or above this, the domain is a company and gets the harvest budget.
pub const COMPANY_THRESHOLD: f64 = 0.6;
/// Below this, once the homepage has been seen, the domain is not a company.
pub const NOT_COMPANY_THRESHOLD: f64 = 0.3;

const ORG_JSON_LD: f64 = 0.25;
const SITE_NAME: f64 = 0.05;
const LEGAL_SUFFIX: f64 = 0.2;
const COPYRIGHT: f64 = 0.1;
const PRIVACY_LINK: f64 = 0.1;
const TERMS_LINK: f64 = 0.05;
const ABOUT_LINK: f64 = 0.1;
const CONTACT_LINK: f64 = 0.05;
const CAREERS_LINK: f64 = 0.2;
const HTTPS: f64 = 0.05;
const SOME_INBOUND: f64 = 0.05;
const MANY_INBOUND: f64 = 0.1;

const THIN_TEXT_BYTES: usize = 300;
const THIN_LINKS: usize = 5;
const PARKED_SCAN_CHARS: usize = 5_000;

/// Company-form words seen in footers ("© 2026 Acme Ltd."). Short forms that are also
/// common words (as, sa, ab) are left out on purpose.
const LEGAL_SUFFIXES: &[&str] = &[
    "inc",
    "incorporated",
    "ltd",
    "limited",
    "llc",
    "llp",
    "lp",
    "plc",
    "corp",
    "corporation",
    "gmbh",
    "ag",
    "bv",
    "nv",
    "pty",
    "sarl",
    "srl",
    "oy",
    "kk",
];

const PARKED_PHRASES: &[&str] = &[
    "domain is for sale",
    "domain may be for sale",
    "buy this domain",
    "this domain is parked",
    "domain parking",
    "parked free",
    "hugedomains.com",
    "sedo domain parking",
    "afternic",
    "dan.com",
];

const ABOUT_SEGMENTS: &[&str] = &[
    "about",
    "about-us",
    "company",
    "who-we-are",
    "our-story",
    "team",
    "our-team",
];
const CONTACT_SEGMENTS: &[&str] = &["contact", "contact-us"];
const TERMS_SEGMENTS: &[&str] = &[
    "terms",
    "tos",
    "terms-of-service",
    "terms-of-use",
    "terms-and-conditions",
    "legal",
    "imprint",
    "impressum",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub score: f64,
    pub signals: Vec<&'static str>,
    pub parked: bool,
    /// False for thin pages (JS shells, stubs): too little content to conclude "not a
    /// company" from. Milestone 13's headless browser should render these.
    pub conclusive: bool,
    /// The domain's own JSON-LD organization, else `og:site_name`, else any JSON-LD organization.
    pub name: Option<String>,
}

pub fn assess(page_url: &Url, domain: &str, page: &ParsedPage, inbound_domains: u32) -> Assessment {
    let mut score = 0.0;
    let mut signals = Vec::new();
    let mut add = |points: f64, signal: &'static str| {
        score += points;
        signals.push(signal);
    };

    let scan: String = page
        .text
        .chars()
        .take(PARKED_SCAN_CHARS)
        .collect::<String>()
        .to_lowercase()
        + " "
        + &page.title.as_deref().unwrap_or_default().to_lowercase();
    if PARKED_PHRASES.iter().any(|p| scan.contains(p)) {
        return Assessment {
            score: 0.0,
            signals: vec!["parked"],
            parked: true,
            conclusive: true,
            name: None,
        };
    }

    let mut orgs = Vec::new();
    for value in &page.json_ld {
        collect_organizations(value, &mut orgs);
    }
    if !orgs.is_empty() {
        add(ORG_JSON_LD, "org_json_ld");
    }
    if page.og_site_name.is_some() {
        add(SITE_NAME, "site_name");
    }

    let footer = page.footer_text.to_lowercase();
    let has_legal_suffix = footer
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| LEGAL_SUFFIXES.contains(&token));
    if has_legal_suffix {
        add(LEGAL_SUFFIX, "legal_suffix");
    }
    if footer.contains('©') || footer.contains("copyright") || footer.contains("(c) 20") {
        add(COPYRIGHT, "copyright");
    }

    let mut links = LinkSignals::default();
    for link in &page.links {
        links.observe(&link.url, &link.text, domain);
    }
    if links.careers {
        add(CAREERS_LINK, "careers_link");
    }
    if links.about {
        add(ABOUT_LINK, "about_link");
    }
    if links.contact {
        add(CONTACT_LINK, "contact_link");
    }
    if links.privacy {
        add(PRIVACY_LINK, "privacy_link");
    }
    if links.terms {
        add(TERMS_LINK, "terms_link");
    }

    if page_url.scheme() == "https" {
        add(HTTPS, "https");
    }
    if inbound_domains >= 5 {
        add(MANY_INBOUND, "many_inbound");
    } else if inbound_domains >= 2 {
        add(SOME_INBOUND, "some_inbound");
    }
    let thin = page.text.len() < THIN_TEXT_BYTES && page.links.len() < THIN_LINKS;
    if thin {
        signals.push("thin");
    }

    let own_org = orgs.iter().find(|o| {
        o.url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .and_then(|u| urls::registrable_domain(&u))
            .as_deref()
            == Some(domain)
    });
    let name = own_org
        .and_then(|o| o.name.clone())
        .or_else(|| page.og_site_name.clone())
        .or_else(|| orgs.iter().find_map(|o| o.name.clone()));

    return Assessment {
        score: score.clamp(0.0, 1.0),
        signals,
        parked: false,
        conclusive: !thin,
        name,
    };
}

#[derive(Debug, Default)]
struct LinkSignals {
    careers: bool,
    about: bool,
    contact: bool,
    privacy: bool,
    terms: bool,
}

impl LinkSignals {
    fn observe(&mut self, url: &Url, text: &str, domain: &str) {
        let internal = urls::registrable_domain(url).as_deref() == Some(domain);
        let text = text.to_lowercase();
        let segments: Vec<String> = url
            .path_segments()
            .map(|s| {
                s.filter(|seg| !seg.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        let has_segment = |set: &[&str]| segments.iter().any(|s| set.contains(&s.as_str()));

        // A careers link must stay on the site or go to its ATS board. "Jobs" on a link
        // to some other site says nothing about this one.
        let (careers_url, careers_text) = scoring::careers_match(url, &text);
        if (careers_url || careers_text) && (internal || ats::is_board(url)) {
            self.careers = true;
        }
        if internal && (text.contains("about") || has_segment(ABOUT_SEGMENTS)) {
            self.about = true;
        }
        if internal && (text.contains("contact") || has_segment(CONTACT_SEGMENTS)) {
            self.contact = true;
        }
        // Privacy and terms pages often live on a parent company's site, so any domain counts.
        if text.contains("privacy") || segments.iter().any(|s| s.contains("privacy")) {
            self.privacy = true;
        }
        if text.contains("terms") || has_segment(TERMS_SEGMENTS) {
            self.terms = true;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Organization {
    name: Option<String>,
    url: Option<String>,
}

/// Collects organization-like nodes from JSON-LD (arrays, `@graph`, nested objects such as
/// `publisher`). An article can mention other organizations, hence the name preference in
/// [`assess`].
fn collect_organizations(value: &Value, out: &mut Vec<Organization>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| collect_organizations(v, out)),
        Value::Object(map) => {
            if map.get("@type").is_some_and(is_organization_type) {
                let text = |key: &str| {
                    map.get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                };
                out.push(Organization {
                    name: text("name"),
                    url: text("url"),
                });
            }
            map.values().for_each(|v| collect_organizations(v, out));
        }
        _ => {}
    }
}

fn is_organization_type(t: &Value) -> bool {
    return match t {
        Value::String(s) => {
            let name = s.rsplit('/').next().unwrap_or(s);
            name.ends_with("Organization")
                || matches!(
                    name,
                    "Corporation" | "LocalBusiness" | "OnlineBusiness" | "OnlineStore" | "NGO"
                )
        }
        Value::Array(types) => types.iter().any(is_organization_type),
        _ => false,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_html;

    fn assess_html(url: &str, html: &str) -> Assessment {
        let url = Url::parse(url).unwrap();
        let domain = urls::registrable_domain(&url).unwrap();
        return assess(&url, &domain, &parse_html(&url, html), 0);
    }

    const COMPANY: &str = r#"<html><head>
        <script type="application/ld+json">
          {"@context":"https://schema.org","@graph":[{"@type":"WebSite"},{"@type":"Organization","name":"Acme Ltd"}]}
        </script></head><body>
        <nav><a href="/about">About</a><a href="/careers">Careers</a><a href="/contact">Contact</a></nav>
        <main>We make payments simple for businesses across Africa.</main>
        <footer>© 2026 Acme Ltd. All rights reserved. <a href="/privacy-policy">Privacy</a>
        <a href="/terms">Terms</a></footer></body></html>"#;

    #[test]
    fn company_site_scores_high() {
        let a = assess_html("https://acme.com/", COMPANY);
        assert!(a.score >= COMPANY_THRESHOLD, "{a:?}");
        assert_eq!(a.name.as_deref(), Some("Acme Ltd"));
        for s in [
            "org_json_ld",
            "legal_suffix",
            "copyright",
            "careers_link",
            "about_link",
            "privacy_link",
            "https",
        ] {
            assert!(a.signals.contains(&s), "missing {s}: {a:?}");
        }
    }

    #[test]
    fn ats_board_counts_as_careers_link_but_other_sites_do_not() {
        let with_board = assess_html(
            "https://acme.com/",
            r#"<body><a href="https://jobs.lever.co/acme">Open roles</a></body>"#,
        );
        assert!(with_board.signals.contains(&"careers_link"));
        let elsewhere = assess_html(
            "https://blog.example.com/",
            r#"<body><a href="https://news.site/jobs-report">Jobs report</a></body>"#,
        );
        assert!(!elsewhere.signals.contains(&"careers_link"));
    }

    #[test]
    fn personal_blog_lands_in_the_gray_zone_or_below() {
        let a = assess_html(
            "https://someblog.net/",
            &format!(
                "<body><a href='/about'>About me</a><a href='/post-1'>Post</a><a href='/post-2'>Post</a>\
                 <a href='/post-3'>Post</a><a href='/post-4'>Post</a><p>{}</p>\
                 <footer>© 2026 Jane Doe</footer></body>",
                "Thoughts on gardening. ".repeat(30)
            ),
        );
        assert!(a.score < COMPANY_THRESHOLD, "{a:?}");
    }

    #[test]
    fn parked_and_thin_pages_score_low() {
        let parked = assess_html(
            "https://acme-shop.com/",
            "<title>acme-shop.com</title><body>This domain is for sale! Buy this domain today.</body>",
        );
        assert!(parked.parked);
        assert_eq!(parked.score, 0.0);

        let thin = assess_html("http://tiny.org/", "<body>Hello</body>");
        assert!(thin.signals.contains(&"thin"));
        assert!(!thin.conclusive);
        assert!(thin.score < NOT_COMPANY_THRESHOLD);
    }

    #[test]
    fn organization_types() {
        let found = |v: serde_json::Value| {
            let mut out = Vec::new();
            collect_organizations(&v, &mut out);
            return out.into_iter().map(|o| o.name).collect::<Vec<_>>();
        };
        assert_eq!(
            found(serde_json::json!({"@type": "Corporation", "name": "X"})),
            [Some("X".into())]
        );
        assert_eq!(
            found(serde_json::json!({"@type": ["Thing", "NewsMediaOrganization"]})),
            [None]
        );
        assert_eq!(
            found(
                serde_json::json!({"@type": "http://schema.org/LocalBusiness", "name": " Shop "})
            ),
            [Some("Shop".into())]
        );
        assert_eq!(
            found(
                serde_json::json!({"@type": "WebPage", "publisher": {"@type": "Organization", "name": "P"}})
            ),
            [Some("P".into())]
        );
        assert!(found(serde_json::json!({"@type": "Person", "name": "Jane"})).is_empty());
    }

    #[test]
    fn prefers_the_sites_own_organization_name() {
        let html = r#"<head><meta property="og:site_name" content="Engadget">
            <script type="application/ld+json">[
              {"@type":"NewsArticle","about":{"@type":"CollegeOrUniversity","name":"University of Kent"}},
              {"@type":"Organization","name":"Engadget Media","url":"https://www.engadget.com/"}
            ]</script></head><body>article</body>"#;
        assert_eq!(
            assess_html("https://www.engadget.com/story", html)
                .name
                .as_deref(),
            Some("Engadget Media")
        );

        let no_own = r#"<head><meta property="og:site_name" content="Engadget">
            <script type="application/ld+json">{"@type":"CollegeOrUniversity","name":"University of Kent"}</script>
            </head><body>article</body>"#;
        assert_eq!(
            assess_html("https://www.engadget.com/story", no_own)
                .name
                .as_deref(),
            Some("Engadget")
        );
    }

    #[test]
    fn js_shells_are_inconclusive_not_negative() {
        let shell = assess_html(
            "https://www.anduril.com/",
            r#"<head><script type="application/ld+json">{"@type":"Organization","name":"Anduril Industries"}</script></head>
               <body><div id="__next"></div></body>"#,
        );
        assert!(!shell.conclusive);
        assert!(shell.signals.contains(&"thin"));
        assert_eq!(shell.name.as_deref(), Some("Anduril Industries"));
        assert!(shell.score >= NOT_COMPANY_THRESHOLD, "{shell:?}");
    }
}
