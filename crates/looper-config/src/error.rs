//! Error type for configuration loading.

/// Errors that occur while resolving platform directories or loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The platform's home/config directories could not be determined.
    #[error("could not determine platform directories (no valid home directory)")]
    NoProjectDirs,

    /// A configuration source (file, env, or CLI) failed to parse or merge.
    ///
    /// The inner `figment::Error` is boxed to keep `ConfigError` — and the many
    /// `Result<_, ConfigError>` returns across the crate — small.
    #[error("failed to load configuration: {0}")]
    Load(Box<figment::Error>),
}

impl From<figment::Error> for ConfigError {
    fn from(err: figment::Error) -> Self {
        Self::Load(Box::new(err))
    }
}
