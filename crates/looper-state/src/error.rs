//! Error type for state persistence.

use std::path::PathBuf;

/// Errors that occur while loading or saving persisted state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The state file could not be read.
    #[error("failed to read state file {path}: {source}")]
    Read {
        /// The file being read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The state file (or its directory / temp file) could not be written.
    #[error("failed to write state at {path}: {source}")]
    Write {
        /// The path being written.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The temporary file could not be atomically renamed over the target.
    #[error("failed to atomically persist state file {path}: {source}")]
    Persist {
        /// The target path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The state file path has no parent directory.
    #[error("state file path {path} has no parent directory")]
    InvalidPath {
        /// The offending path.
        path: PathBuf,
    },

    /// The state file contents could not be parsed.
    #[error("failed to parse state file {path}: {source}")]
    Parse {
        /// The file being parsed.
        path: PathBuf,
        /// The underlying serde error.
        source: serde_json::Error,
    },

    /// The state value could not be serialized.
    #[error("failed to serialize state: {0}")]
    Serialize(serde_json::Error),

    /// The on-disk schema version is newer than this build supports.
    #[error(
        "state schema version {found} is newer than supported version {current}; upgrade Looper"
    )]
    FutureVersion {
        /// The version found on disk.
        found: u32,
        /// The current supported version.
        current: u32,
    },

    /// No migration is available from the on-disk schema version.
    #[error("no migration available from schema version {found} (current {current})")]
    UnsupportedVersion {
        /// The version found on disk.
        found: u32,
        /// The current supported version.
        current: u32,
    },
}
