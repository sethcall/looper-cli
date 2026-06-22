//! Error type for the scan engine.

/// Errors that occur while scanning or persisting the scan cursor.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// Persisting/loading the scan cursor failed.
    #[error("scan state error: {0}")]
    State(#[from] looper_state::StateError),
}
