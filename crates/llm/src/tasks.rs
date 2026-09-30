//! The LLM tasks: their versioned prompts and the JSON shapes they answer with. Building a
//! task's input from crawl data is the caller's job; validating what comes back is too,
//! since a model can return well-formed JSON with nonsense in it.

use serde::{Deserialize, Serialize};

use crate::Prompt;

/// Bumping a version invalidates the cache for that task only.
pub const CLASSIFY_DOMAIN: Prompt = Prompt {
    task: "classify_domain",
    version: 1,
    system: include_str!("../prompts/classify_domain.v1.md"),
};

pub const ENRICH_JOBS: Prompt = Prompt {
    task: "enrich_jobs",
    version: 1,
    system: include_str!("../prompts/enrich_jobs.v1.md"),
};

/// The answer to `CLASSIFY_DOMAIN`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainVerdict {
    pub is_company: bool,
    #[serde(default)]
    pub company_name: Option<String>,
    #[serde(default)]
    pub industry: Option<String>,
    #[serde(default)]
    pub hq_country: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

/// The answer to `ENRICH_JOBS`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnrichedJobs {
    pub jobs: Vec<EnrichedJob>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnrichedJob {
    pub id: i64,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub seniority: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub remote_mode: Option<String>,
    #[serde(default)]
    pub remote_regions: Vec<String>,
}

pub const CV_PROFILE: Prompt = Prompt {
    task: "cv_profile",
    version: 1,
    system: include_str!("../prompts/cv_profile.v1.md"),
};

/// The natural-language job search (the UI's Search tab). Its answer is parsed straight into
/// [`career_core::search::JobQuery`] and then sanitized, since a model can produce
/// well-formed JSON with values this app doesn't have.
pub const SEARCH_QUERY: Prompt = Prompt {
    task: "search_query",
    version: 1,
    system: include_str!("../prompts/search_query.v1.md"),
};

/// The answer to `CV_PROFILE`. Everything is optional: a model may leave out what the CV
/// doesn't say.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct CvProfile {
    pub titles: Vec<String>,
    pub seniority: Option<String>,
    pub years_experience: Option<f64>,
    pub skills: Vec<CvSkill>,
    pub industries: Vec<String>,
    pub locations: Vec<String>,
    pub remote: Option<String>,
    pub relocate: bool,
    pub salary_expectation_usd: Option<f64>,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CvSkill {
    pub name: String,
    #[serde(default)]
    pub weight: Option<f64>,
}
