//! Natural-language job search (milestone 12, `brainstorms/10-llm.md`): one route behind the
//! Search tab. The user's words go to the LLM to be read into a `JobQuery`; the filter chips
//! re-run the same route with the `JobQuery` they already have and no LLM. Either way
//! `areer_core::search` builds the SQL — the model never writes it, and its answer is
//! sanitized before it is used.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use areer_core::profile::{self, Profile};
use areer_core::search::{self, JobQuery};
use areer_llm::tasks::SEARCH_QUERY;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::api::ApiError;

/// The longest search text that is read (and, with the LLM on, sent to the provider).
const MAX_QUERY_CHARS: usize = 500;
/// Words the keyword fallback drops: on their own they carry no signal about a job.
const STOPWORDS: &[&str] = &[
    "a", "am", "an", "and", "any", "are", "around", "as", "at", "be", "find", "for", "from", "get",
    "give", "good", "i", "in", "is", "it", "job", "jobs", "like", "looking", "me", "my", "near",
    "nice", "of", "on", "or", "paid", "paying", "please", "role", "roles", "show", "some", "the",
    "to", "top", "want", "well", "with", "work", "would",
];
const MAX_FALLBACK_KEYWORDS: usize = 8;

#[derive(Deserialize)]
pub struct SearchRequest {
    /// The user's words: the LLM reads them (or the keyword fallback does, without one).
    query: Option<String>,
    /// A filter set the UI already has (a chip removed, the sort changed): used as it is.
    filters: Option<JobQuery>,
}

/// `POST /api/search/nl` with either `{"query": "nice paying jobs in tech around Lagos"}` or
/// `{"filters": {...}}`. The answer echoes the filter that was actually applied — the UI shows
/// it as chips — along with the jobs and what the search had to decide on its own.
pub async fn nl(
    State(state): State<AppState>,
    Json(request): Json<SearchRequest>,
) -> Result<Response, ApiError> {
    let text = request
        .query
        .map(|q| q.trim().chars().take(MAX_QUERY_CHARS).collect::<String>())
        .filter(|q| !q.is_empty());
    let stored = profile::active(&state.pool).await?;

    let (query, source, mut notes) = match (request.filters, text.as_deref()) {
        (Some(filters), _) => {
            let (q, dropped) = filters.sanitized();
            (q, "filters", dropped)
        }
        (None, Some(text)) => {
            let profile = stored.as_ref().map(|p| p.merged());
            read(&state, text, profile.as_ref()).await
        }
        (None, None) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "send a \"query\" or \"filters\""})),
            )
                .into_response());
        }
    };

    let outcome = search::run(&state.pool, &query, stored.as_ref().map(|p| p.id)).await?;
    notes.extend(outcome.notes);
    return Ok(Json(json!({
        "request": text,
        "source": source,
        "query": query,
        "sort": outcome.sort,
        "notes": notes,
        "hits": outcome.hits,
        "llm": {"enabled": state.llm.is_some(), "host": state.llm_host},
    }))
    .into_response());
}

/// The query the user's words describe: what the model made of them, or — with no LLM, or a
/// call that failed — the words themselves as keywords. The source says which it was.
async fn read(
    state: &AppState,
    text: &str,
    profile: Option<&Profile>,
) -> (JobQuery, &'static str, Vec<String>) {
    let Some(llm) = &state.llm else {
        let (query, mut notes) = keyword_query(text).sanitized();
        notes.insert(
            0,
            "no LLM is configured: the words were searched as keywords".into(),
        );
        return (query, "keywords", notes);
    };
    match llm
        .complete_json::<JobQuery>(&SEARCH_QUERY, &digest(text, profile))
        .await
    {
        Ok(done) => {
            let (query, notes) = done.value.sanitized();
            return (query, "llm", notes);
        }
        Err(e) => {
            tracing::warn!(error = %e, "the search text could not be read by the LLM");
            let (query, mut notes) = keyword_query(text).sanitized();
            notes.insert(
                0,
                format!("the LLM couldn't read this ({e}): the words were searched as keywords"),
            );
            return (query, "keywords", notes);
        }
    }
}

/// What the model sees: the words, plus the searcher's profile when there is one, so that
/// "jobs like mine" can be filled in. Never the CV text itself.
fn digest(text: &str, profile: Option<&Profile>) -> String {
    let mut out = format!("query: {text}\n");
    if let Some(p) = profile {
        let list = |items: &[String]| -> String {
            return items.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        };
        let places: Vec<String> = p.locations.iter().map(|l| l.name.clone()).collect();
        out.push_str(&format!(
            "profile: titles: {}; seniority: {}; industries: {}; places: {}; work style: {}\n",
            list(&p.titles),
            p.effective_seniority().unwrap_or("unknown"),
            list(&p.industries),
            list(&places),
            p.remote.as_deref().unwrap_or("no preference"),
        ));
    }
    return out;
}

/// The no-LLM reading: the words that say something about the job, as keywords.
fn keyword_query(text: &str) -> JobQuery {
    let mut keywords: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#') {
        let word = word.trim().to_lowercase();
        if word.len() < 2
            || STOPWORDS.contains(&word.as_str())
            || keywords.contains(&word)
            || keywords.len() >= MAX_FALLBACK_KEYWORDS
        {
            continue;
        }
        keywords.push(word);
    }
    return JobQuery {
        keywords,
        ..JobQuery::default()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_fall_back_to_the_words_that_matter() {
        let query = keyword_query("I'm looking for nice paying jobs in tech around Lagos");
        assert_eq!(query.keywords, ["tech", "lagos"]);
        assert_eq!(keyword_query("a the of").keywords, Vec::<String>::new());
        assert_eq!(
            keyword_query("C++ and C# engineers").keywords,
            ["c++", "c#", "engineers"]
        );
    }

    #[test]
    fn the_digest_carries_the_profile_not_the_cv() {
        let text = digest("jobs like mine", None);
        assert_eq!(text, "query: jobs like mine\n");
        let profile = Profile {
            titles: vec!["Backend Engineer".into()],
            seniority: Some("senior".into()),
            locations: vec![areer_core::profile::ProfilePlace {
                name: "Lagos, NG".into(),
                country_code: Some("NG".into()),
                lat: Some(6.52),
                lon: Some(3.38),
            }],
            ..Profile::default()
        };
        let text = digest("jobs like mine", Some(&profile));
        assert!(text.contains("titles: Backend Engineer"));
        assert!(text.contains("places: Lagos, NG"));
        assert!(text.contains("seniority: senior"));
    }
}
