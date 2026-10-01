//! Ashby job board API: `GET api.ashbyhq.com/posting-api/job-board/<org>?includeCompensation=true`.

use areer_core::jobs::Job;
use serde::Deserialize;

use super::{
    BoardJobs, clean, country_code, employment_type, parse_datetime_ms, posting_url, remote_mode,
    salary_period, truncate,
};

pub fn api_url(token: &str) -> String {
    return format!(
        "https://api.ashbyhq.com/posting-api/job-board/{token}?includeCompensation=true"
    );
}

#[derive(Deserialize)]
struct Response {
    jobs: Vec<Posting>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Posting {
    id: Option<String>,
    title: Option<String>,
    job_url: Option<String>,
    location: Option<String>,
    department: Option<String>,
    team: Option<String>,
    employment_type: Option<String>,
    workplace_type: Option<String>,
    is_remote: Option<bool>,
    is_listed: Option<bool>,
    published_at: Option<String>,
    description_plain: Option<String>,
    address: Option<Address>,
    compensation: Option<Compensation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Address {
    postal_address: Option<PostalAddress>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostalAddress {
    address_country: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Compensation {
    #[serde(default)]
    summary_components: Vec<Component>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Component {
    compensation_type: Option<String>,
    interval: Option<String>,
    currency_code: Option<String>,
    min_value: Option<f64>,
    max_value: Option<f64>,
}

pub fn parse(body: &[u8]) -> anyhow::Result<BoardJobs> {
    let response: Response = serde_json::from_slice(body)?;
    let jobs = response
        .jobs
        .into_iter()
        .filter(|p| p.is_listed != Some(false))
        .filter_map(|p| {
            let (Some(url), Some(title)) = (
                p.job_url.as_deref().and_then(posting_url),
                clean(p.title.as_deref()),
            ) else {
                return None;
            };
            let salary = p.compensation.and_then(|c| {
                c.summary_components
                    .into_iter()
                    .find(|c| c.compensation_type.as_deref() == Some("Salary"))
            });
            let remote = p
                .workplace_type
                .as_deref()
                .and_then(remote_mode)
                .or_else(|| (p.is_remote == Some(true)).then(|| "remote".to_string()));
            return Some(Job {
                url,
                external_id: p.id,
                title,
                location: clean(p.location.as_deref()),
                department: clean(p.department.as_deref()).or_else(|| clean(p.team.as_deref())),
                employment_type: p.employment_type.as_deref().and_then(employment_type),
                remote_mode: remote,
                country_code: p
                    .address
                    .and_then(|a| a.postal_address)
                    .and_then(|a| a.address_country)
                    .as_deref()
                    .and_then(country_code),
                salary_min: salary.as_ref().and_then(|s| s.min_value),
                salary_max: salary.as_ref().and_then(|s| s.max_value),
                salary_currency: salary
                    .as_ref()
                    .and_then(|s| clean(s.currency_code.as_deref())),
                salary_period: salary
                    .as_ref()
                    .and_then(|s| s.interval.as_deref())
                    .and_then(salary_period),
                posted_at: p.published_at.as_deref().and_then(parse_datetime_ms),
                description: clean(p.description_plain.as_deref()).map(|d| truncate(&d, 20_000)),
                source: "ats:ashby".into(),
                ..Job::default()
            });
        })
        .collect();
    // Ashby's job board API doesn't name the company.
    return Ok(BoardJobs {
        company: None,
        jobs,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_board_with_compensation() {
        let body = r#"{"apiVersion":"1","jobs":[
            {"id":"f265","title":"Staff Software Engineer","department":"All Cost Center","team":"ENG",
             "employmentType":"FullTime","location":"Toronto, Canada","isListed":true,"isRemote":true,
             "workplaceType":"Remote","publishedAt":"2026-04-15T13:26:54.184+00:00",
             "address":{"postalAddress":{"addressCountry":"Canada","addressLocality":"Toronto"}},
             "jobUrl":"https://jobs.ashbyhq.com/kong/f265","descriptionPlain":"Unlock intelligence",
             "compensation":{"summaryComponents":[
               {"compensationType":"EquityPercentage","interval":"NONE","minValue":0.1},
               {"compensationType":"Salary","interval":"1 YEAR","currencyCode":"CAD","minValue":163685,"maxValue":245800}]}},
            {"id":"hidden","title":"Unlisted","jobUrl":"https://jobs.ashbyhq.com/kong/h","isListed":false}
        ]}"#;
        let board = parse(body.as_bytes()).unwrap();
        assert_eq!(board.jobs.len(), 1, "unlisted postings are skipped");
        let job = &board.jobs[0];
        assert_eq!(job.employment_type.as_deref(), Some("full_time"));
        assert_eq!(job.remote_mode.as_deref(), Some("remote"));
        assert_eq!(job.country_code, None, "country names wait for enrichment");
        assert_eq!(
            (job.salary_min, job.salary_max),
            (Some(163685.0), Some(245800.0))
        );
        assert_eq!(
            (job.salary_currency.as_deref(), job.salary_period.as_deref()),
            (Some("CAD"), Some("year"))
        );
        assert_eq!(job.department.as_deref(), Some("All Cost Center"));
        assert_eq!(job.posted_at, Some(1776259614184));
    }
}
