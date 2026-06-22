//! Error type for observability initialization.

use std::path::PathBuf;

/// Errors that occur while initializing logging & diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// The log directory could not be created.
    #[error("failed to create log directory {path}: {source}")]
    CreateLogDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A `tracing` subscriber was already installed for this process.
    #[error("failed to install the tracing subscriber: {0}")]
    Init(#[from] tracing_subscriber::util::TryInitError),
}
