//! Platform directory resolution (config / data / log / cache).

use std::path::PathBuf;

use directories::ProjectDirs;

use crate::ConfigError;

/// Resolved platform directories for Looper.
///
/// Paths are *computed*, not created — consumers create the directories they need
/// (e.g. `looper-observability` creates the log directory before writing to it).
#[derive(Debug, Clone)]
pub struct Dirs {
    /// Directory holding the JSON config file.
    pub config: PathBuf,
    /// Directory for durable application data (workspaces, KB sidecar index).
    pub data: PathBuf,
    /// Directory for log files (defaults to `data/logs`).
    pub log: PathBuf,
    /// Directory for disposable caches.
    pub cache: PathBuf,
}

impl Dirs {
    /// Resolve platform directories for the **desktop app** (`com.looper.Looper`).
    ///
    /// # Errors
    /// Returns [`ConfigError::NoProjectDirs`] when no valid home directory exists.
    pub fn resolve() -> Result<Self, ConfigError> {
        Self::from_app("Looper")
    }

    /// Resolve platform directories for **`looper-cli`** (`com.looper.LooperCli`).
    ///
    /// The CLI gets its own data/log/cache dirs so it never clobbers the desktop app's state. Its
    /// config *file*, however, falls back to the shared desktop config — see [`cli_config_file`].
    ///
    /// # Errors
    /// Returns [`ConfigError::NoProjectDirs`] when no valid home directory exists.
    pub fn resolve_cli() -> Result<Self, ConfigError> {
        Self::from_app("LooperCli")
    }

    fn from_app(application: &str) -> Result<Self, ConfigError> {
        let pd =
            ProjectDirs::from("com", "looper", application).ok_or(ConfigError::NoProjectDirs)?;
        Ok(Self {
            config: pd.config_dir().to_path_buf(),
            data: pd.data_dir().to_path_buf(),
            log: pd.data_dir().join("logs"),
            cache: pd.cache_dir().to_path_buf(),
        })
    }

    /// Path to the JSON config file within the config directory.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.json")
    }
}

/// Resolve the config file `looper-cli` should read, **CLI-first then desktop fallback**:
///
/// 1. the CLI's own `com.looper.LooperCli` config, if it exists;
/// 2. otherwise the shared desktop `com.looper.Looper` config, if it exists;
/// 3. otherwise the CLI's own path (so a first-run write lands in the CLI's dir).
///
/// This lets the CLI and desktop app **share** one workspace config by default, while still allowing
/// the CLI to diverge by writing its own.
///
/// # Errors
/// Returns [`ConfigError::NoProjectDirs`] when no valid home directory exists.
pub fn cli_config_file() -> Result<PathBuf, ConfigError> {
    Ok(choose_config_file(
        Dirs::resolve_cli()?.config_file(),
        Dirs::resolve()?.config_file(),
    ))
}

/// Pick the config file: the CLI's own if present, else the desktop's if present, else the CLI's
/// (for a first-run write). Split out from [`cli_config_file`] so the precedence is unit-testable
/// without depending on real platform directories.
fn choose_config_file(cli: PathBuf, desktop: PathBuf) -> PathBuf {
    if cli.exists() {
        return cli;
    }
    if desktop.exists() {
        return desktop;
    }
    cli
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_config_wins_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("cli.json");
        let desktop = tmp.path().join("desktop.json");
        std::fs::write(&cli, "{}").unwrap();
        std::fs::write(&desktop, "{}").unwrap();
        assert_eq!(choose_config_file(cli.clone(), desktop), cli);
    }

    #[test]
    fn falls_back_to_desktop_when_cli_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("cli.json");
        let desktop = tmp.path().join("desktop.json");
        std::fs::write(&desktop, "{}").unwrap();
        assert_eq!(choose_config_file(cli, desktop.clone()), desktop);
    }

    #[test]
    fn defaults_to_cli_path_when_neither_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("cli.json");
        let desktop = tmp.path().join("desktop.json");
        assert_eq!(choose_config_file(cli.clone(), desktop), cli);
    }
}
