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
    pub ui: UiConfig,
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
            ui: UiConfig::default(),
        };
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
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml("db_pth = \"x.db\"").is_err());
    }
}
