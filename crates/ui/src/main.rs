use std::path::PathBuf;

use career_core::{config::Config, db};
use clap::Parser;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    career_core::logging::init();
    let args = Args::parse();

    let mut config = Config::load(args.config.as_deref())?;
    if let Some(db) = args.db {
        config.db_path = db;
    }

    let pool = db::open(&config.db_path).await?;
    let stats = db::stats(&pool).await?;
    tracing::info!(db = %config.db_path.display(), ?stats, "database ready");
    tracing::info!("web server arrives in milestone 7");
    pool.close().await;
    Ok(())
}
