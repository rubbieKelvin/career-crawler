use tracing_subscriber::EnvFilter;

/// Initialise `tracing` output. Level comes from `RUST_LOG`, defaulting to `info`.
pub fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
