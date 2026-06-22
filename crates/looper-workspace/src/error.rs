//! Error type for workspace operations.

use std::path::PathBuf;

/// Errors that occur while managing workspaces and their `.looper` links.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// Persistence (load/save) failed.
    #[error("workspace state error: {0}")]
    State(#[from] looper_state::StateError),

    /// A `.looper` directory link could not be created.
    #[error("failed to create .looper link {link} -> {target}: {source}")]
    Link {
        /// The link path.
        link: PathBuf,
        /// The intended target (KB dir).
        target: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A `.looper` link could not be removed.
    #[error("failed to remove .looper link {link}: {source}")]
    Unlink {
        /// The link path.
        link: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A non-link entry already exists where the `.looper` link should go; we refuse to
    /// clobber a real file/directory.
    #[error("a non-link entry already exists at {0}; refusing to overwrite it")]
    LinkOccupied(PathBuf),

    /// A code repo's `.gitignore` could not be updated for a managed link (plan item 49).
    #[error("failed to update .gitignore at {path}: {source}")]
    Gitignore {
        /// The `.gitignore` path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A folder path supplied by the user does not resolve to a directory.
    #[error("not a folder: {0}")]
    NotADirectory(PathBuf),

    /// Candidate folder discovery failed.
    #[error("failed to inspect workspace folders under {path}: {source}")]
    FolderDiscovery {
        /// The parent folder being inspected.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// No workspace with the given id exists.
    #[error("workspace not found: {0}")]
    NotFound(String),
}
