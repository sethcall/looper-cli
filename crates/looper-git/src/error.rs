//! Error type for git operations.

use std::path::PathBuf;

/// Errors that occur while opening or inspecting a git repository.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The repository could not be opened.
    #[error("failed to open git repository at {path}: {source}")]
    Open {
        /// The path that failed to open.
        path: PathBuf,
        /// The underlying gix error (boxed — gix errors are large).
        source: Box<gix::open::Error>,
    },

    /// HEAD could not be read.
    #[error("failed to read HEAD for {path}: {source}")]
    Head {
        /// The repository path.
        path: PathBuf,
        /// The underlying error (boxed).
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
