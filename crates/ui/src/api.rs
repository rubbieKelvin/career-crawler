//! REST API (see `brainstorms/05-realtime-visualization.md`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use career_core::control::{self, Command};
use career_core::events;
use career_core::samples;
use career_core::time::now_ms;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::queries;

const DEFAULT_GRAPH_NODES: i64 = 1_000;
const MAX_GRAPH_NODES: i64 = 10_000;
const DEFAULT_EVENTS: i64 = 200;
const MAX_EVENTS: i64 = 2_000;
/// Default history window, and the point count history is downsampled to.
const DEFAULT_HISTORY_MS: i64 = 60 * 60 * 1000;
const HISTORY_POINTS: i64 = 300;

/// Any internal failure: logged, and reported as a 500 with a JSON body.
pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        return ApiError(e.into());
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "request failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": self.0.to_string()})),
        )
            .into_response();
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub async fn index() -> Html<&'static str> {
    return Html(include_str!("index.html"));
}

pub async fn stats(State(state): State<AppState>) -> ApiResult<queries::Stats> {
    return Ok(Json(queries::stats(&state.pool).await?));
}

#[derive(Deserialize)]
pub struct GraphParams {
    limit: Option<i64>,
    /// Include domains that were linked to but never fetched (default true).
    discovered: Option<bool>,
}

pub async fn graph(
    State(state): State<AppState>,
    Query(p): Query<GraphParams>,
) -> ApiResult<queries::Graph> {
    let limit = p
        .limit
        .unwrap_or(DEFAULT_GRAPH_NODES)
        .clamp(1, MAX_GRAPH_NODES);
    return Ok(Json(
        queries::graph(&state.pool, limit, p.discovered.unwrap_or(true)).await?,
    ));
}

#[derive(Deserialize)]
pub struct EventParams {
    /// Events after this id, oldest first. Without it: the latest `limit`.
    after_id: Option<i64>,
    limit: Option<i64>,
}

pub async fn events(
    State(state): State<AppState>,
    Query(p): Query<EventParams>,
) -> ApiResult<Vec<Value>> {
    let limit = p.limit.unwrap_or(DEFAULT_EVENTS).clamp(1, MAX_EVENTS);
    let stored = match p.after_id {
        Some(after) => events::since(&state.pool, after, limit).await?,
        None => events::latest(&state.pool, limit).await?,
    };
    let body = stored
        .into_iter()
        .map(|e| json!({"id": e.id, "ts": e.ts, "event": e.event}))
        .collect();
    return Ok(Json(body));
}

pub async fn metrics(State(state): State<AppState>) -> ApiResult<queries::LatestMetrics> {
    return Ok(Json(queries::latest_metrics(&state.pool).await?));
}

#[derive(Deserialize)]
pub struct HistoryParams {
    from: Option<i64>,
    to: Option<i64>,
    step: Option<i64>,
}

pub async fn metrics_history(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> ApiResult<Vec<samples::Sample>> {
    let to = p.to.unwrap_or_else(now_ms);
    let from = p.from.unwrap_or(to - DEFAULT_HISTORY_MS);
    let step = p.step.unwrap_or((to - from) / HISTORY_POINTS).max(1_000);
    return Ok(Json(samples::history(&state.pool, from, to, step).await?));
}

pub async fn domain(
    State(state): State<AppState>,
    Path(host): Path<String>,
) -> Result<Response, ApiError> {
    return Ok(match queries::domain_detail(&state.pool, &host).await? {
        Some(detail) => Json(detail).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown domain"})),
        )
            .into_response(),
    });
}

pub async fn control(
    State(state): State<AppState>,
    Path(command): Path<String>,
) -> Result<Response, ApiError> {
    let Some(command) = Command::parse(&command) else {
        let body = json!({"error": "unknown command; use pause, resume or stop"});
        return Ok((StatusCode::BAD_REQUEST, Json(body)).into_response());
    };
    let id = control::submit(&state.pool, command).await?;
    // Accepted, not applied: the crawler picks it up on its next poll, if it's running.
    let body = json!({"id": id, "command": command.as_str()});
    return Ok((StatusCode::ACCEPTED, Json(body)).into_response());
}
