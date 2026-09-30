//! HTML decoding and extraction of what the crawler needs from a page: title, canonical
//! URL, meta robots directives and outgoing links. Job-specific extraction comes later.

use std::collections::HashSet;
use std::sync::LazyLock;

use career_core::urls;
use encoding_rs::{Encoding, UTF_8};
use scraper::{Html, Selector};
use url::Url;

/// Longest anchor text we keep; enough for scoring, bounded for storage.
const MAX_ANCHOR_CHARS: usize = 200;
/// How far into the document to look for a `<meta charset>`, as the HTML spec suggests.
const CHARSET_SNIFF_BYTES: usize = 1024;

static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static BASE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("base[href]").unwrap());
static CANONICAL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"link[rel~="canonical"][href]"#).unwrap());
static META_ROBOTS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"meta[name="robots" i][content]"#).unwrap());
static ANCHORS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub url: Url,
    pub text: String,
    pub nofollow: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPage {
    pub title: Option<String>,
    pub canonical: Option<Url>,
    /// `<meta name="robots" content="noindex">`: don't store this page's content.
    pub noindex: bool,
    /// `<meta name="robots" content="nofollow">`: don't enqueue any of its links.
    pub nofollow: bool,
    /// Unique normalized http(s) links, in document order.
    pub links: Vec<Link>,
}

/// Decodes an HTML body to a string. Charset precedence: BOM, then the `Content-Type`
/// header, then `<meta charset>` near the top, then UTF-8. Invalid sequences are replaced.
pub fn decode_html(body: &[u8], content_type: Option<&str>) -> String {
    let encoding = content_type
        .and_then(charset_param)
        .or_else(|| sniff_meta_charset(body))
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .unwrap_or(UTF_8);
    // `decode` also honours a BOM over the chosen encoding.
    let (text, _, _) = encoding.decode(body);
    return text.into_owned();
}

fn charset_param(content_type: &str) -> Option<String> {
    return content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        return Some(value.trim().trim_matches(['"', '\'']).to_string());
    });
}

/// Finds `charset=...` in the first bytes of the document. This covers both
/// `<meta charset="x">` and `<meta http-equiv="Content-Type" content="text/html; charset=x">`.
fn sniff_meta_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(CHARSET_SNIFF_BYTES)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let start = head.find("charset=")? + "charset=".len();
    let label: String = head[start..]
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .collect();
    if label.is_empty() {
        return None;
    }
    return Some(label);
}

pub fn parse_html(page_url: &Url, html: &str) -> ParsedPage {
    let doc = Html::parse_document(html);

    let base = doc
        .select(&BASE)
        .next()
        .and_then(|el| page_url.join(el.value().attr("href")?).ok())
        .unwrap_or_else(|| page_url.clone());

    let title = doc
        .select(&TITLE)
        .next()
        .map(|el| collapse_whitespace(&el.text().collect::<String>()))
        .filter(|t| !t.is_empty());

    let canonical = doc
        .select(&CANONICAL)
        .next()
        .and_then(|el| urls::resolve(&base, el.value().attr("href")?));

    let mut noindex = false;
    let mut nofollow = false;
    for el in doc.select(&META_ROBOTS) {
        let content = el
            .value()
            .attr("content")
            .unwrap_or_default()
            .to_ascii_lowercase();
        for directive in content.split(',').map(str::trim) {
            match directive {
                "noindex" => noindex = true,
                "nofollow" => nofollow = true,
                "none" => {
                    noindex = true;
                    nofollow = true;
                }
                _ => {}
            }
        }
    }

    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for el in doc.select(&ANCHORS) {
        let Some(url) = urls::resolve(&base, el.value().attr("href").unwrap_or_default()) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let mut text = collapse_whitespace(&el.text().collect::<String>());
        if text.is_empty() {
            text = ["aria-label", "title"]
                .iter()
                .find_map(|a| el.value().attr(a))
                .map(collapse_whitespace)
                .unwrap_or_default();
        }
        let rel = el
            .value()
            .attr("rel")
            .unwrap_or_default()
            .to_ascii_lowercase();
        links.push(Link {
            url,
            text: text.chars().take(MAX_ANCHOR_CHARS).collect(),
            nofollow: rel.split_whitespace().any(|r| r == "nofollow"),
        });
    }

    return ParsedPage {
        title,
        canonical,
        noindex,
        nofollow,
        links,
    };
}

fn collapse_whitespace(s: &str) -> String {
    return s.split_whitespace().collect::<Vec<_>>().join(" ");
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<!doctype html>
<html><head>
  <title>
    Acme  Inc — About
  </title>
  <link rel="canonical" href="/about?utm_source=x">
  <meta name="ROBOTS" content="index, follow">
</head><body>
  <nav>
    <a href="/careers">Careers</a>
    <a href="/careers#open-roles">Open roles</a>
    <a href="https://jobs.lever.co/acme" rel="noopener nofollow">We're
       hiring!</a>
    <a href="mailto:hi@acme.com">Email</a>
    <a href="#top">Top</a>
    <a href="team"><img src="t.png"></a>
    <a href="/press" aria-label="Press room"></a>
  </nav>
</body></html>"##;

    #[test]
    fn extracts_title_canonical_and_links() {
        let url = Url::parse("https://acme.com/about/").unwrap();
        let page = parse_html(&url, PAGE);
        assert_eq!(page.title.as_deref(), Some("Acme Inc — About"));
        assert_eq!(page.canonical.unwrap().as_str(), "https://acme.com/about");
        assert!(!page.noindex && !page.nofollow);

        let got: Vec<(&str, &str, bool)> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.text.as_str(), l.nofollow))
            .collect();
        assert_eq!(
            got,
            [
                ("https://acme.com/careers", "Careers", false),
                ("https://jobs.lever.co/acme", "We're hiring!", true),
                ("https://acme.com/about/team", "", false),
                ("https://acme.com/press", "Press room", false),
            ]
        );
    }

    #[test]
    fn honours_base_href_and_meta_robots() {
        let html = r#"<html><head><base href="https://cdn.acme.com/site/">
            <meta name="robots" content="none"></head>
            <body><a href="jobs">Jobs</a></body></html>"#;
        let page = parse_html(&Url::parse("https://acme.com/").unwrap(), html);
        assert_eq!(page.links[0].url.as_str(), "https://cdn.acme.com/site/jobs");
        assert!(page.noindex && page.nofollow);
    }

    #[test]
    fn decodes_charsets() {
        // "café" in windows-1252
        let latin = b"<html><body>caf\xe9</body></html>";
        assert!(decode_html(latin, Some("text/html; charset=windows-1252")).contains("café"));
        assert!(decode_html(latin, Some("text/html; charset=\"ISO-8859-1\"")).contains("café"));

        let with_meta =
            b"<html><head><meta charset=\"windows-1252\"></head><body>caf\xe9</body></html>";
        assert!(decode_html(with_meta, Some("text/html")).contains("café"));

        let http_equiv =
            b"<meta http-equiv='Content-Type' content='text/html; charset=windows-1252'>caf\xe9";
        assert!(decode_html(http_equiv, None).contains("café"));

        assert!(decode_html("café".as_bytes(), None).contains("café"));
        // BOM wins over a wrong header.
        let bom = [b"\xef\xbb\xbf".as_slice(), "café".as_bytes()].concat();
        assert!(decode_html(&bom, Some("text/html; charset=windows-1252")).contains("café"));
    }
}
