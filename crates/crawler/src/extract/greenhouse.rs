//! Greenhouse job board API:
//! `GET boards-api.greenhouse.io/v1/boards/<token>/jobs?content=true&pay_transparency=true`.
//! `pay_transparency` adds `pay_input_ranges`, salary ranges in cents.

use areer_core::jobs::Job;
use serde::Deserialize;

use super::{BoardJobs, clean, html_to_text, parse_datetime_ms, posting_url};

pub fn api_url(token: &str) -> String {
    return format!(
        "https://boards-api.greenhouse.io/v1/boards/{token}/jobs?content=true&pay_transparency=true"
    );
}

#[derive(Deserialize)]
struct Response {
    jobs: Vec<Posting>,
}

#[derive(Deserialize)]
struct Posting {
    id: Option<u64>,
    title: Option<String>,
    absolute_url: Option<String>,
    location: Option<Named>,
    #[serde(default)]
    departments: Vec<Named>,
    first_published: Option<String>,
    updated_at: Option<String>,
    /// Entity-escaped HTML.
    content: Option<String>,
    company_name: Option<String>,
    #[serde(default)]
    pay_input_ranges: Vec<PayRange>,
}

#[derive(Deserialize)]
struct PayRange {
    min_cents: Option<f64>,
    max_cents: Option<f64>,
    currency_type: Option<String>,
    /// E.g. "US Salary Range", "Hourly Pay Range". Greenhouse gives no interval field.
    title: Option<String>,
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

pub fn parse(body: &[u8]) -> anyhow::Result<BoardJobs> {
    let response: Response = serde_json::from_slice(body)?;
    let company = response
        .jobs
        .iter()
        .find_map(|p| clean(p.company_name.as_deref()));
    let jobs = response
        .jobs
        .into_iter()
        .filter_map(|p| {
            let (Some(url), Some(title)) = (
                p.absolute_url.as_deref().and_then(posting_url),
                clean(p.title.as_deref()),
            ) else {
                return None;
            };
            let location = p.location.and_then(|l| clean(l.name.as_deref()));
            let remote = location
                .as_deref()
                .is_some_and(|l| l.to_ascii_lowercase().contains("remote"))
                .then(|| "remote".to_string());
            let pay = p.pay_input_ranges.first();
            let period = pay.map(|r| {
                let hourly = r
                    .title
                    .as_deref()
                    .is_some_and(|t| t.to_ascii_lowercase().contains("hour"));
                return if hourly { "hour" } else { "year" }.to_string();
            });
            return Some(Job {
                url,
                external_id: p.id.map(|id| id.to_string()),
                title,
                company: clean(p.company_name.as_deref()),
                location,
                remote_mode: remote,
                department: p
                    .departments
                    .into_iter()
                    .find_map(|d| clean(d.name.as_deref())),
                posted_at: p
                    .first_published
                    .or(p.updated_at)
                    .as_deref()
                    .and_then(parse_datetime_ms),
                description: p
                    .content
                    .as_deref()
                    .map(html_to_text)
                    .filter(|d| !d.is_empty()),
                salary_min: pay.and_then(|r| r.min_cents).map(|c| c / 100.0),
                salary_max: pay.and_then(|r| r.max_cents).map(|c| c / 100.0),
                salary_currency: pay.and_then(|r| clean(r.currency_type.as_deref())),
                salary_period: period,
                source: "ats:greenhouse".into(),
                ..Job::default()
            });
        })
        .collect();
    return Ok(BoardJobs { company, jobs });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_board() {
        let body = r#"{"jobs":[
            {"id":7982023003,"title":"Data Engineer ","absolute_url":"https://a16z.com/about/jobs/?gh_jid=7982023003",
             "location":{"name":"Remote - US"},"departments":[{"id":1,"name":"Operations"}],
             "first_published":"2026-09-16T18:04:50-04:00","updated_at":"2026-09-21T19:46:54-04:00",
             "content":"&lt;p&gt;Build pipelines&lt;/p&gt;","company_name":"a16z",
             "pay_input_ranges":[{"min_cents":9000000,"max_cents":11000000,"currency_type":"USD","title":"US Salary Range"}]},
            {"id":2,"title":"","absolute_url":"https://x/2"},
            {"id":3,"title":"No URL"}
        ],"meta":{"total":3}}"#;
        let board = parse(body.as_bytes()).unwrap();
        assert_eq!(board.company.as_deref(), Some("a16z"));
        assert_eq!(
            board.jobs.len(),
            1,
            "postings without a title or URL are dropped"
        );
        let job = &board.jobs[0];
        assert_eq!(job.title, "Data Engineer");
        assert_eq!(job.external_id.as_deref(), Some("7982023003"));
        assert_eq!(job.remote_mode.as_deref(), Some("remote"));
        assert_eq!(job.department.as_deref(), Some("Operations"));
        assert_eq!(job.description.as_deref(), Some("Build pipelines"));
        assert_eq!(job.posted_at, Some(1789596290000));
        assert_eq!(job.source, "ats:greenhouse");
        assert_eq!(
            (job.salary_min, job.salary_max),
            (Some(90000.0), Some(110000.0))
        );
        assert_eq!(
            (job.salary_currency.as_deref(), job.salary_period.as_deref()),
            (Some("USD"), Some("year"))
        );
    }
}
