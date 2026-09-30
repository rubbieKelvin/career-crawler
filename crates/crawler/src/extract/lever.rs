//! Lever postings API: `GET api.lever.co/v0/postings/<company>?mode=json`.

use career_core::jobs::Job;
use serde::Deserialize;

use super::{
    BoardJobs, clean, country_code, employment_type, posting_url, remote_mode, salary_period,
    truncate,
};

pub fn api_url(token: &str) -> String {
    return format!("https://api.lever.co/v0/postings/{token}?mode=json");
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Posting {
    id: Option<String>,
    text: Option<String>,
    hosted_url: Option<String>,
    categories: Option<Categories>,
    /// Epoch milliseconds.
    created_at: Option<i64>,
    workplace_type: Option<String>,
    country: Option<String>,
    description_plain: Option<String>,
    salary_range: Option<SalaryRange>,
}

#[derive(Deserialize)]
struct Categories {
    location: Option<String>,
    team: Option<String>,
    department: Option<String>,
    commitment: Option<String>,
}

#[derive(Deserialize)]
struct SalaryRange {
    min: Option<f64>,
    max: Option<f64>,
    currency: Option<String>,
    interval: Option<String>,
}

pub fn parse(body: &[u8]) -> anyhow::Result<BoardJobs> {
    let postings: Vec<Posting> = serde_json::from_slice(body)?;
    let jobs = postings
        .into_iter()
        .filter_map(|p| {
            let (Some(url), Some(title)) = (
                p.hosted_url.as_deref().and_then(posting_url),
                clean(p.text.as_deref()),
            ) else {
                return None;
            };
            let categories = p.categories;
            let salary = p.salary_range;
            return Some(Job {
                url,
                external_id: p.id,
                title,
                location: categories
                    .as_ref()
                    .and_then(|c| clean(c.location.as_deref())),
                department: categories.as_ref().and_then(|c| {
                    clean(c.team.as_deref()).or_else(|| clean(c.department.as_deref()))
                }),
                employment_type: categories
                    .as_ref()
                    .and_then(|c| c.commitment.as_deref())
                    .and_then(employment_type),
                remote_mode: p.workplace_type.as_deref().and_then(remote_mode),
                country_code: p.country.as_deref().and_then(country_code),
                salary_min: salary.as_ref().and_then(|s| s.min),
                salary_max: salary.as_ref().and_then(|s| s.max),
                salary_currency: salary.as_ref().and_then(|s| clean(s.currency.as_deref())),
                salary_period: salary
                    .as_ref()
                    .and_then(|s| s.interval.as_deref())
                    .and_then(salary_period),
                posted_at: p.created_at,
                description: clean(p.description_plain.as_deref()).map(|d| truncate(&d, 20_000)),
                source: "ats:lever".into(),
                ..Job::default()
            });
        })
        .collect();
    // Lever doesn't name the company.
    return Ok(BoardJobs {
        company: None,
        jobs,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postings() {
        let body = r#"[
            {"id":"6ed76ce8","text":"Administrative Business Partner","hostedUrl":"https://jobs.lever.co/palantir/6ed76ce8",
             "categories":{"commitment":"Full-time","location":"Singapore, Singapore","team":"Administrative"},
             "createdAt":1786469891368,"workplaceType":"hybrid","country":"SG","descriptionPlain":" Build things ",
             "salaryRange":{"min":90000,"max":120000,"currency":"SGD","interval":"per-year-salary"}},
            {"id":"x","text":"No URL"}
        ]"#;
        let board = parse(body.as_bytes()).unwrap();
        assert_eq!(board.jobs.len(), 1);
        let job = &board.jobs[0];
        assert_eq!(job.url, "https://jobs.lever.co/palantir/6ed76ce8");
        assert_eq!(job.employment_type.as_deref(), Some("full_time"));
        assert_eq!(job.remote_mode.as_deref(), Some("hybrid"));
        assert_eq!(job.country_code.as_deref(), Some("SG"));
        assert_eq!(
            (job.salary_min, job.salary_max),
            (Some(90000.0), Some(120000.0))
        );
        assert_eq!(
            (job.salary_currency.as_deref(), job.salary_period.as_deref()),
            (Some("SGD"), Some("year"))
        );
        assert_eq!(job.description.as_deref(), Some("Build things"));
        assert_eq!(job.posted_at, Some(1786469891368));
    }
}
