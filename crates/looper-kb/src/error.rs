//! Error type for knowledge-base backends.

use std::path::PathBuf;

/// Errors that a [`crate::Kb`] backend may return. Backends map their internal errors
/// into these so `looper-core` can stay backend-agnostic.
#[derive(Debug, thiserror::Error)]
pub enum KbError {
    /// An I/O error touching a specific path.
    #[error("knowledge-base I/O error at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A backend-specific failure (parse, persistence, etc.).
    #[error("knowledge-base backend error: {0}")]
    Backend(String),
}
