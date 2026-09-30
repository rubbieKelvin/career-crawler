//! URL normalization and host/domain helpers. The frontier and `pages` table dedup
//! on the normalized form, so every URL goes through [`normalize`] before storage.

use url::{Host, Url};

/// Query parameters that only track clicks or sessions and never change page content.
const DROPPED_PARAMS: &[&str] = &[
    "gclid",
    "fbclid",
    "msclkid",
    "dclid",
    "yclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "ref",
    "ref_src",
    "jsessionid",
    "phpsessid",
    "sessionid",
];

fn is_dropped_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    return name.starts_with("utm_") || DROPPED_PARAMS.contains(&name.as_str());
}

/// Canonical form used for dedup. The `url` crate already lowercases the scheme and host,
/// strips default ports and gives an empty path as `/`. On top of that we drop the fragment,
/// credentials, `;jsessionid=` path params and tracking/session query params, and sort
/// the remaining query pairs.
///
/// Trailing slashes are left alone: `/careers` and `/careers/` are sometimes different
/// pages. Exact duplicates are caught later by content hash.
pub fn normalize(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);

    if let Some(idx) = url.path().to_ascii_lowercase().find(";jsessionid=") {
        let path = url.path()[..idx].to_string();
        url.set_path(&path);
    }

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_dropped_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        pairs.sort();
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    return url;
}

/// Resolves an `href` found on a page against `base`, keeping only http(s) targets.
/// Returns the normalized absolute URL.
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let url = base.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    return Some(normalize(&url));
}

/// The registrable domain ("eTLD+1") used to group hosts into one company:
/// `careers.acme.co.uk` → `acme.co.uk`. IP hosts are returned as-is.
pub fn registrable_domain(url: &Url) -> Option<String> {
    return match url.host()? {
        Host::Domain(d) => psl::domain_str(d.trim_end_matches('.')).map(str::to_string),
        Host::Ipv4(ip) => Some(ip.to_string()),
        Host::Ipv6(ip) => Some(ip.to_string()),
    };
}

/// Key for per-host politeness: host plus port (only when not the scheme default).
pub fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    return match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        return normalize(&Url::parse(s).unwrap()).to_string();
    }

    #[test]
    fn normalize_strips_noise() {
        assert_eq!(norm("HTTPS://Example.COM:443"), "https://example.com/");
        assert_eq!(norm("https://a.com/x#section"), "https://a.com/x");
        assert_eq!(norm("https://user:pw@a.com/"), "https://a.com/");
        assert_eq!(
            norm("https://a.com/jobs?utm_source=x&b=2&gclid=1&a=1&UTM_Medium=y"),
            "https://a.com/jobs?a=1&b=2"
        );
        assert_eq!(norm("https://a.com/?utm_campaign=z"), "https://a.com/");
        assert_eq!(norm("https://a.com/p;jsessionid=ABC123"), "https://a.com/p");
    }

    #[test]
    fn normalize_keeps_meaningful_parts() {
        assert_eq!(norm("https://a.com/careers/"), "https://a.com/careers/");
        assert_eq!(norm("https://a.com/careers"), "https://a.com/careers");
        assert_eq!(
            norm("http://a.com:8080/x?page=2"),
            "http://a.com:8080/x?page=2"
        );
    }

    #[test]
    fn resolve_filters_schemes_and_fragments() {
        let base = Url::parse("https://acme.com/about/team").unwrap();
        let r = |h: &str| resolve(&base, h).map(|u| u.to_string());
        assert_eq!(
            r("../careers?utm_source=nav"),
            Some("https://acme.com/careers".into())
        );
        assert_eq!(
            r("//jobs.lever.co/acme"),
            Some("https://jobs.lever.co/acme".into())
        );
        assert_eq!(r("#top"), None);
        assert_eq!(r("mailto:hr@acme.com"), None);
        assert_eq!(r("javascript:void(0)"), None);
        assert_eq!(r("tel:+2341234"), None);
        assert_eq!(r("  "), None);
    }

    #[test]
    fn registrable_domain_groups_subdomains() {
        let d = |s: &str| registrable_domain(&Url::parse(s).unwrap());
        assert_eq!(d("https://careers.acme.co.uk/x"), Some("acme.co.uk".into()));
        assert_eq!(d("https://www.paystack.com"), Some("paystack.com".into()));
        assert_eq!(d("https://acme.com.ng"), Some("acme.com.ng".into()));
        assert_eq!(d("http://127.0.0.1:8080/"), Some("127.0.0.1".into()));
    }

    #[test]
    fn host_key_includes_non_default_port() {
        let k = |s: &str| host_key(&Url::parse(s).unwrap());
        assert_eq!(k("https://a.com/x"), "a.com");
        assert_eq!(k("http://a.com:8080/"), "a.com:8080");
    }
}
