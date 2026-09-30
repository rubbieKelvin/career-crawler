use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// Looked up in the working directory when no `--config` is given.
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Shared config for both binaries. Every field has a default, so the file is optional
/// and may be partial. Unknown keys are rejected to catch typos.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub db_path: PathBuf,
    pub seeds_path: PathBuf,
    pub crawler: CrawlerConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlerConfig {
    /// Sent on every request. The part before the first `/` is also the robots.txt agent token.
    pub user_agent: String,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    /// Cap on both the compressed (wire) and decompressed body size.
    pub max_body_bytes: u64,
    pub max_redirects: u8,
    /// Minimum gap between requests to the same host. A robots.txt `Crawl-delay` can raise it.
    pub per_host_delay_ms: u64,
    pub robots_ttl_secs: u64,
    /// Pages fetched concurrently (always at most one per host).
    pub max_concurrency: usize,
    /// Fetches per registrable domain before its remaining URLs are skipped.
    pub max_pages_per_domain: u32,
    /// Link hops from a seed; deeper links are not enqueued.
    pub max_depth: u32,
    /// Links scoring below this are not enqueued.
    pub min_link_score: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub bind: String,
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        return Self {
            db_path: PathBuf::from("data/career.db"),
            seeds_path: PathBuf::from("seeds.txt"),
            crawler: CrawlerConfig::default(),
            ui: UiConfig::default(),
        };
    }
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        return Self {
            user_agent: "career-crawler/0.1".into(),
            request_timeout_secs: 15,
            connect_timeout_secs: 10,
            max_body_bytes: 5 * 1024 * 1024,
            max_redirects: 5,
            per_host_delay_ms: 1000,
            robots_ttl_secs: 24 * 60 * 60,
            max_concurrency: 16,
            max_pages_per_domain: 20,
            max_depth: 5,
            min_link_score: 1.0,
        };
    }
}

impl CrawlerConfig {
    /// The robots.txt product token: the user agent up to the first `/` or space.
    pub fn robots_agent(&self) -> &str {
        return self
            .user_agent
            .split(['/', ' '])
            .next()
            .unwrap_or(&self.user_agent);
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        return Self {
            bind: "127.0.0.1".into(),
            port: 7878,
        };
    }
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        return Ok(toml::from_str(text)?);
    }

    /// Loads `path` if given (it must exist); otherwise `config.toml` if present; otherwise defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => {
                let default = PathBuf::from(DEFAULT_CONFIG_PATH);
                if !default.exists() {
                    return Ok(Self::default());
                }
                default
            }
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        return Self::from_toml(&text)
            .with_context(|| format!("parsing config {}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg = Config::from_toml("[ui]\nport = 9000\n").unwrap();
        assert_eq!(cfg.ui.port, 9000);
        assert_eq!(cfg.ui.bind, "127.0.0.1");
        assert_eq!(cfg.db_path, PathBuf::from("data/career.db"));
    }

    #[test]
    fn robots_agent_is_product_token() {
        let mut c = CrawlerConfig::default();
        assert_eq!(c.robots_agent(), "career-crawler");
        c.user_agent = "MyBot (+https://x.y)".into();
        assert_eq!(c.robots_agent(), "MyBot");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml("db_pth = \"x.db\"").is_err());
    }
}
