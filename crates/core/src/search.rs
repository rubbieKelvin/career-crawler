//! Job search (milestone 12, `brainstorms/10-llm.md`): a [`JobQuery`] — read out of the user's
//! words by the LLM, or built by the UI's filter chips — becomes parameterized SQL. The filters
//! are a closed set, so the statement is assembled from constants and every user value is bound.
//! There is no free-form text-to-SQL, which is why this module also owns the two things SQL
//! cannot do here: this SQLite has no trigonometry (the radius filter runs in Rust over a
//! bounding-box prefilter) and no idea of "well paid" (a salary percentile is resolved against
//! the jobs the other filters kept).

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::enrich::{self, CATEGORIES, geo};
use crate::time::now_ms;

/// The orders the UI offers; anything else falls back to the default.
pub const SORTS: &[&str] = &["relevance", "salary_desc", "recent", "match"];
/// Work styles a query may pin: no constraint, or exactly one mode.
pub const REMOTE_FILTERS: &[&str] = &["any", "remote", "onsite", "hybrid"];
/// Salary filters: a percentile of what the matching jobs pay, or a floor in USD.
pub const SALARY_MODES: &[&str] = &["top_percentile", "min_usd"];

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_RADIUS_KM: f64 = 50.0;
const MIN_RADIUS_KM: f64 = 1.0;
const MAX_RADIUS_KM: f64 = 500.0;
const MAX_KEYWORDS: usize = 8;
const MAX_KEYWORD_CHARS: usize = 40;
const MAX_CATEGORIES: usize = 5;
const MAX_INDUSTRIES: usize = 3;
const MAX_POSTED_DAYS: i64 = 3_650;
const MAX_EXPLANATION_CHARS: usize = 300;
/// Rows the bounding box may hand to the exact radius check.
const MAX_GEO_CANDIDATES: i64 = 2_000;
/// A job's salary is "nice paying" when it is in the top `value` share of the
/// matching jobs that posted one.
const MIN_PERCENTILE: f64 = 0.01;
const MAX_PERCENTILE: f64 = 0.9;
const MAX_SALARY_USD: f64 = 10_000_000.0;

/// A job search, as the LLM reads it or the UI edits it. Every field is optional: a filter
/// that isn't there simply doesn't apply.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JobQuery {
    /// Words the posting should mention. Any of them may match; relevance ranks them.
    pub keywords: Vec<String>,
    /// From `enrich::CATEGORIES`.
    pub categories: Vec<String>,
    /// Company industries, matched loosely against `domains.industry`.
    pub industries: Vec<String>,
    pub near: Option<Near>,
    /// `any` (no constraint), `remote`, `onsite` or `hybrid`.
    pub remote: Option<String>,
    pub salary: Option<Salary>,
    pub posted_within_days: Option<i64>,
    /// `relevance`, `salary_desc`, `recent` or `match`; the app picks when it's absent.
    pub sort: Option<String>,
    pub limit: Option<i64>,
    /// The model's one-line account of what it understood, for the user to check.
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Near {
    /// A city or country name, as the user said it ("Lagos", "Nigeria", "around Berlin").
    pub place: String,
    pub radius_km: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Salary {
    pub mode: String,
    pub value: f64,
}

impl JobQuery {
    /// The query with harmless corrections applied: unknown values dropped, numbers clamped,
    /// lists trimmed and deduplicated. The caller shows the result — the UI renders it as the
    /// chips — so nothing is corrected silently, and the notes say what was ignored.
    pub fn sanitized(mut self) -> (JobQuery, Vec<String>) {
        let mut notes: Vec<String> = Vec::new();

        let mut keywords: Vec<String> = Vec::new();
        for k in &self.keywords {
            let k: String = k.trim().chars().take(MAX_KEYWORD_CHARS).collect();
            if !k.is_empty()
                && keywords.len() < MAX_KEYWORDS
                && !keywords.iter().any(|e| e.eq_ignore_ascii_case(&k))
            {
                keywords.push(k);
            }
        }
        if keywords.len() < self.keywords.len() {
            notes.push(format!(
                "{} keyword(s) ignored: at most {MAX_KEYWORDS} are used",
                self.keywords.len() - keywords.len()
            ));
        }
        self.keywords = keywords;

        let mut categories: Vec<String> = Vec::new();
        for c in &self.categories {
            match enrich::one_of(Some(c.as_str()), CATEGORIES) {
                Some(k) if categories.iter().any(|e| e.as_str() == k) => {}
                Some(k) => categories.push(k.to_string()),
                None => notes.push(format!("unknown category \"{}\" ignored", c.trim())),
            }
        }
        categories.truncate(MAX_CATEGORIES);
        self.categories = categories;

        let mut industries: Vec<String> = Vec::new();
        for i in &self.industries {
            let i = i.trim().to_lowercase();
            if !i.is_empty() && industries.len() < MAX_INDUSTRIES && !industries.contains(&i) {
                industries.push(i);
            }
        }
        self.industries = industries;

        self.near = match self.near.take() {
            Some(n) if !n.place.trim().is_empty() => Some(Near {
                place: n.place.trim().chars().take(80).collect(),
                radius_km: Some(
                    n.radius_km
                        .unwrap_or(DEFAULT_RADIUS_KM)
                        .clamp(MIN_RADIUS_KM, MAX_RADIUS_KM),
                ),
            }),
            Some(_) => {
                notes.push("\"near\" named no place: ignored".into());
                None
            }
            None => None,
        };

        self.remote = match self.remote.as_deref() {
            None => None,
            Some(r) => match enrich::one_of(Some(r), REMOTE_FILTERS) {
                Some("any") => None,
                Some(k) => Some(k.to_string()),
                None => {
                    notes.push(format!("unknown work style \"{}\" ignored", r.trim()));
                    None
                }
            },
        };

        self.salary = match self.salary.take() {
            None => None,
            Some(s) => match enrich::one_of(Some(s.mode.as_str()), SALARY_MODES) {
                Some("top_percentile") if s.value.is_finite() && s.value > 0.0 => Some(Salary {
                    mode: "top_percentile".into(),
                    value: s.value.clamp(MIN_PERCENTILE, MAX_PERCENTILE),
                }),
                Some("min_usd") if s.value.is_finite() && s.value >= 1.0 => Some(Salary {
                    mode: "min_usd".into(),
                    value: s.value.min(MAX_SALARY_USD),
                }),
                _ => {
                    notes.push(format!(
                        "salary filter \"{} {}\" ignored",
                        s.mode.trim(),
                        s.value
                    ));
                    None
                }
            },
        };

        self.posted_within_days = self.posted_within_days.map(|d| d.clamp(1, MAX_POSTED_DAYS));

        self.sort = match self.sort.as_deref() {
            None => None,
            Some(s) => match enrich::one_of(Some(s), SORTS) {
                Some(k) => Some(k.to_string()),
                None => {
                    notes.push(format!("unknown sort \"{}\" ignored", s.trim()));
                    None
                }
            },
        };

        self.limit = Some(self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
        self.explanation = self
            .explanation
            .map(|e| e.trim().chars().take(MAX_EXPLANATION_CHARS).collect())
            .filter(|e: &String| !e.is_empty());
        return (self, notes);
    }
}

/// One job the search kept, plus how it was ranked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub id: i64,
    pub title: String,
    pub company: Option<String>,
    pub domain: Option<String>,
    pub location: Option<String>,
    pub remote_mode: Option<String>,
    pub url: String,
    pub salary_min: Option<f64>,
    pub salary_max: Option<f64>,
    pub salary_currency: Option<String>,
    pub salary_period: Option<String>,
    pub salary_usd_annual: Option<f64>,
    pub category: Option<String>,
    pub seniority: Option<String>,
    pub skills: Vec<String>,
    pub city: Option<String>,
    pub country_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub posted_at: Option<i64>,
    pub first_seen: i64,
    /// The active profile's fit, when there is one.
    pub match_score: Option<f64>,
    /// The words' BM25 score, when the search ranked by it.
    pub relevance: Option<f64>,
    /// Kilometres from the place the query asked around, when it asked for one.
    pub distance_km: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub hits: Vec<SearchHit>,
    /// What the search did that the query didn't literally say: the salary threshold it
    /// applied, a place it couldn't resolve, the order it fell back to.
    pub notes: Vec<String>,
    /// The order actually used: `relevance`, `salary_desc`, `recent` or `match`.
    pub sort: String,
}

/// A bound value. The statement is assembled first, so binds are pushed in the order the
/// placeholders appear: the profile id (from the join), then the filters, then the limit.
#[derive(Debug, Clone, PartialEq)]
enum Bind {
    Text(String),
    Real(f64),
    Int(i64),
}

const FROM: &str = "FROM jobs j LEFT JOIN domains d ON d.id = j.domain_id";
const PROFILE_JOIN: &str = " LEFT JOIN job_matches m ON m.job_id = j.id AND m.profile_id = ?";
const COLUMNS: &str = "j.id, j.title, COALESCE(j.company, d.name, d.host) AS company, d.host AS domain,
        j.location, j.remote_mode, j.url, j.salary_min, j.salary_max, j.salary_currency, j.salary_period,
        j.salary_usd_annual, j.category, j.seniority, j.skills, j.city, j.country_code, j.posted_at,
        j.first_seen, j.lat, j.lon";
/// Remote jobs open to `country` (or to everyone, in the second form) count as within reach
/// of a place: someone looking around Lagos can take a remote role open to Nigeria.
const REMOTE_OPEN_COUNTRY: &str = "(j.remote_mode = 'remote' AND (j.remote_regions IS NULL
     OR j.remote_regions = '[]'
     OR EXISTS (SELECT 1 FROM json_each(j.remote_regions) WHERE json_each.value IN ('global', ?))))";
const REMOTE_OPEN_ANY: &str = "(j.remote_mode = 'remote' AND (j.remote_regions IS NULL
     OR j.remote_regions = '[]'
     OR EXISTS (SELECT 1 FROM json_each(j.remote_regions) WHERE json_each.value = 'global')))";

/// Runs the search. `profile_id` is the active profile, when there is one: it ranks the
/// results and is what `match` sorts by.
pub async fn run(
    pool: &SqlitePool,
    query: &JobQuery,
    profile_id: Option<i64>,
) -> anyhow::Result<Outcome> {
    let now = now_ms();
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut notes: Vec<String> = Vec::new();

    let radius = query
        .near
        .as_ref()
        .map(|n| n.radius_km.unwrap_or(DEFAULT_RADIUS_KM));
    let place = query
        .near
        .as_ref()
        .map(|n| geo::geocode(&n.place))
        .filter(|p| !p.is_empty());
    if let (Some(near), None) = (query.near.as_ref(), place.as_ref()) {
        notes.push(format!(
            "couldn't place \"{}\": searching everywhere",
            near.place
        ));
    }
    let center = place.as_ref().and_then(|p| Some((p.lat?, p.lon?)));
    let bbox = center.map(|(lat, lon)| geo::bbox(lat, lon, radius.unwrap_or(DEFAULT_RADIUS_KM)));

    let join = match profile_id {
        Some(_) => format!("{FROM}{PROFILE_JOIN}"),
        None => FROM.to_string(),
    };
    let mut binds: Vec<Bind> = profile_id.map(Bind::Int).into_iter().collect();
    let (mut clauses, filter_binds) = filters(query, place.as_ref(), bbox, now);
    binds.extend(filter_binds);
    let sort = resolve_sort(query, profile_id.is_some(), &mut notes);

    match query.salary.as_ref() {
        Some(s) if s.mode == "top_percentile" => {
            let salary = "j.salary_usd_annual IS NOT NULL";
            let count: i64 = prepared(
                &format!(
                    "SELECT COUNT(*) {join} {}",
                    where_sql(&clauses, Some(salary))
                ),
                &binds,
            )
            .fetch_one(pool)
            .await?
            .try_get(0)?;
            if count == 0 {
                notes.push("no job matching the other filters posted a salary".into());
                return Ok(Outcome {
                    hits: Vec::new(),
                    notes,
                    sort: sort.to_string(),
                });
            }
            // The top `value` share of the jobs that posted one; the threshold is inclusive.
            let offset = ((count as f64 * s.value).ceil() as i64 - 1).clamp(0, count - 1);
            let threshold: f64 = prepared(
                &format!(
                    "SELECT j.salary_usd_annual {join} {} ORDER BY j.salary_usd_annual DESC LIMIT 1 OFFSET {offset}",
                    where_sql(&clauses, Some(salary))
                ),
                &binds,
            )
            .fetch_one(pool)
            .await?
            .try_get(0)?;
            notes.push(format!(
                "the top {}% of the {count} matching jobs that posted a salary is ${threshold:.0}/yr or more",
                (s.value * 100.0).round()
            ));
            clauses.push("j.salary_usd_annual >= ?".into());
            binds.push(Bind::Real(threshold));
        }
        Some(s) if s.mode == "min_usd" => {
            notes.push(format!(
                "paying at least ${:.0}/yr; jobs that posted none can't match",
                s.value
            ));
            clauses.push("j.salary_usd_annual >= ?".into());
            binds.push(Bind::Real(s.value));
        }
        _ => {}
    }

    let mut columns = COLUMNS.to_string();
    let mut main_join = join;
    if profile_id.is_some() {
        columns.push_str(", m.score AS match_score");
    }
    if sort == "relevance" {
        columns.push_str(", bm25(jobs_fts, 8.0, 1.0, 2.0) AS relevance");
        main_join.push_str(" JOIN jobs_fts ON jobs_fts.rowid = j.id");
    }
    // A radius search cuts in Rust, after the exact distance is known, so the SQL only caps
    // how many candidates it looks at.
    let sql_limit = if bbox.is_some() {
        MAX_GEO_CANDIDATES.max(limit)
    } else {
        limit
    };
    let sql = format!(
        "SELECT {columns} {main_join} {} ORDER BY {} LIMIT ?",
        where_sql(&clauses, None),
        order_sql(sort)
    );
    binds.push(Bind::Int(sql_limit));
    let rows = prepared(&sql, &binds).fetch_all(pool).await?;
    if bbox.is_some() && rows.len() as i64 >= sql_limit {
        notes.push(format!(
            "checked the {sql_limit} best-placed jobs in that area; widen the radius or narrow the filters"
        ));
    }
    let mut hits: Vec<SearchHit> = rows.iter().map(row_to_hit).collect::<Result<_, _>>()?;

    if let Some((lat, lon)) = center {
        let radius = radius.unwrap_or(DEFAULT_RADIUS_KM);
        hits.retain(|h| {
            return match (h.lat, h.lon) {
                (Some(la), Some(lo)) => geo::distance_km((lat, lon), (la, lo)) <= radius,
                // No coordinates: the SQL already checked the country (or a remote opening),
                // which is all there is to go on.
                _ => true,
            };
        });
        for h in hits.iter_mut() {
            if let (Some(la), Some(lo)) = (h.lat, h.lon) {
                h.distance_km =
                    Some((geo::distance_km((lat, lon), (la, lo)) * 10.0).round() / 10.0);
            }
        }
    }
    hits.truncate(limit as usize);
    return Ok(Outcome {
        hits,
        notes,
        sort: sort.to_string(),
    });
}

/// The default order: the profile when there is one, the words when there are any, newest
/// otherwise. An order that can't apply (no profile to match, no words to rank) becomes
/// `recent`, and says so.
fn resolve_sort(query: &JobQuery, has_profile: bool, notes: &mut Vec<String>) -> &'static str {
    let has_keywords = !query.keywords.is_empty();
    let wanted = query.sort.as_deref().unwrap_or(if has_profile {
        "match"
    } else if has_keywords {
        "relevance"
    } else {
        "recent"
    });
    return match wanted {
        "match" if has_profile => "match",
        "match" => {
            notes.push("no CV profile to rank by: newest first".into());
            "recent"
        }
        "relevance" if has_keywords => "relevance",
        "relevance" => {
            notes.push("no keywords to rank by: newest first".into());
            "recent"
        }
        "salary_desc" => "salary_desc",
        _ => "recent",
    };
}

fn order_sql(sort: &str) -> &'static str {
    return match sort {
        "relevance" => "bm25(jobs_fts, 8.0, 1.0, 2.0), j.id DESC",
        // Jobs that posted no salary can't be said to pay well, so they come last.
        "salary_desc" => "j.salary_usd_annual IS NULL, j.salary_usd_annual DESC, j.id DESC",
        "match" => "m.score IS NULL, m.score DESC, j.id DESC",
        _ => "COALESCE(j.posted_at, j.first_seen) DESC, j.id DESC",
    };
}

/// The `WHERE` for a set of clauses, with `extra` appended as one more `AND` (which is what
/// the salary threshold needs after the percentile is known).
fn where_sql(clauses: &[String], extra: Option<&str>) -> String {
    let mut all: Vec<&str> = clauses.iter().map(String::as_str).collect();
    if let Some(extra) = extra {
        all.push(extra);
    }
    return if all.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", all.join(" AND "))
    };
}

/// The applied filters, as SQL clauses plus their binds.
fn filters(
    query: &JobQuery,
    place: Option<&geo::Place>,
    bbox: Option<(f64, f64, f64, f64)>,
    now: i64,
) -> (Vec<String>, Vec<Bind>) {
    // Only open postings: a search is for jobs you could take.
    let mut clauses: Vec<String> = vec!["j.closed_at IS NULL".into()];
    let mut binds: Vec<Bind> = Vec::new();

    if !query.keywords.is_empty() {
        clauses.push("j.id IN (SELECT rowid FROM jobs_fts WHERE jobs_fts MATCH ?)".into());
        binds.push(Bind::Text(fts_expr(&query.keywords)));
    }
    if !query.categories.is_empty() {
        let holes = vec!["?"; query.categories.len()].join(",");
        clauses.push(format!("j.category IN ({holes})"));
        binds.extend(query.categories.iter().map(|c| Bind::Text(c.clone())));
    }
    if !query.industries.is_empty() {
        // The classifier's industry is free text ("fintech", "Financial services"), so this
        // is a loose contains rather than an equality.
        let parts: Vec<&str> = query
            .industries
            .iter()
            .map(|_| "instr(LOWER(COALESCE(d.industry, '')), ?) > 0")
            .collect();
        clauses.push(format!("({})", parts.join(" OR ")));
        binds.extend(query.industries.iter().map(|i| Bind::Text(i.clone())));
    }
    if let Some(mode) = &query.remote {
        clauses.push("j.remote_mode = ?".into());
        binds.push(Bind::Text(mode.clone()));
    }
    if let Some(days) = query.posted_within_days {
        // Many sources give no posting date; the day we first saw it is the next best thing.
        clauses.push("COALESCE(j.posted_at, j.first_seen) >= ?".into());
        binds.push(Bind::Int(now - days * 24 * 60 * 60 * 1000));
    }
    let country = place.and_then(|p| p.country_code.as_deref());
    if let Some((min_lat, max_lat, min_lon, max_lon)) = bbox {
        let mut parts = vec!["(j.lat BETWEEN ? AND ? AND j.lon BETWEEN ? AND ?)".to_string()];
        binds.push(Bind::Real(min_lat));
        binds.push(Bind::Real(max_lat));
        binds.push(Bind::Real(min_lon));
        binds.push(Bind::Real(max_lon));
        // This SQL has no trigonometry, so the box is only a prefilter: `run` cuts it back to
        // the circle afterwards. Jobs it can't place exactly stay in for that cut.
        match country {
            Some(cc) => {
                parts.push("(j.lat IS NULL AND j.country_code = ?)".to_string());
                binds.push(Bind::Text(cc.to_string()));
                parts.push(REMOTE_OPEN_COUNTRY.to_string());
                binds.push(Bind::Text(cc.to_string()));
            }
            None => parts.push(REMOTE_OPEN_ANY.to_string()),
        }
        clauses.push(format!("({})", parts.join(" OR ")));
    } else if let Some(cc) = country {
        clauses.push(format!("(j.country_code = ? OR {REMOTE_OPEN_COUNTRY})"));
        binds.push(Bind::Text(cc.to_string()));
        binds.push(Bind::Text(cc.to_string()));
    }
    return (clauses, binds);
}

/// The FTS5 `MATCH` expression: each keyword as a quoted phrase, any of which may match
/// (relevance sorts them). Quoting keeps the user's punctuation from being read as FTS syntax.
fn fts_expr(keywords: &[String]) -> String {
    let terms: Vec<String> = keywords
        .iter()
        .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
        .collect();
    return terms.join(" OR ");
}

/// The statement with its binds applied, in placeholder order.
fn prepared<'q>(
    sql: &'q str,
    binds: &'q [Bind],
) -> sqlx::query::Query<'q, sqlx::Sqlite, SqliteArguments<'q>> {
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = match bind {
            Bind::Text(s) => query.bind(s.as_str()),
            Bind::Real(v) => query.bind(*v),
            Bind::Int(v) => query.bind(*v),
        };
    }
    return query;
}

fn row_to_hit(row: &SqliteRow) -> anyhow::Result<SearchHit> {
    let skills: Vec<String> = row
        .try_get::<Option<String>, _>("skills")?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    return Ok(SearchHit {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        company: row.try_get("company")?,
        domain: row.try_get("domain")?,
        location: row.try_get("location")?,
        remote_mode: row.try_get("remote_mode")?,
        url: row.try_get("url")?,
        salary_min: row.try_get("salary_min")?,
        salary_max: row.try_get("salary_max")?,
        salary_currency: row.try_get("salary_currency")?,
        salary_period: row.try_get("salary_period")?,
        salary_usd_annual: row.try_get("salary_usd_annual")?,
        category: row.try_get("category")?,
        seniority: row.try_get("seniority")?,
        skills,
        city: row.try_get("city")?,
        country_code: row.try_get("country_code")?,
        lat: row.try_get("lat")?,
        lon: row.try_get("lon")?,
        posted_at: row.try_get("posted_at")?,
        first_seen: row.try_get("first_seen")?,
        // Selected only when they apply, so a missing column is simply no value.
        match_score: row.try_get("match_score").unwrap_or(None),
        relevance: row.try_get("relevance").unwrap_or(None),
        distance_km: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::enrich::Enrichment;
    use crate::jobs::{self, Job};

    async fn domain(pool: &SqlitePool, id: i64, host: &str, industry: Option<&str>) {
        sqlx::query(
            "INSERT INTO domains (id, host, name, status, industry, first_seen)
             VALUES (?, ?, ?, 'company', ?, 0)",
        )
        .bind(id)
        .bind(host)
        .bind(host)
        .bind(industry)
        .execute(pool)
        .await
        .unwrap();
    }

    /// An open job, enriched with `e`, as the crawler would have stored it.
    async fn add(
        pool: &SqlitePool,
        domain_id: Option<i64>,
        url: &str,
        title: &str,
        description: Option<&str>,
        e: Enrichment,
    ) -> i64 {
        let mut conn = pool.acquire().await.unwrap();
        let job = Job {
            url: url.into(),
            title: title.into(),
            description: description.map(str::to_string),
            source: "jsonld".into(),
            ..Job::default()
        };
        jobs::upsert(&mut conn, &job, domain_id, None, 0)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE url = ?")
            .bind(url)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        enrich::write(&mut conn, id, &e, "done", false)
            .await
            .unwrap();
        return id;
    }

    fn enriched(category: &str, skills: &[&str]) -> Enrichment {
        return Enrichment {
            category: Some(category.into()),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            ..Enrichment::default()
        };
    }

    fn query(keywords: &[&str]) -> JobQuery {
        return JobQuery {
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            ..JobQuery::default()
        };
    }

    async fn titles(pool: &SqlitePool, q: &JobQuery) -> (Vec<String>, Outcome) {
        let outcome = run(pool, q, None).await.unwrap();
        return (
            outcome.hits.iter().map(|h| h.title.clone()).collect(),
            outcome,
        );
    }

    #[tokio::test]
    async fn keywords_reach_titles_descriptions_and_skills() {
        let (_dir, pool) = test_pool().await;
        domain(&pool, 1, "acme.com", Some("fintech")).await;
        add(
            &pool,
            Some(1),
            "https://acme.com/1",
            "Rust Engineer",
            Some("We build payment rails in Rust."),
            enriched("engineering", &["rust"]),
        )
        .await;
        add(
            &pool,
            Some(1),
            "https://acme.com/2",
            "Pastry Chef",
            Some("Sourdough, viennoiserie, early mornings."),
            enriched("other", &[]),
        )
        .await;

        let (found, _) = titles(&pool, &query(&["rust"])).await;
        assert_eq!(found, ["Rust Engineer"], "title and skills are indexed");
        let (found, _) = titles(&pool, &query(&["sourdough"])).await;
        assert_eq!(found, ["Pastry Chef"], "the description is indexed");
        let (found, _) = titles(&pool, &query(&["rust", "sourdough"])).await;
        assert_eq!(found.len(), 2, "any keyword may match");
        let (found, _) = titles(&pool, &query(&["cheesemonger"])).await;
        assert!(found.is_empty());

        // Rewriting a title keeps the index in step (the update trigger).
        sqlx::query("UPDATE jobs SET title = 'Rust Pastry Chef' WHERE url = 'https://acme.com/2'")
            .execute(&pool)
            .await
            .unwrap();
        let (found, _) = titles(&pool, &query(&["rust"])).await;
        assert_eq!(found.len(), 2);
        let (found, _) = titles(&pool, &query(&["pastry"])).await;
        assert_eq!(found, ["Rust Pastry Chef"]);
    }

    #[tokio::test]
    async fn filters_narrow_by_category_industry_work_style_and_date() {
        let (_dir, pool) = test_pool().await;
        domain(&pool, 1, "acme.com", Some("fintech")).await;
        domain(&pool, 2, "bank.example", Some("Financial Services")).await;
        let remote = add(
            &pool,
            Some(1),
            "https://acme.com/1",
            "Backend Engineer",
            None,
            Enrichment {
                remote_mode: Some("remote".into()),
                ..enriched("engineering", &[])
            },
        )
        .await;
        assert!(remote > 0);
        sqlx::query("UPDATE jobs SET posted_at = ? WHERE id = ?")
            .bind(now_ms())
            .bind(remote)
            .execute(&pool)
            .await
            .unwrap();
        let onsite = add(
            &pool,
            Some(2),
            "https://bank.example/1",
            "Data Analyst",
            None,
            enriched("data", &[]),
        )
        .await;
        sqlx::query("UPDATE jobs SET posted_at = ? WHERE id = ?")
            .bind(now_ms() - 400 * 24 * 60 * 60 * 1000)
            .bind(onsite)
            .execute(&pool)
            .await
            .unwrap();
        // A closed job never shows up, whatever the filters.
        let closed = add(
            &pool,
            Some(1),
            "https://acme.com/2",
            "Frontend Engineer",
            None,
            enriched("engineering", &[]),
        )
        .await;
        sqlx::query("UPDATE jobs SET closed_at = 1 WHERE id = ?")
            .bind(closed)
            .execute(&pool)
            .await
            .unwrap();

        let engineering = JobQuery {
            categories: vec!["engineering".into()],
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &engineering).await;
        assert_eq!(found, ["Backend Engineer"], "closed jobs are left out");

        let fintech = JobQuery {
            industries: vec!["fintech".into()],
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &fintech).await;
        assert_eq!(found, ["Backend Engineer"]);
        let banking = JobQuery {
            industries: vec!["financial".into()],
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &banking).await;
        assert_eq!(found, ["Data Analyst"], "industry matching is loose");

        let remote_only = JobQuery {
            remote: Some("remote".into()),
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &remote_only).await;
        assert_eq!(found, ["Backend Engineer"]);

        let recent = JobQuery {
            posted_within_days: Some(30),
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &recent).await;
        assert_eq!(found, ["Backend Engineer"], "the old posting is out");
    }

    #[tokio::test]
    async fn a_salary_percentile_is_relative_to_the_matched_jobs() {
        let (_dir, pool) = test_pool().await;
        domain(&pool, 1, "acme.com", Some("fintech")).await;
        for (i, usd) in [10_000.0, 20_000.0, 30_000.0, 40_000.0].iter().enumerate() {
            add(
                &pool,
                Some(1),
                &format!("https://acme.com/{i}"),
                &format!("Engineer {i}"),
                None,
                Enrichment {
                    salary_usd_annual: Some(*usd),
                    ..enriched("engineering", &[])
                },
            )
            .await;
        }
        add(
            &pool,
            Some(1),
            "https://acme.com/none",
            "Engineer, no pay posted",
            None,
            enriched("engineering", &[]),
        )
        .await;

        let top_quarter = JobQuery {
            salary: Some(Salary {
                mode: "top_percentile".into(),
                value: 0.25,
            }),
            ..JobQuery::default()
        };
        let (found, outcome) = titles(&pool, &top_quarter).await;
        assert_eq!(found, ["Engineer 3"], "the top quarter of four is one job");
        assert!(
            outcome.notes.iter().any(|n| n.contains("top 25%")),
            "{:?}",
            outcome.notes
        );
        assert_eq!(
            outcome.hits[0].salary_usd_annual,
            Some(40_000.0),
            "jobs without a posted salary can't match a salary filter"
        );

        let floor = JobQuery {
            salary: Some(Salary {
                mode: "min_usd".into(),
                value: 25_000.0,
            }),
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &floor).await;
        assert_eq!(found, ["Engineer 3", "Engineer 2"]);

        let impossible = JobQuery {
            salary: Some(Salary {
                mode: "top_percentile".into(),
                value: 0.25,
            }),
            categories: vec!["legal".into()],
            ..JobQuery::default()
        };
        let (found, outcome) = titles(&pool, &impossible).await;
        assert!(found.is_empty());
        assert!(
            outcome.notes.iter().any(|n| n.contains("posted a salary")),
            "{:?}",
            outcome.notes
        );
    }

    #[tokio::test]
    async fn a_place_keeps_what_is_near_and_what_is_remote_open_to_it() {
        let (_dir, pool) = test_pool().await;
        domain(&pool, 1, "acme.com", None).await;
        add(
            &pool,
            Some(1),
            "https://acme.com/lagos",
            "Lagos Engineer",
            None,
            Enrichment {
                city: Some("Lagos".into()),
                country_code: Some("NG".into()),
                lat: Some(6.5244),
                lon: Some(3.3792),
                ..enriched("engineering", &[])
            },
        )
        .await;
        add(
            &pool,
            Some(1),
            "https://acme.com/abuja",
            "Abuja Engineer",
            None,
            Enrichment {
                city: Some("Abuja".into()),
                country_code: Some("NG".into()),
                lat: Some(9.0579),
                lon: Some(7.4951),
                ..enriched("engineering", &[])
            },
        )
        .await;
        add(
            &pool,
            Some(1),
            "https://acme.com/remote-ng",
            "Remote Engineer, Nigeria",
            None,
            Enrichment {
                remote_mode: Some("remote".into()),
                remote_regions: vec!["NG".into()],
                ..enriched("engineering", &[])
            },
        )
        .await;
        add(
            &pool,
            Some(1),
            "https://acme.com/remote-emea",
            "Remote Engineer, EMEA",
            None,
            Enrichment {
                remote_mode: Some("remote".into()),
                remote_regions: vec!["EMEA".into()],
                ..enriched("engineering", &[])
            },
        )
        .await;
        add(
            &pool,
            Some(1),
            "https://acme.com/lagos-unplaced",
            "Lagos Engineer, no coordinates",
            None,
            Enrichment {
                city: Some("Lagos".into()),
                country_code: Some("NG".into()),
                ..enriched("engineering", &[])
            },
        )
        .await;

        let near_lagos = JobQuery {
            near: Some(Near {
                place: "Lagos".into(),
                radius_km: Some(50.0),
            }),
            ..JobQuery::default()
        };
        let (found, outcome) = titles(&pool, &near_lagos).await;
        assert_eq!(
            found.len(),
            3,
            "within 50 km, remote open to Nigeria, and the unplaced Nigerian one: {found:?}"
        );
        assert!(found.contains(&"Lagos Engineer".to_string()));
        assert!(found.contains(&"Remote Engineer, Nigeria".to_string()));
        assert!(found.contains(&"Lagos Engineer, no coordinates".to_string()));
        assert!(
            outcome
                .hits
                .iter()
                .all(|h| h.distance_km.unwrap_or(0.0) < 1.0 || h.lat.is_none())
        );

        // A country with no city in the table still filters, on the country.
        let near_nigeria = JobQuery {
            near: Some(Near {
                place: "Nigeria".into(),
                radius_km: None,
            }),
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &near_nigeria).await;
        assert_eq!(
            found.len(),
            4,
            "all but the EMEA-only remote job: {found:?}"
        );

        let nowhere = JobQuery {
            near: Some(Near {
                place: "Xyzzyland".into(),
                radius_km: Some(50.0),
            }),
            ..JobQuery::default()
        };
        let (found, outcome) = titles(&pool, &nowhere).await;
        assert_eq!(found.len(), 5, "an unknown place filters nothing");
        assert!(
            outcome.notes.iter().any(|n| n.contains("couldn't place")),
            "{:?}",
            outcome.notes
        );
    }

    #[tokio::test]
    async fn orders_and_the_limit() {
        let (_dir, pool) = test_pool().await;
        domain(&pool, 1, "acme.com", None).await;
        let low = add(
            &pool,
            Some(1),
            "https://acme.com/low",
            "Low",
            None,
            Enrichment {
                salary_usd_annual: Some(10_000.0),
                ..enriched("engineering", &[])
            },
        )
        .await;
        let high = add(
            &pool,
            Some(1),
            "https://acme.com/high",
            "High",
            None,
            Enrichment {
                salary_usd_annual: Some(50_000.0),
                ..enriched("engineering", &[])
            },
        )
        .await;
        let none = add(
            &pool,
            Some(1),
            "https://acme.com/none",
            "Unpaid unknown",
            None,
            enriched("engineering", &[]),
        )
        .await;
        for (id, days) in [(low, 10), (high, 5), (none, 1)] {
            sqlx::query("UPDATE jobs SET posted_at = ? WHERE id = ?")
                .bind(now_ms() - days * 24 * 60 * 60 * 1000)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }

        let by_pay = JobQuery {
            sort: Some("salary_desc".into()),
            ..JobQuery::default()
        };
        let (found, outcome) = titles(&pool, &by_pay).await;
        assert_eq!(
            found,
            ["High", "Low", "Unpaid unknown"],
            "no salary goes last"
        );
        assert_eq!(outcome.sort, "salary_desc");

        let (found, _) = titles(&pool, &JobQuery::default()).await;
        assert_eq!(found, ["Unpaid unknown", "High", "Low"], "newest first");

        let one = JobQuery {
            limit: Some(1),
            ..JobQuery::default()
        };
        let (found, _) = titles(&pool, &one).await;
        assert_eq!(found.len(), 1);

        // No profile: a match sort has nothing to rank by.
        let by_match = JobQuery {
            sort: Some("match".into()),
            ..JobQuery::default()
        };
        let (_, outcome) = titles(&pool, &by_match).await;
        assert_eq!(outcome.sort, "recent");
        assert!(
            outcome.notes.iter().any(|n| n.contains("profile")),
            "{:?}",
            outcome.notes
        );
    }

    #[test]
    fn sanitizing_drops_nonsense_and_clamps_numbers() {
        let (query, notes) = JobQuery {
            keywords: (0..10).map(|i| format!("rust {i}")).collect(),
            categories: vec!["Engineering".into(), "wizard".into()],
            industries: vec![" Fintech ".into(), "fintech".into(), "a".into(), "b".into()],
            near: Some(Near {
                place: "  Lagos ".into(),
                radius_km: Some(9_999.0),
            }),
            remote: Some("sometimes".into()),
            salary: Some(Salary {
                mode: "top_percentile".into(),
                value: 3.0,
            }),
            posted_within_days: Some(100_000),
            sort: Some("sideways".into()),
            limit: Some(5_000),
            explanation: Some("   ".into()),
        }
        .sanitized();

        assert_eq!(query.keywords.len(), MAX_KEYWORDS);
        assert_eq!(query.categories, ["engineering"]);
        assert_eq!(query.industries, ["fintech", "a", "b"]);
        let near = query.near.clone().expect("the place survives");
        assert_eq!(near.place, "Lagos");
        assert_eq!(near.radius_km, Some(MAX_RADIUS_KM));
        assert_eq!(query.remote, None);
        assert_eq!(query.salary.as_ref().unwrap().value, MAX_PERCENTILE);
        assert_eq!(query.posted_within_days, Some(MAX_POSTED_DAYS));
        assert_eq!(query.sort, None);
        assert_eq!(query.limit, Some(MAX_LIMIT));
        assert_eq!(query.explanation, None);
        for want in ["wizard", "sometimes", "sideways", "keyword"] {
            assert!(
                notes.iter().any(|n| n.contains(want)),
                "{want} not explained in {notes:?}"
            );
        }
    }

    #[test]
    fn quoting_keeps_punctuation_out_of_the_match_syntax() {
        assert_eq!(
            fts_expr(&["c++".into(), "a \"b\"".into()]),
            "\"c++\" OR \"a \"\"b\"\"\""
        );
    }
}
