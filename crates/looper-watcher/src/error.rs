//! Error type for the file watcher.

use std::path::PathBuf;

use notify_debouncer_full::notify;

/// Errors that occur while starting or operating the file watcher.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The watcher backend (notify) could not be initialized.
    #[error("failed to initialize the file watcher: {0}")]
    Backend(#[from] notify::Error),

    /// A specific path could not be watched (e.g. it does not exist, or the OS watch
    /// limit was reached — see [`crate::WatchHealth`]).
    #[error("failed to watch {path}: {source}")]
    Watch {
        /// The path that could not be watched.
        path: PathBuf,
        /// The underlying notify error.
        source: notify::Error,
    },
}
