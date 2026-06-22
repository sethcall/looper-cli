//! `looper-config` — layered configuration for Looper.
//!
//! Resolves a typed [`Config`] from, in increasing precedence: built-in defaults, a
//! JSON property file, `LOOPER_`-prefixed environment variables (nested with `__`,
//! e.g. `LOOPER_LOG__LEVEL`), then CLI overrides. Also resolves platform directories
//! ([`Dirs`]).
//!
//! Dependency rule: **leaf** crate (external deps only). See `../../AGENTS.md`.
//! Implements plan item 04 (`../../specs/plan/04-config-layered.md`).

mod dirs;
mod error;

use std::path::PathBuf;

use clap::Parser;
use figment::{
    providers::{Env, Format, Json, Serialized},
    Figment,
};
use serde::{Deserialize, Serialize};

pub use dirs::{cli_config_file, Dirs};
pub use error::ConfigError;

/// The fully-resolved Looper configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Logging configuration.
    pub log: LogConfig,
}

impl Config {
    /// The effective log directory: the configured override, else the platform
    /// default from `dirs`.
    #[must_use]
    pub fn log_dir(&self, dirs: &Dirs) -> PathBuf {
        self.log.dir.clone().unwrap_or_else(|| dirs.log.clone())
    }
}

/// Logging-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Level filter: `trace` | `debug` | `info` | `warn` | `error`.
    pub level: String,
    /// Override for the log directory. When unset, the platform log dir is used.
    pub dir: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            dir: None,
        }
    }
}

/// Looper command-line arguments that override configuration (highest precedence).
#[derive(Debug, Default, Parser)]
#[command(
    name = "looper",
    version,
    about = "Looper — local-first knowledge desktop app"
)]
pub struct Cli {
    /// Use a specific JSON config file instead of the default location.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Log level filter (trace|debug|info|warn|error).
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// Directory for log files.
    #[arg(long, value_name = "DIR")]
    pub log_dir: Option<PathBuf>,
}

/// Load configuration, layering sources by precedence (lowest first): built-in
/// defaults, the JSON config file, `LOOPER_`-prefixed environment variables, then
/// CLI overrides.
///
/// A missing config file is not an error — defaults (and any higher-precedence
/// sources) still apply.
///
/// # Errors
/// Returns [`ConfigError::Load`] if a source cannot be parsed or the merged result
/// does not deserialize into [`Config`].
pub fn load(cli: &Cli, dirs: &Dirs) -> Result<Config, ConfigError> {
    let config_file = cli.config.clone().unwrap_or_else(|| dirs.config_file());

    let mut figment = Figment::from(Serialized::defaults(Config::default()))
        .merge(Json::file(&config_file))
        .merge(Env::prefixed("LOOPER_").split("__"));

    // CLI overrides (highest precedence) — only the values that were actually passed.
    if let Some(level) = &cli.log_level {
        figment = figment.merge(Serialized::default("log.level", level));
    }
    if let Some(dir) = &cli.log_dir {
        figment = figment.merge(Serialized::default("log.dir", dir));
    }

    figment.extract().map_err(ConfigError::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::result_large_err)] // figment Jail closures return figment::Result<()>

    use super::*;
    use figment::Jail;

    fn test_dirs(base: &std::path::Path) -> Dirs {
        Dirs {
            config: base.to_path_buf(),
            data: base.join("data"),
            log: base.join("logs"),
            cache: base.join("cache"),
        }
    }

    #[test]
    fn defaults_apply_when_no_sources_present() {
        Jail::expect_with(|jail| {
            let dirs = test_dirs(jail.directory());
            let cfg = load(&Cli::default(), &dirs).expect("load");
            assert_eq!(cfg.log.level, "info");
            assert_eq!(cfg.log.dir, None);
            assert_eq!(cfg.log_dir(&dirs), dirs.log);
            Ok(())
        });
    }

    #[test]
    fn file_overrides_defaults() {
        Jail::expect_with(|jail| {
            jail.create_file("config.json", r#"{"log":{"level":"warn"}}"#)?;
            let dirs = test_dirs(jail.directory());
            let cfg = load(&Cli::default(), &dirs).expect("load");
            assert_eq!(cfg.log.level, "warn");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        Jail::expect_with(|jail| {
            jail.create_file("config.json", r#"{"log":{"level":"warn"}}"#)?;
            jail.set_env("LOOPER_LOG__LEVEL", "debug");
            let dirs = test_dirs(jail.directory());
            let cfg = load(&Cli::default(), &dirs).expect("load");
            assert_eq!(cfg.log.level, "debug");
            Ok(())
        });
    }

    #[test]
    fn cli_overrides_env_and_file() {
        Jail::expect_with(|jail| {
            jail.create_file("config.json", r#"{"log":{"level":"warn"}}"#)?;
            jail.set_env("LOOPER_LOG__LEVEL", "debug");
            let dirs = test_dirs(jail.directory());
            let cli = Cli {
                log_level: Some("trace".to_string()),
                ..Cli::default()
            };
            let cfg = load(&cli, &dirs).expect("load");
            assert_eq!(cfg.log.level, "trace");
            Ok(())
        });
    }
}
