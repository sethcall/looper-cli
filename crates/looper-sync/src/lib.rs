//! `looper-sync` — pluggable auto-sync backends behind a swappable seam (plan item 30).
//!
//! [`Syncer`] is the engine-facing trait (`looper-core` holds `Arc<dyn Syncer>`; `looper-app`
//! injects [`DefaultSyncer`]; tests inject [`MockSyncer`]) — mirroring the KB `Kb`/`KbProvider`
//! seam. [`SyncBackend`] is one strategy (GitFileshare, Dolt). Backends are **stateless across
//! folders** — all sync state lives in the folder (its repo) — and take just a folder path + the
//! per-folder [`SyncFolderConfig`] as input.
//!
//! Dependency rule: depends on `looper-ipc` (the shared DTOs) and, once the GitFileshare backend
//! lands (item 30.04), `looper-git` for read-only probing. Must NOT depend on
//! `looper-core`/`looper-app`. See `../../AGENTS.md`.

mod dolt;
mod error;
mod git_fileshare;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use looper_ipc::{ConflictFile, SyncFolderConfig, SyncResolution, SyncStrategy};

pub use dolt::DoltBackend;
pub use error::SyncError;
pub use git_fileshare::{detect_git, git_test, resolve_git, GitFileshareBackend, GitLocator};

/// A read-only probe of a folder for its strategy (drives "can I enable Git here?" + status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncProbe {
    /// The backend can manage this folder (Git: it's a repository).
    pub manageable: bool,
    /// A remote / upstream is configured.
    pub has_remote: bool,
    /// The current branch (None if detached / unborn).
    pub branch: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// An unresolved in-progress merge is present.
    pub conflicted: bool,
    /// A human note (e.g. "not a git repository").
    pub detail: Option<String>,
}

/// The result of one sync cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing to do — already in sync.
    UpToDate,
    /// Something changed this cycle.
    Updated {
        /// Remote changes were integrated locally.
        pulled: bool,
        /// Local changes were pushed to the remote.
        pushed: bool,
    },
    /// A merge conflict — the folder is now paused awaiting resolution.
    Conflict {
        /// The conflicted files.
        files: Vec<ConflictFile>,
    },
}

/// One concrete sync strategy (e.g. GitFileshare, Dolt). Stateless across folders.
pub trait SyncBackend: Send + Sync {
    /// The strategy this backend implements.
    fn strategy(&self) -> SyncStrategy;
    /// Inspect a folder without mutating it.
    ///
    /// # Errors
    /// Returns [`SyncError`] if the probe itself fails (not for "not manageable", which is a
    /// successful probe with `manageable == false`).
    fn probe(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError>;
    /// Run one sync cycle.
    ///
    /// # Errors
    /// Returns [`SyncError`] on a non-conflict failure (a conflict is [`SyncOutcome::Conflict`]).
    fn sync(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError>;
    /// Resolve a paused conflict.
    ///
    /// # Errors
    /// Returns [`SyncError`] if the resolution cannot be completed.
    fn resolve(
        &self,
        folder: &Path,
        cfg: &SyncFolderConfig,
        how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError>;
}

/// The engine-facing seam. `looper-core` holds `Arc<dyn Syncer>` and dispatches per folder by
/// `cfg.strategy`; `looper-app` injects [`DefaultSyncer`]; tests inject [`MockSyncer`].
pub trait Syncer: Send + Sync {
    /// A short identifier (for logging).
    fn name(&self) -> &str;
    /// Inspect a folder.
    ///
    /// # Errors
    /// Propagates the backend's [`SyncError`].
    fn probe(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError>;
    /// Run one sync cycle.
    ///
    /// # Errors
    /// Propagates the backend's [`SyncError`].
    fn sync(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError>;
    /// Resolve a paused conflict.
    ///
    /// # Errors
    /// Propagates the backend's [`SyncError`].
    fn resolve(
        &self,
        folder: &Path,
        cfg: &SyncFolderConfig,
        how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError>;
}

/// The production syncer: a registry of backends, dispatched by `cfg.strategy`.
pub struct DefaultSyncer {
    backends: HashMap<SyncStrategy, Box<dyn SyncBackend>>,
}

impl DefaultSyncer {
    /// Build the default backend registry. GitFileshare is registered when its backend lands
    /// (plan item 30.04); Dolt is the [`DoltBackend`] stub (item 30.10).
    #[must_use]
    pub fn new() -> Self {
        Self::with_git(Arc::new(GitLocator::resolved(None)))
    }

    /// Build a registry over a specific (shared, live) git binary locator (plan item 30.11).
    /// The app holds the same `Arc<GitLocator>` so it can change the git binary at runtime.
    #[must_use]
    pub fn with_git(git: Arc<GitLocator>) -> Self {
        let mut backends: HashMap<SyncStrategy, Box<dyn SyncBackend>> = HashMap::new();
        backends.insert(SyncStrategy::Git, Box::new(GitFileshareBackend::new(git)));
        backends.insert(SyncStrategy::Dolt, Box::new(DoltBackend));
        Self { backends }
    }

    fn backend(&self, strategy: SyncStrategy) -> Result<&dyn SyncBackend, SyncError> {
        self.backends
            .get(&strategy)
            .map(|b| b.as_ref())
            .ok_or(SyncError::Unsupported(strategy))
    }
}

impl Default for DefaultSyncer {
    fn default() -> Self {
        Self::new()
    }
}

impl Syncer for DefaultSyncer {
    fn name(&self) -> &str {
        "default"
    }
    fn probe(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError> {
        self.backend(cfg.strategy)?.probe(folder, cfg)
    }
    fn sync(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError> {
        self.backend(cfg.strategy)?.sync(folder, cfg)
    }
    fn resolve(
        &self,
        folder: &Path,
        cfg: &SyncFolderConfig,
        how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError> {
        self.backend(cfg.strategy)?.resolve(folder, cfg, how)
    }
}

/// A scriptable, no-subprocess [`Syncer`] for `looper-core` / `looper-app` tests.
#[derive(Default)]
pub struct MockSyncer {
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    sync_outcomes: std::collections::VecDeque<SyncOutcome>,
    probe_calls: usize,
    sync_calls: usize,
    resolve_calls: usize,
}

impl MockSyncer {
    /// Queue the outcome the next `sync` call returns (FIFO; defaults to `UpToDate` when empty).
    pub fn script_sync(&self, outcome: SyncOutcome) {
        self.lock().sync_outcomes.push_back(outcome);
    }
    /// How many times `probe` has been called.
    #[must_use]
    pub fn probe_calls(&self) -> usize {
        self.lock().probe_calls
    }
    /// How many times `sync` has been called.
    #[must_use]
    pub fn sync_calls(&self) -> usize {
        self.lock().sync_calls
    }
    /// How many times `resolve` has been called.
    #[must_use]
    pub fn resolve_calls(&self) -> usize {
        self.lock().resolve_calls
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Syncer for MockSyncer {
    fn name(&self) -> &str {
        "mock"
    }
    fn probe(&self, _folder: &Path, _cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError> {
        self.lock().probe_calls += 1;
        Ok(SyncProbe {
            manageable: true,
            has_remote: true,
            branch: Some("main".to_string()),
            ahead: 0,
            behind: 0,
            conflicted: false,
            detail: None,
        })
    }
    fn sync(&self, _folder: &Path, _cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError> {
        let mut state = self.lock();
        state.sync_calls += 1;
        Ok(state
            .sync_outcomes
            .pop_front()
            .unwrap_or(SyncOutcome::UpToDate))
    }
    fn resolve(
        &self,
        _folder: &Path,
        _cfg: &SyncFolderConfig,
        _how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError> {
        self.lock().resolve_calls += 1;
        Ok(SyncOutcome::UpToDate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looper_ipc::SyncDirection;

    fn cfg(strategy: SyncStrategy) -> SyncFolderConfig {
        SyncFolderConfig {
            folder: "/tmp/x".to_string(),
            is_kb: false,
            is_git_repo: true,
            enabled: true,
            strategy,
            branch: "main".to_string(),
            direction: SyncDirection::PullPush,
            dolt: None,
        }
    }

    #[test]
    fn default_syncer_reports_dolt_unsupported() {
        let syncer = DefaultSyncer::new();
        let outcome = syncer.sync(Path::new("/tmp/x"), &cfg(SyncStrategy::Dolt));
        assert!(matches!(
            outcome,
            Err(SyncError::Unsupported(SyncStrategy::Dolt))
        ));
    }

    #[test]
    fn git_backend_is_registered() {
        // GitFileshare is registered (item 30.04): a non-repo path reaches the backend and
        // reports NotARepo, not Unsupported.
        let syncer = DefaultSyncer::new();
        let outcome = syncer.sync(
            Path::new("/looper-nonexistent-not-a-repo"),
            &cfg(SyncStrategy::Git),
        );
        assert!(matches!(outcome, Err(SyncError::NotARepo(_))));
    }

    #[test]
    fn mock_syncer_scripts_outcomes_and_counts_calls() {
        let mock = MockSyncer::default();
        mock.script_sync(SyncOutcome::Updated {
            pulled: true,
            pushed: false,
        });
        let c = cfg(SyncStrategy::Git);

        assert_eq!(
            mock.sync(Path::new("/tmp/x"), &c).unwrap(),
            SyncOutcome::Updated {
                pulled: true,
                pushed: false
            }
        );
        // Defaults to UpToDate once the script is drained.
        assert_eq!(
            mock.sync(Path::new("/tmp/x"), &c).unwrap(),
            SyncOutcome::UpToDate
        );
        assert_eq!(mock.sync_calls(), 2);

        mock.probe(Path::new("/tmp/x"), &c).unwrap();
        mock.resolve(Path::new("/tmp/x"), &c, SyncResolution::UseMine)
            .unwrap();
        assert_eq!(mock.probe_calls(), 1);
        assert_eq!(mock.resolve_calls(), 1);
    }
}
