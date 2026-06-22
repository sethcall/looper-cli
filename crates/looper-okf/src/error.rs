//! Error type for the OKF producer.

use std::path::PathBuf;

/// Errors from the OKF knowledge-base producer.
#[derive(Debug, thiserror::Error)]
pub enum OkfError {
    /// An I/O error touching a path (reading source, writing a bundle file).
    #[error("OKF I/O error at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Persisting/loading the sidecar index failed.
    #[error("OKF state error: {0}")]
    State(#[from] looper_state::StateError),

    /// Serializing frontmatter to YAML failed.
    #[error("OKF YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    /// Serializing the viz graph to JSON failed.
    #[error("OKF JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The bundle directory to visualize does not exist.
    #[error("OKF bundle directory not found: {path}")]
    BundleNotFound {
        /// The missing bundle path.
        path: PathBuf,
    },
}
