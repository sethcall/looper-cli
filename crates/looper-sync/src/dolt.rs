//! The Dolt backend — designed but **not implemented** (plan item 30.10). Every operation
//! returns [`SyncError::Unsupported`] so selecting Dolt fails cleanly instead of pretending
//! to work. The clean trait boundary lets a real implementation land later without touching
//! `looper-core`/`looper-app`.

use std::path::Path;

use looper_ipc::{SyncFolderConfig, SyncResolution, SyncStrategy};

use crate::{SyncBackend, SyncError, SyncOutcome, SyncProbe};

/// Placeholder Dolt backend. See `specs/plan/30-git-sync/10-dolt-backend-research-stub.md`.
pub struct DoltBackend;

impl SyncBackend for DoltBackend {
    fn strategy(&self) -> SyncStrategy {
        SyncStrategy::Dolt
    }

    fn probe(&self, _folder: &Path, _cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError> {
        Err(SyncError::Unsupported(SyncStrategy::Dolt))
    }

    fn sync(&self, _folder: &Path, _cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError> {
        Err(SyncError::Unsupported(SyncStrategy::Dolt))
    }

    fn resolve(
        &self,
        _folder: &Path,
        _cfg: &SyncFolderConfig,
        _how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError> {
        Err(SyncError::Unsupported(SyncStrategy::Dolt))
    }
}
