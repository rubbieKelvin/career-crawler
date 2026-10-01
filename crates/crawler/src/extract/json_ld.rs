//! schema.org `JobPosting` in JSON-LD. Google for Jobs requires it, so many careers sites
//! and ATS posting pages carry it.

use areer_core::jobs::Job;
use serde_json::Value;
use url::Url;

use super::{
    clean, country_code, employment_type, html_to_text, parse_datetime_ms, posting_url,
    remote_mode, salary_period,
};
use crate::parse::ParsedPage;

/// Every `JobPosting` on the page. A posting without its own `url` takes the page URL, but
/// only when it's the page's sole posting, so two postings can't claim one URL.
pub fn job_postings(page: &ParsedPage, page_url: &Url) -> Vec<Job> {
    let mut nodes = Vec::new();
    for value in &page.json_ld {
        collect_postings(value, &mut nodes);
    }
    let sole = nodes.len() == 1;
    return nodes
        .into_iter()
        .filter_map(|node| {
            let url = match text(node.get("url")) {
                Some(u) => page_url
                    .join(&u)
                    .ok()
                    .and_then(|u| posting_url(u.as_str()))?,
                None if sole => page_url.to_string(),
                None => return None,
            };
            return posting(node, url);
        })
        .collect();
}

fn collect_postings<'a>(value: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| collect_postings(v, out)),
        Value::Object(map) => {
            if map.get("@type").is_some_and(is_job_posting) {
                out.push(map);
                return;
            }
            map.values().for_each(|v| collect_postings(v, out));
        }
        _ => {}
    }
}

fn is_job_posting(t: &Value) -> bool {
    return match t {
        Value::String(s) => s.rsplit('/').next() == Some("JobPosting"),
        Value::Array(types) => types.iter().any(is_job_posting),
        _ => false,
    };
}

fn posting(node: &serde_json::Map<String, Value>, url: String) -> Option<Job> {
    let title = text(node.get("title")).or_else(|| text(node.get("name")))?;
    let places = as_list(node.get("jobLocation"));
    let location = places
        .iter()
        .filter_map(|p| place_text(p))
        .collect::<Vec<_>>()
        .join("; ");
    let country = places
        .iter()
        .find_map(|p| text(p.get("address").and_then(|a| a.get("addressCountry"))))
        .as_deref()
        .and_then(country_code);
    let remote = as_list(node.get("jobLocationType"))
        .iter()
        .find_map(|t| text(Some(t)).as_deref().and_then(remote_mode));
    let salary = node.get("baseSalary");
    let amount = salary.and_then(|s| s.get("value"));
    let (min, max) = match amount {
        Some(Value::Object(q)) => {
            let single = number(q.get("value"));
            (
                number(q.get("minValue")).or(single),
                number(q.get("maxValue")).or(single),
            )
        }
        other => (number(other), number(other)),
    };

    return Some(Job {
        url,
        external_id: text(node.get("identifier").map(|i| i.get("value").unwrap_or(i))),
        title,
        company: text(node.get("hiringOrganization")),
        location: clean(Some(&location)),
        country_code: country,
        remote_mode: remote,
        employment_type: as_list(node.get("employmentType"))
            .iter()
            .find_map(|t| text(Some(t)).as_deref().and_then(employment_type)),
        salary_min: min,
        salary_max: max,
        salary_currency: text(salary.and_then(|s| s.get("currency"))),
        salary_period: text(amount.and_then(|a| a.get("unitText")))
            .as_deref()
            .and_then(salary_period),
        posted_at: text(node.get("datePosted"))
            .as_deref()
            .and_then(parse_datetime_ms),
        description: text(node.get("description"))
            .map(|d| html_to_text(&d))
            .filter(|d| !d.is_empty()),
        source: "jsonld".into(),
        ..Job::default()
    });
}

/// A string, a number, or an object's `name` / `@value`, trimmed.
fn text(value: Option<&Value>) -> Option<String> {
    return match value? {
        Value::String(s) => clean(Some(s)),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(map) => text(map.get("name").or_else(|| map.get("@value"))),
        Value::Array(items) => items.iter().find_map(|v| text(Some(v))),
        _ => None,
    };
}

fn number(value: Option<&Value>) -> Option<f64> {
    return match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.replace(',', "").trim().parse().ok(),
        _ => None,
    };
}

fn as_list(value: Option<&Value>) -> Vec<&Value> {
    return match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(v) => vec![v],
        None => Vec::new(),
    };
}

/// "Lagos, Lagos State, NG" from a Place (or a bare address string).
fn place_text(place: &Value) -> Option<String> {
    let address = place.get("address").unwrap_or(place);
    if let Value::String(s) = address {
        return clean(Some(s));
    }
    let parts: Vec<String> = ["addressLocality", "addressRegion", "addressCountry"]
        .iter()
        .filter_map(|k| text(address.get(*k)))
        .collect();
    return (!parts.is_empty()).then(|| parts.join(", "));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_html;

    fn postings(url: &str, json: &str) -> Vec<Job> {
        let url = Url::parse(url).unwrap();
        let html = format!(r#"<script type="application/ld+json">{json}</script>"#);
        return job_postings(&parse_html(&url, &html), &url);
    }

    #[test]
    fn parses_a_full_posting() {
        let jobs = postings(
            "https://paystack.com/careers/backend-engineer",
            r#"{"@context":"https://schema.org","@type":"JobPosting","title":"Backend Engineer",
                "datePosted":"2026-09-01","employmentType":["FULL_TIME"],
                "hiringOrganization":{"@type":"Organization","name":"Paystack"},
                "jobLocation":{"@type":"Place","address":{"@type":"PostalAddress","addressLocality":"Lagos",
                   "addressRegion":"Lagos State","addressCountry":{"@type":"Country","name":"NG"}}},
                "baseSalary":{"@type":"MonetaryAmount","currency":"NGN",
                   "value":{"@type":"QuantitativeValue","minValue":"12,000,000","maxValue":18000000,"unitText":"YEAR"}},
                "identifier":{"@type":"PropertyValue","value":"BE-42"},
                "description":"<p>Build <b>payments</b></p>"}"#,
        );
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(
            job.url, "https://paystack.com/careers/backend-engineer",
            "no url field: the page's"
        );
        assert_eq!(job.company.as_deref(), Some("Paystack"));
        assert_eq!(job.location.as_deref(), Some("Lagos, Lagos State, NG"));
        assert_eq!(job.country_code.as_deref(), Some("NG"));
        assert_eq!(job.employment_type.as_deref(), Some("full_time"));
        assert_eq!(
            (job.salary_min, job.salary_max),
            (Some(12_000_000.0), Some(18_000_000.0))
        );
        assert_eq!(
            (job.salary_currency.as_deref(), job.salary_period.as_deref()),
            (Some("NGN"), Some("year"))
        );
        assert_eq!(job.external_id.as_deref(), Some("BE-42"));
        assert_eq!(job.description.as_deref(), Some("Build payments"));
        assert_eq!(job.source, "jsonld");
    }

    #[test]
    fn remote_graph_and_multiple_postings() {
        let jobs = postings(
            "https://acme.com/careers",
            r#"{"@graph":[{"@type":"WebPage"},
                {"@type":"JobPosting","title":"Remote SRE","url":"/careers/sre?utm_source=x","jobLocationType":"TELECOMMUTE"},
                {"@type":"JobPosting","title":"No URL here"},
                {"@type":["JobPosting"],"name":"Designer","url":"https://acme.com/careers/design",
                 "baseSalary":{"currency":"USD","value":50}}]}"#,
        );
        let got: Vec<(&str, &str)> = jobs
            .iter()
            .map(|j| (j.title.as_str(), j.url.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("Remote SRE", "https://acme.com/careers/sre"),
                ("Designer", "https://acme.com/careers/design")
            ],
            "a posting without a URL is dropped when the page has several"
        );
        assert_eq!(jobs[0].remote_mode.as_deref(), Some("remote"));
        assert_eq!(
            (jobs[1].salary_min, jobs[1].salary_max),
            (Some(50.0), Some(50.0))
        );
    }

    #[test]
    fn ignores_other_types() {
        assert!(postings("https://a.com/", r#"{"@type":"Organization","name":"A"}"#).is_empty());
    }
}
