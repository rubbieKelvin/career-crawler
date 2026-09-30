mod api;
mod live;
mod queries;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use career_core::{config::Config, db};
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
}

pub fn router(state: AppState) -> Router {
    return Router::new()
        .route("/", get(api::index))
        .route("/ws", get(live::ws))
        .route("/api/stats", get(api::stats))
        .route("/api/graph", get(api::graph))
        .route("/api/events", get(api::events))
        .route("/api/metrics", get(api::metrics))
        .route("/api/metrics/history", get(api::metrics_history))
        .route("/api/domains/{host}", get(api::domain))
        .route("/api/control/{command}", post(api::control))
        .with_state(state);
}

/// Opens the UI's state and starts the tailer that feeds live updates.
pub fn start(pool: SqlitePool, tail_interval: Duration) -> AppState {
    let (live, _) = broadcast::channel(LIVE_BUFFER);
    tokio::spawn(live::tail(pool.clone(), live.clone(), tail_interval));
    return AppState { pool, live };
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
    let app = router(start(pool, TAIL_INTERVAL));

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
