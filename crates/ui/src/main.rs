mod api;
mod live;
mod profile_api;
mod queries;
mod search_api;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use career_core::{config::Config, db};
use career_llm::Llm;
use clap::Parser;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

/// How often the UI checks the database for new events and metrics samples.
const TAIL_INTERVAL: Duration = Duration::from_millis(200);
/// Messages buffered per WebSocket client before it's told it lagged.
const LIVE_BUFFER: usize = 4096;

/// Local web UI for the crawler: live graph, history, metrics, job search.
#[derive(Debug, Parser)]
#[command(name = "ui", version)]
struct Args {
    /// Config file (default: ./config.toml if it exists, otherwise built-in defaults).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override `db_path` from the config.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Override `ui.bind` from the config.
    #[arg(long)]
    bind: Option<String>,
    /// Override `ui.port` from the config.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub live: live::Sender,
    /// The LLM, when it is enabled and has a key. The natural-language search reads with it;
    /// `cv_llm` is the same client when a CV may be sent to it.
    pub llm: Option<Arc<Llm>>,
    /// Reads CVs, when the LLM is on and `llm.send_cv` allows it; otherwise the local parser does.
    pub cv_llm: Option<Arc<Llm>>,
    /// The provider text goes to, if it goes anywhere (shown to the user before it is sent).
    pub llm_host: Option<String>,
}

pub fn router(state: AppState) -> Router {
    return Router::new()
        .route("/", get(api::index))
        .route("/graph", get(api::graph_page))
        .route("/search", get(api::search_page))
        .route("/profile", get(api::profile_page))
        .route("/resources", get(api::resources_page))
        .route("/static/{file}", get(api::asset))
        .route("/ws", get(live::ws))
        .route("/api/stats", get(api::stats))
        .route("/api/graph", get(api::graph))
        .route("/api/events", get(api::events))
        .route("/api/metrics", get(api::metrics))
        .route("/api/metrics/history", get(api::metrics_history))
        .route("/api/history", get(api::history))
        .route("/api/domains/{host}", get(api::domain))
        .route("/api/domains/{host}/graph", get(api::domain_graph))
        .route("/api/control/{command}", post(api::control))
        .route(
            "/api/profile",
            get(profile_api::get).delete(profile_api::remove),
        )
        .route(
            "/api/profile/cv",
            post(profile_api::upload)
                .layer(DefaultBodyLimit::max(career_cv::text::MAX_BYTES + 4096)),
        )
        .route("/api/profile/overrides", put(profile_api::edit))
        .route("/api/profile/matches", get(profile_api::matches))
        .route("/api/places", get(profile_api::places))
        .route("/api/profiles", get(profile_api::list))
        .route(
            "/api/profiles/{id}",
            get(profile_api::show)
                .patch(profile_api::rename)
                .delete(profile_api::destroy),
        )
        .route("/api/profiles/{id}/overrides", put(profile_api::edit_one))
        .route("/api/profiles/{id}/activate", post(profile_api::activate))
        .route(
            "/api/profiles/{id}/deactivate",
            post(profile_api::deactivate),
        )
        .route("/api/profiles/{id}/matches", get(profile_api::matches_of))
        .route("/api/search/nl", post(search_api::nl))
        .with_state(state);
}

/// Opens the UI's state and starts the tailer that feeds live updates.
pub fn start(pool: SqlitePool, tail_interval: Duration) -> AppState {
    let (live, _) = broadcast::channel(LIVE_BUFFER);
    tokio::spawn(live::tail(pool.clone(), live.clone(), tail_interval));
    return AppState {
        pool,
        live,
        llm: None,
        cv_llm: None,
        llm_host: None,
    };
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    career_core::logging::init();
    let args = Args::parse();

    let mut config = Config::load(args.config.as_deref())?;
    if let Some(db) = args.db {
        config.db_path = db;
    }
    let bind = args.bind.unwrap_or(config.ui.bind);
    let port = args.port.unwrap_or(config.ui.port);
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address {bind}:{port}"))?;
    if !addr.ip().is_loopback() {
        tracing::warn!(%addr, "binding beyond localhost; the UI has no authentication");
    }

    let pool = db::open(&config.db_path).await?;
    tracing::info!(db = %config.db_path.display(), "database ready");
    let mut state = start(pool.clone(), TAIL_INTERVAL);
    let llm = Llm::from_config(&config.llm, pool)?;
    state.llm_host = llm.as_ref().map(|_| config.llm.base_url.clone());
    if config.llm.send_cv {
        state.cv_llm = llm.clone();
    }
    state.llm = llm;
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("UI at http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    return Ok(());
}
