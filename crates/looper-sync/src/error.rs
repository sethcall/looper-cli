//! Errors surfaced by sync backends.

use std::path::PathBuf;

use looper_ipc::SyncStrategy;

/// A failure during a sync probe, cycle, or conflict resolution.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The folder isn't a git repository (Looper never `git init`s it).
    #[error("{0} is not a git repository")]
    NotARepo(PathBuf),
    /// No git remote / upstream is configured for the sync branch.
    #[error("no git remote configured for {folder}")]
    NoRemote { folder: PathBuf },
    /// HEAD is on a different branch than the one configured for sync.
    #[error("folder is on branch {actual}, expected {expected}")]
    NotOnSyncBranch { expected: String, actual: String },
    /// A `git` subprocess exited non-zero.
    #[error("git {args} failed (exit {code:?}): {stderr}")]
    GitCli {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    /// Authentication failed (classified from git stderr).
    #[error("git authentication failed for {folder}: {stderr}")]
    Auth { folder: PathBuf, stderr: String },
    /// A `git` subprocess exceeded its timeout.
    #[error("git operation timed out: {args}")]
    Timeout { args: String },
    /// The requested backend has no implementation yet (e.g. Dolt today).
    #[error("the {0:?} sync backend is not implemented yet")]
    Unsupported(SyncStrategy),
    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
