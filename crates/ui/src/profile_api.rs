//! The CV profile API (see `brainstorms/12-cv-profile.md`): upload a CV, read the profile,
//! edit it (edits are stored apart from what the CV said, so a re-upload never wipes them),
//! and list the best-matching jobs. Job matches are recomputed here in the background; the
//! crawler notices the change on its own and re-steers.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use career_core::matching::MatchFilter;
use career_core::profile::{self, Overrides, StoredProfile};
use career_core::{matching, profile::ProfilePlace};
use career_cv::IngestError;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::SqlitePool;

use crate::AppState;
use crate::api::ApiError;

/// The profile fields a user can edit.
const EDITABLE: &[&str] = &[
    "titles",
    "seniority",
    "years_experience",
    "skills",
    "industries",
    "locations",
    "remote",
    "relocate",
    "salary_expectation_usd",
    "languages",
    "must_have",
    "exclude",
    "excluded_companies",
];
const MAX_LIST_ITEMS: usize = 50;
const MAX_ITEM_CHARS: usize = 80;

fn bad_request(message: impl Into<String>) -> Response {
    return (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": message.into()})),
    )
        .into_response();
}

fn profile_body(state: &AppState, stored: Option<&StoredProfile>) -> Value {
    let llm = json!({
        // Where a CV upload goes when the LLM may read one (the UI says so before it happens).
        "cv_sent_to": state.cv_llm.as_ref().and(state.llm_host.clone()),
    });
    return match stored {
        None => json!({"profile": null, "llm": llm}),
        Some(p) => json!({
            "profile": {
                "id": p.id,
                "name": p.name,
                "active": p.active,
                "source": p.source,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
                "matches_stale": p.matches_stale(),
                "extracted": p.extracted,
                "overrides": p.overrides,
                "merged": p.merged(),
            },
            "llm": llm,
        }),
    };
}

/// Recomputes the active profile's job matches without blocking the request.
fn rescore_in_background(pool: SqlitePool) {
    tokio::spawn(async move {
        let result: anyhow::Result<()> = async {
            let Some(stored) = profile::active(&pool).await? else {
                return Ok(());
            };
            let scored = matching::rescore_all(&pool, stored.id, &stored.merged()).await?;
            profile::mark_matched(&pool, stored.id, stored.updated_at).await?;
            tracing::info!(profile = stored.id, scored, "job matches recomputed");
            return Ok(());
        }
        .await;
        if let Err(e) = result {
            tracing::error!(error = %e, "failed to recompute job matches");
        }
    });
}

#[derive(Deserialize)]
pub struct PlaceParams {
    q: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/places?q=lag`: cities and countries to pick a location from.
pub async fn places(
    Query(p): Query<PlaceParams>,
) -> Json<Vec<career_core::enrich::geo::Suggestion>> {
    let limit = p.limit.unwrap_or(8).clamp(1, 20);
    return Json(career_core::enrich::geo::search(
        p.q.as_deref().unwrap_or(""),
        limit,
    ));
}

pub async fn get(State(state): State<AppState>) -> Result<Response, ApiError> {
    let stored = profile::active(&state.pool).await?;
    return Ok(Json(profile_body(&state, stored.as_ref())).into_response());
}

#[derive(Deserialize)]
pub struct UploadParams {
    filename: Option<String>,
}

/// `POST /api/profile/cv?filename=cv.pdf` with the file as the raw request body.
pub async fn upload(
    State(state): State<AppState>,
    Query(p): Query<UploadParams>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let filename = p.filename.unwrap_or_default();
    let result = career_cv::ingest(&state.pool, state.cv_llm.as_deref(), &body, &filename).await;
    match result {
        Ok(done) => {
            tracing::info!(profile = done.profile_id, source = %done.source, reused = done.reused, "CV loaded");
        }
        Err(IngestError::Cv(e)) => return Ok(bad_request(e.to_string())),
        Err(IngestError::Db(e)) => return Err(e.into()),
    }
    rescore_in_background(state.pool.clone());
    let stored = profile::active(&state.pool).await?;
    return Ok(Json(profile_body(&state, stored.as_ref())).into_response());
}

/// Trims, drops empties and duplicates, and caps a list of strings.
fn clean_strings(value: &Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let text: String = item.as_str()?.trim().chars().take(MAX_ITEM_CHARS).collect();
        if !text.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&text)) {
            out.push(text);
        }
    }
    out.truncate(MAX_LIST_ITEMS);
    return Some(out);
}

/// Accepts the forms a UI naturally sends (plain strings for places and skills) and turns
/// them into the stored shapes.
fn normalize(key: &str, value: &Value) -> Result<Value, String> {
    let invalid = || format!("\"{key}\" has the wrong shape");
    return match key {
        "locations" => {
            let names = clean_strings(value).ok_or_else(invalid)?;
            let mut places: Vec<ProfilePlace> = Vec::new();
            for name in &names {
                // The places we can rank by distance are the ones in the offline table, so
                // anything else is refused instead of being stored as dead text.
                if career_core::enrich::geo::geocode(name).is_empty() {
                    return Err(format!(
                        "Couldn't find a place called \"{name}\". Start typing and pick one of the suggestions."
                    ));
                }
                places.push(career_cv::parse::place_from_text(name));
            }
            Ok(json!(places))
        }
        "skills" => {
            let items = value.as_array().ok_or_else(invalid)?;
            let skills: Vec<Value> = items
                .iter()
                .filter_map(|i| match i {
                    Value::String(s) if !s.trim().is_empty() => {
                        Some(json!({"name": s.trim().to_lowercase(), "weight": 0.6}))
                    }
                    Value::Object(o) => {
                        let name = o.get("name")?.as_str()?.trim().to_lowercase();
                        let weight = o.get("weight").and_then(Value::as_f64).unwrap_or(0.6).clamp(0.1, 1.0);
                        (!name.is_empty()).then(|| json!({"name": name, "weight": weight}))
                    }
                    _ => None,
                })
                .take(MAX_LIST_ITEMS)
                .collect();
            Ok(json!(skills))
        }
        "titles" | "industries" | "languages" | "must_have" | "exclude" | "excluded_companies" => {
            Ok(json!(clean_strings(value).ok_or_else(invalid)?))
        }
        "seniority" => match career_core::enrich::one_of(value.as_str(), career_core::enrich::SENIORITIES) {
            Some(s) => Ok(json!(s)),
            None => Err("\"seniority\" must be one of intern, junior, mid, senior, lead, manager, director, executive".into()),
        },
        "remote" => match career_core::enrich::one_of(value.as_str(), career_core::enrich::REMOTE_MODES) {
            Some(s) => Ok(json!(s)),
            None => Err("\"remote\" must be onsite, hybrid or remote".into()),
        },
        "relocate" => value.as_bool().map(|b| json!(b)).ok_or_else(invalid),
        "years_experience" | "salary_expectation_usd" => match value.as_f64() {
            Some(n) if n.is_finite() && n >= 0.0 => Ok(json!(n)),
            _ => Err(invalid()),
        },
        other => Err(format!("unknown field \"{other}\"")),
    };
}

/// Applies a patch to the stored overrides: a key replaces that field's override, `null`
/// removes it (back to what the CV said).
fn apply_patch(current: &Overrides, patch: &Map<String, Value>) -> Result<Overrides, String> {
    let Value::Object(mut merged) = serde_json::to_value(current).map_err(|e| e.to_string())?
    else {
        return Err("overrides are not an object".into());
    };
    for (key, value) in patch {
        if value.is_null() {
            if !EDITABLE.contains(&key.as_str()) {
                return Err(format!("unknown field \"{key}\""));
            }
            merged.remove(key);
            continue;
        }
        merged.insert(key.clone(), normalize(key, value)?);
    }
    return serde_json::from_value(Value::Object(merged)).map_err(|e| e.to_string());
}

pub async fn edit(
    State(state): State<AppState>,
    Json(patch): Json<Map<String, Value>>,
) -> Result<Response, ApiError> {
    let Some(stored) = profile::active(&state.pool).await? else {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "no profile yet: upload a CV first"})),
        )
            .into_response());
    };
    return edit_stored(&state, stored, patch).await;
}

async fn edit_stored(
    state: &AppState,
    stored: StoredProfile,
    patch: Map<String, Value>,
) -> Result<Response, ApiError> {
    let overrides = match apply_patch(&stored.overrides, &patch) {
        Ok(o) => o,
        Err(message) => return Ok(bad_request(message)),
    };
    profile::set_overrides(&state.pool, stored.id, &overrides).await?;
    // Only the active profile is ranked; an inactive one is scored when it is activated.
    if stored.active {
        rescore_in_background(state.pool.clone());
    }
    let fresh = profile::get(&state.pool, stored.id).await?;
    return Ok(Json(profile_body(state, fresh.as_ref())).into_response());
}

fn not_found() -> Response {
    return (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "no such profile"})),
    )
        .into_response();
}

/// A profile for the list: enough to tell them apart and see their state, not the lot.
async fn summary(pool: &SqlitePool, p: &StoredProfile) -> anyhow::Result<Value> {
    let match_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM job_matches WHERE profile_id = ?")
            .bind(p.id)
            .fetch_one(pool)
            .await?;
    let edited = serde_json::to_value(&p.overrides)?
        .as_object()
        .map_or(0, |o| o.values().filter(|v| !v.is_null()).count());
    let merged = p.merged();
    return Ok(json!({
        "id": p.id,
        "name": p.name,
        "active": p.active,
        "source": p.source,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "matches_stale": p.matches_stale(),
        "match_count": match_count,
        "edited_fields": edited,
        "titles": merged.titles.iter().take(3).collect::<Vec<_>>(),
        "seniority": merged.seniority,
        "skill_count": merged.skills.len(),
    }));
}

/// `GET /api/profiles`: every profile, the active one first.
pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let mut rows = Vec::new();
    for p in profile::list(&state.pool).await? {
        rows.push(summary(&state.pool, &p).await?);
    }
    return Ok(Json(json!({
        "profiles": rows,
        "llm": {"cv_sent_to": state.cv_llm.as_ref().and(state.llm_host.clone())},
    }))
    .into_response());
}

pub async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    return Ok(match profile::get(&state.pool, id).await? {
        Some(p) => Json(profile_body(&state, Some(&p))).into_response(),
        None => not_found(),
    });
}

pub async fn edit_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<Map<String, Value>>,
) -> Result<Response, ApiError> {
    return match profile::get(&state.pool, id).await? {
        Some(stored) => edit_stored(&state, stored, patch).await,
        None => Ok(not_found()),
    };
}

#[derive(Deserialize)]
pub struct Rename {
    name: String,
}

const MAX_NAME_CHARS: usize = 80;

/// `PATCH /api/profiles/{id}` with `{"name": "…"}`.
pub async fn rename(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Rename>,
) -> Result<Response, ApiError> {
    let name: String = body.name.trim().chars().take(MAX_NAME_CHARS).collect();
    if name.is_empty() {
        return Ok(bad_request("a profile needs a name"));
    }
    if !profile::rename(&state.pool, id, &name).await? {
        return Ok(not_found());
    }
    let fresh = profile::get(&state.pool, id).await?;
    return Ok(Json(profile_body(&state, fresh.as_ref())).into_response());
}

/// `POST /api/profiles/{id}/activate`: rank jobs and steer the crawl by this profile.
pub async fn activate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if profile::get(&state.pool, id).await?.is_none() {
        return Ok(not_found());
    }
    profile::activate(&state.pool, id).await?;
    rescore_in_background(state.pool.clone());
    let fresh = profile::get(&state.pool, id).await?;
    return Ok(Json(profile_body(&state, fresh.as_ref())).into_response());
}

/// `POST /api/profiles/{id}/deactivate`: stop using it, keep it stored.
pub async fn deactivate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if profile::get(&state.pool, id).await?.is_none() {
        return Ok(not_found());
    }
    sqlx::query("UPDATE profiles SET active = 0 WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    let fresh = profile::get(&state.pool, id).await?;
    return Ok(Json(profile_body(&state, fresh.as_ref())).into_response());
}

/// `DELETE /api/profiles/{id}`: removes the profile and its job matches for good.
pub async fn destroy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if !profile::delete(&state.pool, id).await? {
        return Ok(not_found());
    }
    return Ok(Json(json!({"deleted": id})).into_response());
}

/// Stops using the profile (it stays stored). The crawler goes back to neutral scoring.
pub async fn remove(State(state): State<AppState>) -> Result<Response, ApiError> {
    sqlx::query("UPDATE profiles SET active = 0 WHERE active = 1")
        .execute(&state.pool)
        .await?;
    return Ok(Json(profile_body(&state, None)).into_response());
}

pub async fn matches(
    State(state): State<AppState>,
    Query(p): Query<MatchFilter>,
) -> Result<Response, ApiError> {
    let stored = profile::active(&state.pool).await?;
    return matches_for(&state, stored, p).await;
}

pub async fn matches_of(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<MatchFilter>,
) -> Result<Response, ApiError> {
    return match profile::get(&state.pool, id).await? {
        Some(stored) => matches_for(&state, Some(stored), p).await,
        None => Ok(not_found()),
    };
}

async fn matches_for(
    state: &AppState,
    stored: Option<StoredProfile>,
    filter: MatchFilter,
) -> Result<Response, ApiError> {
    let Some(stored) = stored else {
        return Ok(Json(json!({"matches": [], "stale": false, "profile": false})).into_response());
    };
    let (total, top) = matching::browse(&state.pool, stored.id, &filter).await?;
    let rows: Vec<Value> = top
        .into_iter()
        .map(|m| {
            let reasons: Vec<String> = m
                .reasons
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or_default();
            return json!({
                "job_id": m.job_id, "score": m.score, "reasons": reasons, "title": m.title,
                "company": m.company, "domain": m.domain, "location": m.location,
                "remote_mode": m.remote_mode, "url": m.url, "salary_usd_annual": m.salary_usd_annual,
                "seniority": m.seniority, "category": m.category,
                "employment_type": m.employment_type, "posted_at": m.posted_at,
            });
        })
        .collect();
    let facets = matching::facets(&state.pool, stored.id).await?;
    return Ok(Json(json!({
        "matches": rows, "total": total, "facets": facets, "stale": stored.matches_stale() && stored.active, "profile": true,
    }))
    .into_response());
}
