//! How well a job fits a profile (see `brainstorms/12-cv-profile.md`). A deterministic score
//! in [0, 1] with short reasons, computed for every job: relevance is a ranking, never a
//! filter. Unknowns (a job with no salary, a profile with no location) score neutral, so
//! missing data neither rescues nor sinks a job.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row, SqliteConnection, SqlitePool};

use crate::enrich;
use crate::profile::{Profile, ProfilePlace};
use crate::time::now_ms;

const W_SKILLS: f64 = 0.35;
const W_TITLE: f64 = 0.25;
const W_SENIORITY: f64 = 0.10;
const W_LOCATION: f64 = 0.20;
const W_SALARY: f64 = 0.10;
const NEUTRAL: f64 = 0.5;
/// A job missing a `must_have` term can't rank above this.
const MISSING_MUST_HAVE_CAP: f64 = 0.35;
/// Description characters searched for `must_have` / `exclude` terms.
const DESCRIPTION_CHARS: usize = 3_000;
/// Scoring batch size for a full recompute.
const BATCH: i64 = 500;
/// A job this good counts as a "hit" for the domain that yielded it (crawl steering).
pub const HIT_SCORE: f64 = 0.7;

/// The fields of a stored job that matching reads.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchJob {
    pub id: i64,
    pub title: String,
    pub company: Option<String>,
    /// The company's domain, when known.
    pub domain: Option<String>,
    pub category: Option<String>,
    pub seniority: Option<String>,
    pub skills: Vec<String>,
    pub description: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub country_code: Option<String>,
    pub remote_mode: Option<String>,
    pub remote_regions: Vec<String>,
    pub salary_usd_annual: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    pub score: f64,
    pub reasons: Vec<String>,
}

fn words(s: &str) -> Vec<String> {
    return s
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
}

/// Letters and digits only, lowercase: for comparing company names.
fn simplify(s: &str) -> String {
    return s
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
}

const LEVEL_WORDS: &[&str] = &[
    "senior",
    "sr",
    "junior",
    "jr",
    "lead",
    "staff",
    "principal",
    "head",
    "of",
    "the",
    "and",
    "intern",
];

fn seniority_rank(level: &str) -> Option<i32> {
    return match level {
        "intern" => Some(0),
        "junior" => Some(1),
        "mid" => Some(2),
        "senior" => Some(3),
        "lead" | "manager" => Some(4),
        "director" => Some(5),
        "executive" => Some(6),
        _ => None,
    };
}

/// Categories close enough to earn partial credit.
fn neighbours(a: &str, b: &str) -> bool {
    const PAIRS: &[(&str, &str)] = &[
        ("engineering", "data"),
        ("engineering", "security"),
        ("engineering", "it"),
        ("engineering", "product"),
        ("data", "product"),
        ("data", "finance"),
        ("product", "design"),
        ("sales", "marketing"),
        ("sales", "customer_support"),
        ("marketing", "design"),
        ("operations", "finance"),
        ("operations", "customer_support"),
        ("it", "security"),
    ];
    return PAIRS
        .iter()
        .any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a));
}

struct Part {
    value: f64,
    reason: Option<String>,
}

fn part(value: f64, reason: impl Into<String>) -> Part {
    return Part {
        value,
        reason: Some(reason.into()),
    };
}

fn neutral() -> Part {
    return Part {
        value: NEUTRAL,
        reason: None,
    };
}

fn skills_part(profile: &Profile, job: &MatchJob) -> Part {
    if profile.skills.is_empty() || job.skills.is_empty() {
        return neutral();
    }
    let mut hit = Vec::new();
    let mut total = 0.0;
    for skill in &job.skills {
        if let Some(s) = profile
            .skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(skill))
        {
            // A skill the CV mentions at all counts for most; frequency adds the rest.
            total += (0.6 + 0.4 * s.weight.clamp(0.0, 1.0)).min(1.0);
            hit.push(skill.as_str());
        }
    }
    let value = total / job.skills.len() as f64;
    let reason = if hit.is_empty() {
        format!("skills: none of {} match", job.skills.len())
    } else {
        format!(
            "skills: {} ({}/{})",
            hit.join(", "),
            hit.len(),
            job.skills.len()
        )
    };
    return part(value, reason);
}

fn title_part(profile: &Profile, job: &MatchJob) -> Part {
    if profile.titles.is_empty() {
        return neutral();
    }
    let job_words: Vec<String> = words(&job.title)
        .into_iter()
        .filter(|w| !LEVEL_WORDS.contains(&w.as_str()))
        .collect();
    let mut best = (0.0, "");
    for title in &profile.titles {
        let own: Vec<String> = words(title)
            .into_iter()
            .filter(|w| !LEVEL_WORDS.contains(&w.as_str()))
            .collect();
        if own.is_empty() {
            continue;
        }
        let shared = own.iter().filter(|w| job_words.contains(w)).count();
        let overlap = shared as f64 / own.len() as f64;
        if overlap > best.0 {
            best = (overlap, title.as_str());
        }
    }
    if best.0 >= 0.6 {
        return part(1.0, format!("title: like {}", best.1));
    }
    if best.0 >= 0.34 {
        return part(0.8, format!("title: close to {}", best.1));
    }
    let categories: Vec<&str> = profile
        .titles
        .iter()
        .filter_map(|t| enrich::category(t, None))
        .collect();
    return match job.category.as_deref() {
        Some(c) if categories.contains(&c) => part(0.7, format!("category: {c}")),
        Some(c) if categories.iter().any(|k| neighbours(k, c)) => {
            part(0.4, format!("category: near {c}"))
        }
        Some(c) => part(0.1, format!("category: {c} is outside your titles")),
        None => neutral(),
    };
}

fn seniority_part(profile: &Profile, job: &MatchJob) -> Part {
    let (Some(mine), Some(theirs)) = (profile.effective_seniority(), job.seniority.as_deref())
    else {
        return neutral();
    };
    let (Some(a), Some(b)) = (seniority_rank(mine), seniority_rank(theirs)) else {
        return neutral();
    };
    let value = match (a - b).abs() {
        0 => 1.0,
        1 => 0.8,
        2 => 0.35,
        _ => 0.1,
    };
    return part(value, format!("seniority: {theirs} vs your {mine}"));
}

fn place_country(places: &[ProfilePlace]) -> Vec<&str> {
    return places
        .iter()
        .filter_map(|p| p.country_code.as_deref())
        .collect();
}

fn location_part(profile: &Profile, job: &MatchJob) -> Part {
    let countries = place_country(&profile.locations);
    let job_remote = job.remote_mode.as_deref() == Some("remote");
    let prefers_remote = profile.remote.as_deref() == Some("remote");

    if job_remote {
        if job.remote_regions.is_empty() {
            return part(0.7, "remote");
        }
        let open = job
            .remote_regions
            .iter()
            .any(|r| r == "global" || countries.contains(&r.as_str()));
        return if open {
            part(1.0, "remote, open to you")
        } else if profile.relocate {
            part(0.3, "remote, but not open to your country")
        } else {
            part(0.15, "remote, but not open to your country")
        };
    }
    if profile.locations.is_empty() {
        return neutral();
    }

    let mut value = None;
    let mut reason = String::new();
    if let (Some(lat), Some(lon)) = (job.lat, job.lon) {
        let nearest = profile
            .locations
            .iter()
            .filter_map(|p| Some((p, enrich::geo::distance_km((p.lat?, p.lon?), (lat, lon)))))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((place, km)) = nearest {
            value = Some(match km {
                km if km <= 50.0 => 1.0,
                km if km <= 100.0 => 0.8,
                km if km <= 250.0 => 0.5,
                _ if job
                    .country_code
                    .as_deref()
                    .is_some_and(|c| countries.contains(&c)) =>
                {
                    0.4
                }
                _ if profile.relocate => 0.3,
                _ => 0.05,
            });
            reason = format!("location: {km:.0} km from {}", place.name);
        }
    }
    let value = match value {
        Some(v) => v,
        None => match job.country_code.as_deref() {
            Some(c) if countries.contains(&c) => {
                reason = format!("location: in {c}");
                0.5
            }
            Some(c) => {
                reason = format!("location: {c}");
                if profile.relocate { 0.3 } else { 0.05 }
            }
            None => return neutral(),
        },
    };
    // Someone who wants remote work isn't served by an office, near or not.
    let value = if prefers_remote { value * 0.5 } else { value };
    return part(value, reason);
}

fn salary_part(profile: &Profile, job: &MatchJob) -> Part {
    let (Some(expect), Some(pay)) = (profile.salary_expectation_usd, job.salary_usd_annual) else {
        return neutral();
    };
    if expect <= 0.0 {
        return neutral();
    }
    let ratio = pay / expect;
    return if ratio >= 1.0 {
        part(1.0, "salary: at or above your expectation")
    } else if ratio >= 0.8 {
        part(0.7, "salary: a little under your expectation")
    } else {
        part(0.2, "salary: well under your expectation")
    };
}

pub fn score(profile: &Profile, job: &MatchJob) -> Match {
    let haystack = format!(
        "{} {} {}",
        job.title,
        job.company.as_deref().unwrap_or(""),
        job.description
            .as_deref()
            .map(|d| d.chars().take(DESCRIPTION_CHARS).collect::<String>())
            .unwrap_or_default()
    )
    .to_lowercase();

    let excluded_company = profile.excluded_companies.iter().find(|c| {
        let c = simplify(c);
        return c.len() >= 3
            && (job
                .company
                .as_deref()
                .map(simplify)
                .is_some_and(|n| n.contains(&c))
                || job
                    .domain
                    .as_deref()
                    .and_then(|d| d.split('.').next())
                    .is_some_and(|label| simplify(label) == c));
    });
    if let Some(c) = excluded_company {
        return Match {
            score: 0.0,
            reasons: vec![format!("excluded company: {c}")],
        };
    }
    if let Some(term) = profile
        .exclude
        .iter()
        .find(|t| !t.trim().is_empty() && haystack.contains(&t.trim().to_lowercase()))
    {
        return Match {
            score: 0.0,
            reasons: vec![format!("excluded: mentions \"{term}\"")],
        };
    }

    let parts = [
        (W_SKILLS, skills_part(profile, job)),
        (W_TITLE, title_part(profile, job)),
        (W_SENIORITY, seniority_part(profile, job)),
        (W_LOCATION, location_part(profile, job)),
        (W_SALARY, salary_part(profile, job)),
    ];
    let mut total: f64 = parts.iter().map(|(w, p)| w * p.value).sum();
    let mut reasons: Vec<String> = parts.into_iter().filter_map(|(_, p)| p.reason).collect();

    let missing: Vec<&String> = profile
        .must_have
        .iter()
        .filter(|t| !t.trim().is_empty() && !haystack.contains(&t.trim().to_lowercase()))
        .collect();
    if !missing.is_empty() {
        total = total.min(MISSING_MUST_HAVE_CAP);
        reasons.push(format!(
            "missing must-have: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    return Match {
        score: total.clamp(0.0, 1.0),
        reasons,
    };
}

fn job_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<MatchJob, sqlx::Error> {
    let list = |col: &str| -> Result<Vec<String>, sqlx::Error> {
        let raw: Option<String> = row.try_get(col)?;
        return Ok(raw
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default());
    };
    return Ok(MatchJob {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        company: row.try_get("company")?,
        domain: row.try_get("host")?,
        category: row.try_get("category")?,
        seniority: row.try_get("seniority")?,
        skills: list("skills")?,
        description: row.try_get("description")?,
        lat: row.try_get("lat")?,
        lon: row.try_get("lon")?,
        country_code: row.try_get("country_code")?,
        remote_mode: row.try_get("remote_mode")?,
        remote_regions: list("remote_regions")?,
        salary_usd_annual: row.try_get("salary_usd_annual")?,
    });
}

const JOB_COLUMNS: &str = "j.id, j.title, j.company, d.host, j.category, j.seniority, j.skills,
        substr(j.description, 1, 3000) AS description, j.lat, j.lon, j.country_code, j.remote_mode,
        j.remote_regions, j.salary_usd_annual";

/// Scores these jobs for a profile and stores the matches. Returns how many.
pub async fn score_jobs(
    conn: &mut SqliteConnection,
    profile_id: i64,
    profile: &Profile,
    job_ids: &[i64],
) -> anyhow::Result<usize> {
    if job_ids.is_empty() {
        return Ok(0);
    }
    let rows = sqlx::query(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs j LEFT JOIN domains d ON d.id = j.domain_id
         WHERE j.id IN (SELECT value FROM json_each(?))"
    ))
    .bind(serde_json::to_string(job_ids)?)
    .fetch_all(&mut *conn)
    .await?;
    let now = now_ms();
    for row in &rows {
        let job = job_from_row(row)?;
        let m = score(profile, &job);
        sqlx::query(
            "INSERT INTO job_matches (profile_id, job_id, score, reasons, computed_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(profile_id, job_id) DO UPDATE SET score = excluded.score,
               reasons = excluded.reasons, computed_at = excluded.computed_at",
        )
        .bind(profile_id)
        .bind(job.id)
        .bind(m.score)
        .bind(serde_json::to_string(&m.reasons)?)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    return Ok(rows.len());
}

/// Recomputes the matches of every open job, in batches (one transaction each, so a
/// crawler sharing the database is never blocked for long).
pub async fn rescore_all(
    pool: &SqlitePool,
    profile_id: i64,
    profile: &Profile,
) -> anyhow::Result<usize> {
    let mut after = 0;
    let mut total = 0;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE id > ? AND closed_at IS NULL ORDER BY id LIMIT ?",
        )
        .bind(after)
        .bind(BATCH)
        .fetch_all(pool)
        .await?;
        let Some(last) = ids.last().copied() else {
            return Ok(total);
        };
        let mut tx = pool.begin().await?;
        total += score_jobs(&mut tx, profile_id, profile, &ids).await?;
        tx.commit().await?;
        after = last;
    }
}

/// How many open jobs of a domain fit the profile well: the crawl's "this neighbourhood
/// yields" signal.
pub async fn domain_hits<'e>(
    exec: impl sqlx::SqliteExecutor<'e>,
    profile_id: i64,
    domain: &str,
) -> anyhow::Result<i64> {
    let hits = sqlx::query_scalar(
        "SELECT COUNT(*) FROM job_matches m JOIN jobs j ON j.id = m.job_id JOIN domains d ON d.id = j.domain_id
         WHERE m.profile_id = ? AND d.host = ? AND j.closed_at IS NULL AND m.score >= ?",
    )
    .bind(profile_id)
    .bind(domain)
    .bind(HIT_SCORE)
    .fetch_one(exec)
    .await?;
    return Ok(hits);
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
pub struct TopMatch {
    pub job_id: i64,
    pub score: f64,
    pub reasons: Option<String>,
    pub title: String,
    pub company: Option<String>,
    pub domain: Option<String>,
    pub location: Option<String>,
    pub remote_mode: Option<String>,
    pub url: String,
    pub salary_usd_annual: Option<f64>,
    pub seniority: Option<String>,
    pub category: Option<String>,
    pub employment_type: Option<String>,
    /// The posting's own date when it has one, else when the crawler first saw it.
    pub posted_at: i64,
}

/// What the profile page filters a profile's ranked jobs by. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchFilter {
    /// Substring of the title, company or location.
    pub q: Option<String>,
    /// Lowest score, in [0,1].
    pub min_score: Option<f64>,
    /// `onsite`, `hybrid` or `remote`.
    pub remote: Option<String>,
    pub seniority: Option<String>,
    pub category: Option<String>,
    pub has_salary: Option<bool>,
    /// ISO country code of the job.
    pub country: Option<String>,
    /// Only jobs posted (or first seen) within this many days.
    pub posted_days: Option<i64>,
    /// `score` (default), `recent` or `salary`.
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// The best-matching open jobs.
pub async fn top(pool: &SqlitePool, profile_id: i64, limit: i64) -> anyhow::Result<Vec<TopMatch>> {
    let filter = MatchFilter {
        limit: Some(limit),
        ..MatchFilter::default()
    };
    return Ok(browse(pool, profile_id, &filter).await?.1);
}

const DEFAULT_BROWSE: i64 = 25;
const MAX_BROWSE: i64 = 200;

/// A profile's open jobs, best match first unless sorted otherwise, with the total before
/// paging. The sort comes from a closed list and every value is bound.
pub async fn browse(
    pool: &SqlitePool,
    profile_id: i64,
    f: &MatchFilter,
) -> anyhow::Result<(i64, Vec<TopMatch>)> {
    fn push_where<'a>(
        b: &mut sqlx::QueryBuilder<'a, sqlx::Sqlite>,
        profile_id: i64,
        f: &'a MatchFilter,
    ) {
        b.push(" FROM job_matches m JOIN jobs j ON j.id = m.job_id LEFT JOIN domains d ON d.id = j.domain_id WHERE m.profile_id = ")
            .push_bind(profile_id)
            .push(" AND j.closed_at IS NULL");
        if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            let like = format!(
                "%{}%",
                q.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            b.push(" AND (j.title LIKE ")
                .push_bind(like.clone())
                .push(" ESCAPE '\\' OR COALESCE(j.company, d.name, d.host) LIKE ")
                .push_bind(like.clone())
                .push(" ESCAPE '\\' OR j.location LIKE ")
                .push_bind(like)
                .push(" ESCAPE '\\')");
        }
        if let Some(min) = f.min_score.filter(|m| *m > 0.0) {
            b.push(" AND m.score >= ").push_bind(min);
        }
        for (column, value) in [
            ("remote_mode", &f.remote),
            ("seniority", &f.seniority),
            ("category", &f.category),
            ("country_code", &f.country),
        ] {
            if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                b.push(format!(" AND j.{column} = ")).push_bind(v);
            }
        }
        if f.has_salary == Some(true) {
            b.push(" AND j.salary_usd_annual IS NOT NULL");
        }
        if let Some(days) = f.posted_days.filter(|d| *d > 0) {
            b.push(" AND COALESCE(j.posted_at, j.first_seen) >= ")
                .push_bind(crate::time::now_ms() - days.min(3650) * 86_400_000);
        }
    }

    let mut count = sqlx::QueryBuilder::new("SELECT COUNT(*)");
    push_where(&mut count, profile_id, f);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

    let order = match f.sort.as_deref() {
        Some("recent") => "COALESCE(j.posted_at, j.first_seen) DESC, m.score DESC",
        Some("salary") => "j.salary_usd_annual DESC NULLS LAST, m.score DESC",
        _ => "m.score DESC",
    };
    let mut page = sqlx::QueryBuilder::new(
        "SELECT m.job_id, m.score, m.reasons, j.title, COALESCE(j.company, d.name, d.host) AS company,
                d.host AS domain, j.location, j.remote_mode, j.url, j.salary_usd_annual,
                j.seniority, j.category, j.employment_type,
                COALESCE(j.posted_at, j.first_seen) AS posted_at",
    );
    push_where(&mut page, profile_id, f);
    page.push(format!(" ORDER BY {order}, j.id LIMIT "));
    page.push_bind(f.limit.unwrap_or(DEFAULT_BROWSE).clamp(1, MAX_BROWSE));
    page.push(" OFFSET ")
        .push_bind(f.offset.unwrap_or(0).max(0));
    let rows = page.build_query_as().fetch_all(pool).await?;
    return Ok((total, rows));
}

/// The values a profile's jobs have for the filter dropdowns, most common first, and how
/// many are strong (>= 70%) or good (>= 50%) matches. Over all the profile's open jobs, so
/// the options don't shrink as filters are applied.
#[derive(Debug, Default, Serialize)]
pub struct Facets {
    pub categories: Vec<(String, i64)>,
    pub seniorities: Vec<(String, i64)>,
    pub countries: Vec<(String, i64)>,
    pub total: i64,
    pub strong: i64,
    pub good: i64,
}

pub async fn facets(pool: &SqlitePool, profile_id: i64) -> anyhow::Result<Facets> {
    let mut out = Facets::default();
    for (column, target) in [
        ("category", &mut out.categories),
        ("seniority", &mut out.seniorities),
        ("country_code", &mut out.countries),
    ] {
        *target = sqlx::query_as(&format!(
            "SELECT j.{column}, COUNT(*) AS n FROM job_matches m JOIN jobs j ON j.id = m.job_id
             WHERE m.profile_id = ? AND j.closed_at IS NULL AND j.{column} IS NOT NULL
             GROUP BY 1 ORDER BY n DESC, 1 LIMIT 40"
        ))
        .bind(profile_id)
        .fetch_all(pool)
        .await?;
    }
    (out.total, out.strong, out.good) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(m.score >= 0.7), 0), COALESCE(SUM(m.score >= 0.5), 0)
         FROM job_matches m JOIN jobs j ON j.id = m.job_id
         WHERE m.profile_id = ? AND j.closed_at IS NULL",
    )
    .bind(profile_id)
    .fetch_one(pool)
    .await?;
    return Ok(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::profile::WeightedSkill;

    fn skill(name: &str, weight: f64) -> WeightedSkill {
        return WeightedSkill {
            name: name.into(),
            weight,
        };
    }

    fn lagos() -> ProfilePlace {
        return ProfilePlace {
            name: "Lagos, NG".into(),
            country_code: Some("NG".into()),
            lat: Some(6.52),
            lon: Some(3.38),
        };
    }

    fn me() -> Profile {
        return Profile {
            titles: vec!["Backend Engineer".into()],
            seniority: Some("senior".into()),
            skills: vec![skill("rust", 1.0), skill("sql", 0.5), skill("aws", 0.5)],
            locations: vec![lagos()],
            salary_expectation_usd: Some(60_000.0),
            ..Profile::default()
        };
    }

    fn job() -> MatchJob {
        return MatchJob {
            id: 1,
            title: "Senior Backend Engineer".into(),
            company: Some("Acme".into()),
            domain: Some("acme.com".into()),
            category: Some("engineering".into()),
            seniority: Some("senior".into()),
            skills: vec!["rust".into(), "sql".into()],
            lat: Some(6.45),
            lon: Some(3.4),
            country_code: Some("NG".into()),
            salary_usd_annual: Some(70_000.0),
            ..MatchJob::default()
        };
    }

    #[test]
    fn a_job_that_fits_everywhere_scores_high_with_reasons() {
        let m = score(&me(), &job());
        assert!(m.score > 0.9, "{m:?}");
        assert!(m.reasons.iter().any(|r| r.starts_with("skills: rust, sql")));
        assert!(m.reasons.iter().any(|r| r.contains("km from Lagos")));
    }

    #[test]
    fn a_different_job_scores_low() {
        let far = MatchJob {
            title: "Regional Sales Director".into(),
            category: Some("sales".into()),
            seniority: Some("director".into()),
            skills: vec!["salesforce".into()],
            lat: Some(40.7),
            lon: Some(-74.0),
            country_code: Some("US".into()),
            salary_usd_annual: Some(30_000.0),
            ..job()
        };
        let m = score(&me(), &far);
        assert!(m.score < 0.3, "{m:?}");
        assert!(score(&me(), &job()).score > m.score);
    }

    #[test]
    fn unknowns_are_neutral_not_penalized() {
        let bare = MatchJob {
            id: 1,
            title: "Engineer".into(),
            ..MatchJob::default()
        };
        let m = score(&Profile::default(), &bare);
        assert!((m.score - 0.5).abs() < 1e-9, "{m:?}");
    }

    #[test]
    fn hard_rules() {
        let mut p = me();
        p.exclude = vec!["Crypto".into()];
        let mut j = job();
        j.description = Some("We build crypto exchanges.".into());
        assert_eq!(score(&p, &j).score, 0.0);

        let mut p = me();
        p.excluded_companies = vec!["acme".into()];
        assert_eq!(score(&p, &job()).score, 0.0);
        p.excluded_companies = vec!["ac".into()];
        assert!(score(&p, &job()).score > 0.5, "too short to match on");

        let mut p = me();
        p.must_have = vec!["kubernetes".into()];
        let m = score(&p, &job());
        assert!(m.score <= MISSING_MUST_HAVE_CAP);
        assert!(m.reasons.last().unwrap().contains("kubernetes"));
        j = job();
        j.description = Some("You will run Kubernetes.".into());
        assert!(score(&p, &j).score > MISSING_MUST_HAVE_CAP);
    }

    #[test]
    fn remote_jobs_depend_on_who_they_are_open_to() {
        let mut j = job();
        j.remote_mode = Some("remote".into());
        j.lat = None;
        j.lon = None;
        j.remote_regions = vec!["NG".into()];
        let open = score(&me(), &j).score;
        j.remote_regions = vec!["US".into()];
        let closed = score(&me(), &j).score;
        j.remote_regions = vec!["global".into()];
        assert!(score(&me(), &j).score >= open - 1e-9);
        assert!(open > closed + 0.1, "{open} vs {closed}");
    }

    #[test]
    fn wanting_remote_work_discounts_offices_and_relocation_softens_distance() {
        let mut p = me();
        let near = score(&p, &job()).score;
        p.remote = Some("remote".into());
        assert!(score(&p, &job()).score < near);

        let mut far = job();
        far.lat = Some(51.5);
        far.lon = Some(-0.1);
        far.country_code = Some("GB".into());
        let mut p = me();
        let stuck = score(&p, &far).score;
        p.relocate = true;
        assert!(score(&p, &far).score > stuck);
    }

    #[test]
    fn seniority_and_salary_shape_the_score() {
        let mut junior = job();
        junior.seniority = Some("junior".into());
        junior.title = "Junior Backend Engineer".into();
        assert!(score(&me(), &junior).score < score(&me(), &job()).score);
        let mut cheap = job();
        cheap.salary_usd_annual = Some(20_000.0);
        assert!(score(&me(), &cheap).score < score(&me(), &job()).score);
    }

    #[tokio::test]
    async fn scores_are_stored_ranked_and_recomputed() {
        let (_dir, pool) = test_pool().await;
        let pid = crate::profile::insert(&pool, "p", "parser", "h", "t", &me())
            .await
            .unwrap();
        for (url, title, cat, skills) in [
            (
                "https://j/1",
                "Senior Backend Engineer",
                "engineering",
                r#"["rust","sql"]"#,
            ),
            (
                "https://j/2",
                "Sales Director",
                "sales",
                r#"["salesforce"]"#,
            ),
        ] {
            sqlx::query(
                "INSERT INTO jobs (url, title, category, skills, source, first_seen, last_seen)
                 VALUES (?, ?, ?, ?, 'jsonld', 1, 1)",
            )
            .bind(url)
            .bind(title)
            .bind(cat)
            .bind(skills)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(rescore_all(&pool, pid, &me()).await.unwrap(), 2);
        let best = top(&pool, pid, 10).await.unwrap();
        assert_eq!(best[0].title, "Senior Backend Engineer");
        // Filters and paging: text narrows, the total ignores the page.
        let filter = |q: &str, limit| MatchFilter {
            q: Some(q.into()),
            limit,
            ..MatchFilter::default()
        };
        let (total, rows) = browse(&pool, pid, &filter("sales", None)).await.unwrap();
        assert_eq!((total, rows.len()), (1, 1));
        let (total, rows) = browse(&pool, pid, &filter("", Some(1))).await.unwrap();
        assert_eq!((total, rows.len()), (2, 1));
        let (total, _) = browse(&pool, pid, &filter("%", None)).await.unwrap();
        assert_eq!(total, 0);
        assert!(best[0].score > best[1].score);
        assert!(best[0].reasons.as_deref().unwrap().contains("skills"));

        // Category filter, recency window (jobs were first seen at t=1) and the facets.
        let by_category = MatchFilter {
            category: Some("sales".into()),
            ..MatchFilter::default()
        };
        assert_eq!(browse(&pool, pid, &by_category).await.unwrap().0, 1);
        let recent = MatchFilter {
            posted_days: Some(7),
            ..MatchFilter::default()
        };
        assert_eq!(browse(&pool, pid, &recent).await.unwrap().0, 0);
        let f = facets(&pool, pid).await.unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.categories.len(), 2);

        // A changed profile re-ranks; closed jobs are not scored.
        let mut sales = me();
        sales.titles = vec!["Sales Director".into()];
        sales.skills = vec![skill("salesforce", 1.0)];
        sales.seniority = Some("director".into());
        sqlx::query("UPDATE jobs SET closed_at = 5 WHERE url = 'https://j/1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(rescore_all(&pool, pid, &sales).await.unwrap(), 1);
        assert_eq!(top(&pool, pid, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn domain_hits_count_good_open_jobs() {
        let (_dir, pool) = test_pool().await;
        let pid = crate::profile::insert(&pool, "p", "parser", "h", "t", &me())
            .await
            .unwrap();
        let did: i64 = sqlx::query_scalar(
            "INSERT INTO domains (host, first_seen) VALUES ('acme.com', 1) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO jobs (url, domain_id, title, category, skills, seniority, source, first_seen, last_seen) VALUES ('https://j/1', ?, 'Senior Backend Engineer', 'engineering', '[\"rust\"]', 'senior', 'jsonld', 1, 1)")
            .bind(did).execute(&pool).await.unwrap();
        rescore_all(&pool, pid, &me()).await.unwrap();
        assert_eq!(domain_hits(&pool, pid, "acme.com").await.unwrap(), 1);
        assert_eq!(domain_hits(&pool, pid, "other.com").await.unwrap(), 0);
    }
}
