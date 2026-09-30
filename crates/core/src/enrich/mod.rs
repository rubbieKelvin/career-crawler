//! Job enrichment: the normalized fields that make search work (category, seniority,
//! skills, city/country/coordinates, remote regions, annual and USD salary). The rules here
//! are deterministic and offline and always run first; the LLM (crawler's `enrich` task)
//! fills what they can't. See `brainstorms/10-llm.md`.

pub mod geo;
pub mod salary;

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, SqlitePool};

pub use geo::Place;

pub const CATEGORIES: &[&str] = &[
    "engineering",
    "data",
    "product",
    "design",
    "sales",
    "marketing",
    "customer_support",
    "operations",
    "finance",
    "hr",
    "legal",
    "security",
    "it",
    "other",
];
pub const SENIORITIES: &[&str] = &[
    "intern",
    "junior",
    "mid",
    "senior",
    "lead",
    "manager",
    "director",
    "executive",
];
pub const REMOTE_MODES: &[&str] = &["onsite", "hybrid", "remote"];

const MAX_SKILLS: usize = 12;
/// How much of a description is scanned for skills.
const SKILL_SCAN_CHARS: usize = 6_000;
/// LLM enrichment gives up on a job after this many failed batches.
pub const MAX_LLM_ATTEMPTS: i64 = 3;

/// The fields of a stored job that enrichment reads.
#[derive(Debug, Clone, PartialEq, Default, FromRow)]
pub struct JobFacts {
    pub id: i64,
    pub title: String,
    pub department: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub country_code: Option<String>,
    pub remote_mode: Option<String>,
    pub salary_min: Option<f64>,
    pub salary_max: Option<f64>,
    pub salary_currency: Option<String>,
    pub salary_period: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Enrichment {
    pub category: Option<String>,
    pub seniority: Option<String>,
    pub skills: Vec<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub remote_mode: Option<String>,
    pub remote_regions: Vec<String>,
    pub salary_annual_min: Option<f64>,
    pub salary_annual_max: Option<f64>,
    pub salary_usd_annual: Option<f64>,
}

/// Whether the words of `keyword` appear in `text` (both already folded to lowercase words
/// separated by single spaces). Short keywords match whole words; longer ones also match
/// as a word prefix (`design` → `designer`).
fn has_word(text: &str, keyword: &str) -> bool {
    if keyword.contains(' ') {
        return format!(" {text} ").contains(&format!(" {keyword} "));
    }
    return text.split(' ').any(|w| {
        if keyword.len() <= 3 {
            w == keyword
        } else {
            w.starts_with(keyword)
        }
    });
}

fn words(s: &str) -> String {
    return s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
}

/// First category whose keywords appear, in a deliberate order (a "Data Engineer" is data,
/// a "Support Engineer" is support, before the generic engineering words).
const CATEGORY_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "security",
        &["security", "infosec", "appsec", "cybersecurity", "pentest"],
    ),
    (
        "data",
        &[
            "data",
            "machine learning",
            "ml",
            "analytics",
            "analyst",
            "scientist",
            "bi",
            "ai",
        ],
    ),
    (
        "design",
        &["design", "ux", "ui", "creative", "illustrat", "animator"],
    ),
    (
        "product",
        &[
            "product manager",
            "product owner",
            "product",
            "head of product",
        ],
    ),
    (
        "it",
        &[
            "it",
            "sysadmin",
            "system administrator",
            "network administrator",
            "helpdesk",
        ],
    ),
    (
        "customer_support",
        &[
            "customer success",
            "customer support",
            "customer service",
            "customer experience",
            "support",
            "care",
        ],
    ),
    (
        "engineering",
        &[
            "engineer",
            "developer",
            "software",
            "devops",
            "sre",
            "programmer",
            "architect",
            "backend",
            "frontend",
            "fullstack",
            "full stack",
            "firmware",
            "embedded",
            "qa",
            "sdet",
            "platform",
            "technical lead",
            "cto",
        ],
    ),
    (
        "sales",
        &[
            "sales",
            "account executive",
            "account manager",
            "business development",
            "bdr",
            "sdr",
            "partnerships",
            "revenue",
        ],
    ),
    (
        "marketing",
        &[
            "marketing",
            "growth",
            "seo",
            "content",
            "brand",
            "communications",
            "social media",
            "copywriter",
            "pr",
        ],
    ),
    (
        "finance",
        &[
            "finance",
            "financial",
            "accountant",
            "accounting",
            "controller",
            "payroll",
            "audit",
            "treasury",
            "bookkeeper",
            "fp a",
        ],
    ),
    (
        "hr",
        &[
            "recruiter",
            "recruiting",
            "talent",
            "people operations",
            "human resources",
            "hr",
            "people partner",
        ],
    ),
    (
        "legal",
        &["legal", "counsel", "attorney", "compliance", "paralegal"],
    ),
    (
        "operations",
        &[
            "operations",
            "logistics",
            "supply chain",
            "program manager",
            "project manager",
            "office manager",
            "administrat",
            "coordinator",
            "procurement",
            "facilities",
        ],
    ),
];

fn category_of(text: &str) -> Option<&'static str> {
    let text = words(text);
    return CATEGORY_KEYWORDS
        .iter()
        .find(|(_, keywords)| keywords.iter().any(|k| has_word(&text, k)))
        .map(|(category, _)| *category);
}

/// From the title first, then the department.
pub fn category(title: &str, department: Option<&str>) -> Option<&'static str> {
    return category_of(title).or_else(|| department.and_then(category_of));
}

const SENIORITY_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "executive",
        &[
            "chief",
            "ceo",
            "cto",
            "cfo",
            "coo",
            "cmo",
            "vp",
            "vice president",
            "president",
        ],
    ),
    ("director", &["director", "head of"]),
    ("lead", &["lead", "staff", "principal", "architect"]),
    ("senior", &["senior", "sr"]),
    ("manager", &["manager"]),
    (
        "junior",
        &["junior", "jr", "entry level", "graduate", "new grad"],
    ),
    (
        "intern",
        &[
            "intern",
            "internship",
            "trainee",
            "apprentice",
            "working student",
        ],
    ),
];

pub fn seniority(title: &str) -> Option<&'static str> {
    let title = words(title);
    return SENIORITY_KEYWORDS
        .iter()
        .find(|(_, keywords)| keywords.iter().any(|k| has_word(&title, k)))
        .map(|(level, _)| *level);
}

/// Skills, as lowercase display names, and the words that signal each.
const SKILLS: &[(&str, &[&str])] = &[
    ("rust", &["rust"]),
    ("go", &["golang"]),
    ("python", &["python"]),
    ("java", &["java"]),
    ("javascript", &["javascript"]),
    ("typescript", &["typescript"]),
    ("react", &["react", "reactjs"]),
    ("vue", &["vue", "vuejs"]),
    ("angular", &["angular"]),
    ("node.js", &["nodejs", "node.js"]),
    ("django", &["django"]),
    ("rails", &["rails", "ruby on rails"]),
    ("ruby", &["ruby"]),
    ("php", &["php"]),
    ("laravel", &["laravel"]),
    ("c++", &["c++"]),
    ("c#", &["c#"]),
    (".net", &[".net", "dotnet"]),
    ("swift", &["swift", "swiftui"]),
    ("kotlin", &["kotlin"]),
    ("android", &["android"]),
    ("ios", &["ios"]),
    ("flutter", &["flutter"]),
    ("sql", &["sql"]),
    ("postgresql", &["postgres", "postgresql"]),
    ("mysql", &["mysql"]),
    ("mongodb", &["mongodb"]),
    ("redis", &["redis"]),
    ("kafka", &["kafka"]),
    ("spark", &["spark"]),
    ("airflow", &["airflow"]),
    ("dbt", &["dbt"]),
    ("snowflake", &["snowflake"]),
    ("aws", &["aws"]),
    ("gcp", &["gcp", "google cloud"]),
    ("azure", &["azure"]),
    ("kubernetes", &["kubernetes", "k8s"]),
    ("docker", &["docker"]),
    ("terraform", &["terraform"]),
    ("linux", &["linux"]),
    ("graphql", &["graphql"]),
    ("machine learning", &["machine learning"]),
    ("pytorch", &["pytorch"]),
    ("tensorflow", &["tensorflow"]),
    ("llm", &["llm", "llms"]),
    ("figma", &["figma"]),
    ("salesforce", &["salesforce"]),
    ("hubspot", &["hubspot"]),
    ("excel", &["excel"]),
    ("tableau", &["tableau"]),
    ("power bi", &["power bi", "powerbi"]),
    ("seo", &["seo"]),
];

/// Like `words`, but keeps the characters that are part of skill names (`c++`, `c#`, `.net`).
fn skill_words(s: &str) -> String {
    return s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '+' | '#' | '.') {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
}

pub fn skills(title: &str, description: Option<&str>) -> Vec<String> {
    let mut text = title.to_string();
    if let Some(d) = description {
        text.push(' ');
        text.extend(d.chars().take(SKILL_SCAN_CHARS));
    }
    let text = skill_words(&text);
    let tokens: std::collections::HashSet<&str> = text.split(' ').collect();
    let padded = format!(" {text} ");
    return SKILLS
        .iter()
        .filter(|(_, signals)| {
            signals.iter().any(|s| {
                if s.contains(' ') {
                    padded.contains(&format!(" {s} "))
                } else {
                    tokens.contains(s)
                }
            })
        })
        .map(|(name, _)| name.to_string())
        .take(MAX_SKILLS)
        .collect();
}

/// The remote mode a location string implies (`Remote`, `Hybrid`), if any.
fn remote_mode_of(location: &str) -> Option<&'static str> {
    let l = words(location);
    if has_word(&l, "hybrid") {
        return Some("hybrid");
    }
    if has_word(&l, "remote") || has_word(&l, "anywhere") || has_word(&l, "worldwide") {
        return Some("remote");
    }
    return None;
}

/// Region words found in location text, and the canonical name each stands for.
const REGION_WORDS: &[(&str, &str)] = &[
    ("emea", "EMEA"),
    ("apac", "APAC"),
    ("latam", "LATAM"),
    ("europe", "Europe"),
    ("north america", "North America"),
    ("americas", "Americas"),
    ("africa", "Africa"),
    ("asia", "Asia"),
    ("worldwide", "global"),
    ("anywhere", "global"),
    ("global", "global"),
];

/// A region a remote job can be open to, as we store it: a named region, `global`, or an
/// ISO country code. Anything else (a model's invention) is `None`.
pub fn remote_region(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some((_, canonical)) = REGION_WORDS
        .iter()
        .find(|(word, _)| word.eq_ignore_ascii_case(value))
    {
        return Some(canonical.to_string());
    }
    return country_code(Some(value));
}

/// Regions a remote job is open to, from its location string: named regions, countries,
/// and the country of a city it names. "Anywhere" is `global`.
fn remote_regions_of(location: &str, place: &Place) -> Vec<String> {
    let l = words(location);
    let mut regions = Vec::new();
    for (word, region) in REGION_WORDS {
        if has_word(&l, word) && !regions.iter().any(|r| r == region) {
            regions.push(region.to_string());
        }
    }
    let country = location
        .split([',', '|', '/', ';', '(', ')', '-', '·'])
        .find_map(geo::country_by_name);
    for cc in [country, place.country_code.as_deref()]
        .into_iter()
        .flatten()
    {
        if !regions.iter().any(|r| r == cc) {
            regions.push(cc.to_string());
        }
    }
    return regions;
}

/// The deterministic pass. Values the source already gave (`country_code`, `remote_mode`)
/// are kept.
pub fn rules(job: &JobFacts) -> Enrichment {
    let location = job.location.as_deref().unwrap_or("");
    let place = geo::geocode(location);
    let remote_mode = job
        .remote_mode
        .clone()
        .or_else(|| remote_mode_of(location).map(str::to_string));
    let remote_regions = if remote_mode.as_deref() == Some("remote") {
        remote_regions_of(location, &place)
    } else {
        Vec::new()
    };
    let pay = salary::normalize(
        job.salary_min,
        job.salary_max,
        job.salary_currency.as_deref(),
        job.salary_period.as_deref(),
    );
    return Enrichment {
        category: category(&job.title, job.department.as_deref()).map(str::to_string),
        seniority: seniority(&job.title).map(str::to_string),
        skills: skills(&job.title, job.description.as_deref()),
        city: place.city,
        region: place.region,
        country_code: job.country_code.clone().or(place.country_code),
        lat: place.lat,
        lon: place.lon,
        remote_mode,
        remote_regions,
        salary_annual_min: pay.annual_min,
        salary_annual_max: pay.annual_max,
        salary_usd_annual: pay.usd_annual,
    };
}

/// Whether the LLM could add something the rules missed: no category, or a location the
/// rules couldn't place. Jobs whose location names nothing placeable ("Remote") don't
/// count, as there is nothing to split.
pub fn needs_llm(job: &JobFacts, found: &Enrichment) -> bool {
    let has_place_text = job
        .location
        .as_deref()
        .is_some_and(|l| !l.trim().is_empty() && remote_mode_of(l).is_none() || l.contains(','));
    return found.category.is_none() || (has_place_text && found.country_code.is_none());
}

/// Checks a value against an allowed list (case-insensitively), returning the list's spelling.
pub fn one_of(value: Option<&str>, allowed: &'static [&'static str]) -> Option<&'static str> {
    let value = value?.trim().to_lowercase().replace([' ', '-'], "_");
    return allowed.iter().find(|a| **a == value).copied();
}

/// A valid ISO 3166-1 alpha-2 code from the table, upper-cased.
pub fn country_code(value: Option<&str>) -> Option<String> {
    let code = value?.trim().to_uppercase();
    return geo::is_country_code(&code).then_some(code);
}

/// Jobs waiting for enrichment: never enriched (`pending`), or, when `with_llm`, those the
/// rules left for the LLM (`rules`) that haven't used up their attempts.
pub async fn pending(
    pool: &SqlitePool,
    with_llm: bool,
    limit: i64,
) -> anyhow::Result<Vec<(JobFacts, String)>> {
    let rows: Vec<(JobFacts, String)> = sqlx::query(
        "SELECT id, title, department, location, description, country_code, remote_mode, salary_min,
                salary_max, salary_currency, salary_period, enrich_state
         FROM jobs
         WHERE enrich_state = 'pending' OR (? AND enrich_state = 'rules' AND enrich_attempts < ?)
         ORDER BY enrich_state = 'pending' DESC, id LIMIT ?",
    )
    .bind(with_llm)
    .bind(MAX_LLM_ATTEMPTS)
    .bind(limit)
    .try_map(|row: sqlx::sqlite::SqliteRow| {
        use sqlx::Row;
        let facts = JobFacts::from_row(&row)?;
        return Ok((facts, row.try_get("enrich_state")?));
    })
    .fetch_all(pool)
    .await?;
    return Ok(rows);
}

/// Writes one job's enrichment and moves it to `state` (`rules` or `done`). A failed LLM
/// attempt passes `count_attempt` to be retried later, a bounded number of times.
pub async fn write(
    conn: &mut SqliteConnection,
    job_id: i64,
    e: &Enrichment,
    state: &str,
    count_attempt: bool,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE jobs SET category = ?, seniority = ?, skills = ?, city = ?, region = ?, country_code = ?,
                lat = ?, lon = ?, remote_mode = ?, remote_regions = ?, salary_annual_min = ?,
                salary_annual_max = ?, salary_usd_annual = ?, enrich_state = ?,
                enrich_attempts = enrich_attempts + ?
         WHERE id = ?",
    )
    .bind(&e.category)
    .bind(&e.seniority)
    .bind(serde_json::to_string(&e.skills)?)
    .bind(&e.city)
    .bind(&e.region)
    .bind(&e.country_code)
    .bind(e.lat)
    .bind(e.lon)
    .bind(&e.remote_mode)
    .bind((!e.remote_regions.is_empty()).then(|| serde_json::to_string(&e.remote_regions)).transpose()?)
    .bind(e.salary_annual_min)
    .bind(e.salary_annual_max)
    .bind(e.salary_usd_annual)
    .bind(state)
    .bind(i64::from(count_attempt))
    .bind(job_id)
    .execute(conn)
    .await?;
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(title: &str, location: Option<&str>) -> JobFacts {
        return JobFacts {
            id: 1,
            title: title.into(),
            location: location.map(str::to_string),
            ..JobFacts::default()
        };
    }

    #[test]
    fn categories_from_titles() {
        for (title, want) in [
            ("Senior Backend Engineer", Some("engineering")),
            ("Data Engineer", Some("data")),
            ("Product Designer", Some("design")),
            ("Product Manager, Payments", Some("product")),
            ("Customer Support Engineer", Some("customer_support")),
            ("Account Executive, EMEA", Some("sales")),
            ("Security Engineer", Some("security")),
            ("Talent Acquisition Partner", Some("hr")),
            ("Staff Accountant", Some("finance")),
            ("IT Support Specialist", Some("it")),
            ("Beekeeper", None),
        ] {
            assert_eq!(category(title, None), want, "{title}");
        }
        assert_eq!(category("Specialist", Some("Marketing")), Some("marketing"));
    }

    #[test]
    fn short_keywords_match_whole_words_only() {
        // "ai" and "it" must not fire inside other words ("maintain", "digital").
        assert_eq!(category("Maintain Fleet", None), None);
        assert_eq!(category("Digital Wizard", None), None);
        assert_eq!(category("AI Researcher", None), Some("data"));
    }

    #[test]
    fn seniority_from_titles() {
        for (title, want) in [
            ("Senior Software Engineer", Some("senior")),
            ("Sr. Designer", Some("senior")),
            ("Staff Engineer", Some("lead")),
            ("Engineering Manager", Some("manager")),
            ("Director of Sales", Some("director")),
            ("VP Engineering", Some("executive")),
            ("Junior Analyst", Some("junior")),
            ("Software Engineering Intern", Some("intern")),
            ("Software Engineer", None),
        ] {
            assert_eq!(seniority(title), want, "{title}");
        }
    }

    #[test]
    fn skills_from_title_and_description() {
        let s = skills(
            "Backend Engineer",
            Some(
                "You'll write Rust and Python, deploy to AWS on Kubernetes, use C++ and .NET. We love Go? No: golang.",
            ),
        );
        for want in ["rust", "python", "aws", "kubernetes", "c++", ".net", "go"] {
            assert!(s.iter().any(|x| x == want), "{want} in {s:?}");
        }
        assert!(skills("Chef", Some("Trust the process, go far")).is_empty());
    }

    #[test]
    fn rules_fill_place_remote_and_pay() {
        let mut job = facts("Senior Data Analyst", Some("Lagos, Nigeria"));
        job.salary_min = Some(400_000.0);
        job.salary_max = Some(600_000.0);
        job.salary_currency = Some("NGN".into());
        job.salary_period = Some("month".into());
        let e = rules(&job);
        assert_eq!(e.category.as_deref(), Some("data"));
        assert_eq!(e.seniority.as_deref(), Some("senior"));
        assert_eq!(
            (e.city.as_deref(), e.country_code.as_deref()),
            (Some("Lagos"), Some("NG"))
        );
        assert_eq!(e.salary_annual_min, Some(4_800_000.0));
        assert_eq!(e.salary_usd_annual, Some(6_000_000.0 * 0.00065));
        assert_eq!(e.remote_mode, None);
        assert!(!needs_llm(&job, &e));
    }

    #[test]
    fn remote_jobs_get_their_regions() {
        let e = rules(&facts("Engineer", Some("Remote - EMEA")));
        assert_eq!(e.remote_mode.as_deref(), Some("remote"));
        assert_eq!(e.remote_regions, ["EMEA"]);
        let e = rules(&facts("Engineer", Some("Remote (Nigeria)")));
        assert_eq!(e.remote_regions, ["NG"]);
        assert_eq!(e.country_code.as_deref(), Some("NG"));
        let e = rules(&facts("Engineer", Some("Anywhere")));
        assert_eq!(e.remote_regions, ["global"]);
        let mut job = facts("Engineer", Some("Berlin"));
        job.remote_mode = Some("hybrid".into());
        let e = rules(&job);
        assert_eq!(
            e.remote_mode.as_deref(),
            Some("hybrid"),
            "the source's mode wins"
        );
        assert!(e.remote_regions.is_empty());
    }

    #[test]
    fn the_llm_is_wanted_only_when_rules_are_stuck() {
        let stuck_category = facts("Beekeeper", Some("Lagos, Nigeria"));
        assert!(needs_llm(&stuck_category, &rules(&stuck_category)));
        let stuck_place = facts("Engineer", Some("Ikoyi Peninsula, Off-Grid Sector 7"));
        assert!(needs_llm(&stuck_place, &rules(&stuck_place)));
        let remote = facts("Engineer", Some("Remote"));
        assert!(!needs_llm(&remote, &rules(&remote)));
        let no_location = facts("Engineer", None);
        assert!(!needs_llm(&no_location, &rules(&no_location)));
    }

    #[test]
    fn validators() {
        assert_eq!(
            one_of(Some("Customer Support"), CATEGORIES),
            Some("customer_support")
        );
        assert_eq!(one_of(Some("astronaut"), CATEGORIES), None);
        assert_eq!(country_code(Some(" ng ")), Some("NG".into()));
        assert_eq!(country_code(Some("ZZ")), None);
        assert_eq!(country_code(Some("Nigeria")), None);
    }
}
