//! `looper-core` — the Looper orchestration engine.
//!
//! Wires watcher + scan + workspace + git + the `Kb` **trait** into a coherent
//! application service that `looper-app` drives, and emits a typed [`EngineEvent`]
//! stream. It depends on the KB *trait* only — **never** a concrete backend like
//! `looper-okf` (enforced by `Cargo.toml`).
//!
//! ## Model (plan item 14)
//!
//! [`Engine::open_workspace`] spawns one worker thread per workspace that runs the
//! lifecycle **discover repos → catch-up scan → index via `Kb` → watch → re-index**,
//! publishing [`EngineEvent`]s on a [`tokio::sync::broadcast`] bus the app subscribes to.
//! Downstream work (scan, `Kb::index`) is synchronous and IO-bound, so each workspace
//! gets a dedicated OS thread rather than an async task; the broadcast bus is the only
//! async surface (the app `recv().await`s it inside Tauri's runtime). Dropping the last
//! [`Engine`] clone (or calling [`Engine::shutdown`]) stops every worker and releases all
//! OS watches.
//!
//! Dependency rule: highest non-binary crate; may depend on every foundational/domain
//! crate plus `looper-kb` (trait). See `../../AGENTS.md`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use looper_enrichment::Enricher;
use looper_git::{discover_repos, HeadState};
use looper_ipc::{
    EngineChangeKind, EngineEvent, EnrichmentConfig, InternalErrorDraft, InternalErrorSeverity,
    KbBackend, RepoInfo, SyncDirection, SyncFolderConfig, SyncFolderState, SyncResolution,
    SyncStatus,
};
use looper_kb::{DocSummary, Kb, KbError, KbProvider, SearchHit, SourceDoc, TagCount};
use looper_scan::{Change, ChangeKind, ScanConfig, ScanEngine};
use looper_sync::{SyncError, SyncOutcome, Syncer};
use looper_watcher::{WatchConfig, WatchFilter, WatchMessage, Watcher, DEFAULT_DEBOUNCE};
use looper_workspace::{link, LinkState, Workspace};
use tokio::sync::broadcast;

/// Event-bus capacity. Generous so a briefly-stalled subscriber doesn't lag out during a
/// large startup scan.
const BUS_CAPACITY: usize = 1024;

/// Errors surfaced *synchronously* by the engine API. Per-workspace runtime failures are
/// reported asynchronously as [`EngineEvent::Warning`] on the bus instead.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// `open_workspace` was called for a workspace id that is already running.
    #[error("workspace `{0}` is already open")]
    WorkspaceAlreadyOpen(String),

    /// A sync command targeted a workspace that isn't currently open.
    #[error("workspace `{0}` is not open")]
    WorkspaceNotOpen(String),

    /// The KB backend for the workspace could not be opened.
    #[error("KB error: {0}")]
    Kb(#[from] KbError),

    /// An I/O error preparing the workspace (e.g. creating its KB directory).
    #[error("I/O error at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// When Auto Sync fires for an enabled folder (plan item 30; all fields configurable).
#[derive(Debug, Clone)]
pub struct SyncTriggers {
    /// Re-sync each enabled folder on this interval (`None` disables the timer).
    pub periodic: Option<Duration>,
    /// After a watched file changes, push that folder once this debounce elapses (PullPush
    /// only; `None` disables on-change syncing).
    pub on_change_debounce: Option<Duration>,
    /// Sync each enabled folder once when the workspace opens.
    pub on_open: bool,
}

impl Default for SyncTriggers {
    fn default() -> Self {
        Self {
            periodic: Some(Duration::from_secs(60)),
            on_change_debounce: Some(Duration::from_secs(10)),
            on_open: true,
        }
    }
}

/// A message to a workspace's sync thread.
enum SyncJob {
    /// Sync this folder now (on-open, manual, or retry-after-error).
    SyncNow(String),
    /// Resolve a paused conflict for this folder.
    Resolve(String, SyncResolution),
    /// A watched file changed under this folder (debounced into a push).
    Touched(String),
    /// The live config changed; re-read it (and re-emit the snapshot).
    ReloadConfigs,
}

/// A message to a workspace's enrichment thread (plan item 41).
enum EnrichJob {
    /// Manually enrich one bundle doc (the per-doc Enrich button). `path` is the source path
    /// (the UI key for status); `concept_id` targets the bundle `.md`.
    Doc { concept_id: String, path: String },
    /// Enrich every bundle doc — the after-scan hook (only when `enrich_after_scan`).
    AfterScan,
    /// Force-enrich every bundle doc now (the toolbar "Enrich all" button), bypassing the
    /// after-scan gate.
    All,
}

/// The Looper engine. Clone freely — clones share one underlying engine (and one bus).
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    providers: HashMap<KbBackend, Arc<dyn KbProvider>>,
    syncer: Arc<dyn Syncer>,
    enricher: Arc<dyn Enricher>,
    triggers: SyncTriggers,
    events: broadcast::Sender<EngineEvent>,
    workspaces: Mutex<HashMap<String, WorkspaceHandle>>,
}

/// Ingestion pause + backlog for a workspace (item 50). `depth > 0` pauses applying observed file
/// changes to the index; while paused they accumulate in `pending` (deduped by path) and drain on
/// resume. A depth counter, so nested/overlapping pausers compose.
#[derive(Default)]
struct Ingestion {
    depth: AtomicUsize,
    pending: Mutex<BTreeSet<PathBuf>>,
}

struct WorkspaceHandle {
    /// This workspace's KB backend (shared with its worker; used by `search`).
    kb: Arc<dyn Kb>,
    stop: Arc<AtomicBool>,
    /// Force-rescan signal — `rescan()` sets it; the worker reconciles on its next poll (item 47).
    rescan: Arc<AtomicBool>,
    /// Cheap path-set reconcile signal — `reconcile_index()` sets it for a `.looperignore` change
    /// (item 48).
    reconcile: Arc<AtomicBool>,
    /// Rebuild signal — `rebuild_index()` sets it; the worker clears + re-indexes (item 47).
    rebuild: Arc<AtomicBool>,
    /// Queued single-doc removals — `remove_document()` pushes; the worker drains them (item 47).
    removals: Arc<Mutex<Vec<PathBuf>>>,
    /// Ingestion pause depth + paused-change backlog (item 50).
    ingestion: Arc<Ingestion>,
    join: Option<JoinHandle<()>>,
    /// The sync worker's join handle + its job channel + live config/state (plan item 30).
    sync_join: Option<JoinHandle<()>>,
    sync_tx: Sender<SyncJob>,
    sync_cfgs: Arc<Mutex<Vec<SyncFolderConfig>>>,
    sync_state: Arc<Mutex<HashMap<String, SyncFolderState>>>,
    /// The enrichment worker's join handle + job channel + live config (plan item 41).
    enrich_join: Option<JoinHandle<()>>,
    enrich_tx: Sender<EnrichJob>,
    enrich_cfg: Arc<Mutex<EnrichmentConfig>>,
}

impl WorkspaceHandle {
    fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.sync_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.enrich_join.take() {
            let _ = join.join();
        }
    }
}

impl Engine {
    /// Construct an engine over an injected KB *provider* + Auto Sync *syncer* + enrichment
    /// *enricher* (the composition root's seams; each workspace gets its own KB rooted at its
    /// `kb_dir`). Uses the default [`SyncTriggers`].
    #[must_use]
    pub fn new(
        provider: Arc<dyn KbProvider>,
        syncer: Arc<dyn Syncer>,
        enricher: Arc<dyn Enricher>,
    ) -> Self {
        Self::with_triggers(provider, syncer, enricher, SyncTriggers::default())
    }

    /// Like [`Engine::new`] but with explicit Auto Sync triggers (tests use short intervals).
    #[must_use]
    pub fn with_triggers(
        provider: Arc<dyn KbProvider>,
        syncer: Arc<dyn Syncer>,
        enricher: Arc<dyn Enricher>,
        triggers: SyncTriggers,
    ) -> Self {
        let (events, _) = broadcast::channel(BUS_CAPACITY);
        // OKF is the only backend today; the map keeps the engine ready to dispatch others by
        // each workspace's `backend`. The composition root injects the concrete provider.
        let mut providers: HashMap<KbBackend, Arc<dyn KbProvider>> = HashMap::new();
        providers.insert(KbBackend::Okf, provider);
        Self {
            inner: Arc::new(EngineInner {
                providers,
                syncer,
                enricher,
                triggers,
                events,
                workspaces: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Name of the default KB backend's provider (e.g. `"okf"`, `"mock"`).
    #[must_use]
    pub fn kb_name(&self) -> &str {
        self.inner
            .providers
            .get(&KbBackend::Okf)
            .map_or("none", |provider| provider.name())
    }

    /// Subscribe to the engine event stream. The app forwards these to the frontend.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// Open a workspace: spawn its lifecycle worker (discover → scan → index → watch).
    /// Returns immediately; progress and outcomes arrive as [`EngineEvent`]s.
    ///
    /// # Errors
    /// [`CoreError::WorkspaceAlreadyOpen`] if a worker for this id is already running.
    pub fn open_workspace(&self, workspace: Workspace) -> Result<(), CoreError> {
        let id = workspace.id.as_str().to_string();
        let mut workspaces = lock(&self.inner.workspaces);
        if workspaces.contains_key(&id) {
            return Err(CoreError::WorkspaceAlreadyOpen(id));
        }

        // Open this workspace's KB at its `kb_dir` synchronously: it surfaces setup errors
        // to the caller and makes the backend available to `search` before the worker runs.
        std::fs::create_dir_all(&workspace.kb_dir).map_err(|source| CoreError::Io {
            path: workspace.kb_dir.clone(),
            source,
        })?;
        let kb = self
            .inner
            .providers
            .get(&workspace.backend)
            .ok_or_else(|| {
                KbError::Backend(format!(
                    "no KB backend registered for {:?}",
                    workspace.backend
                ))
            })?
            .open(&workspace.kb_dir)?;

        let stop = Arc::new(AtomicBool::new(false));
        // Force-rescan signal (item 47): set by `rescan()`, checked in the worker's poll loop.
        let rescan = Arc::new(AtomicBool::new(false));
        // Cheap path-set reconcile signal (item 48): a `.looperignore` change, no re-hashing.
        let reconcile = Arc::new(AtomicBool::new(false));
        // Rebuild signal (item 47): clear the KB + reset the cursor, then re-index from scratch.
        let rebuild = Arc::new(AtomicBool::new(false));
        // Worker-routed single-doc removals (item 47): `remove_document` pushes paths; the worker
        // evicts each from the KB + forgets its cursor entry so the two stay consistent.
        let removals = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        // Ingestion pause + paused-change backlog (item 50).
        let ingestion = Arc::new(Ingestion::default());
        let kb_dir = workspace.kb_dir.clone();
        // Shared, mutually-exclusive guard so the index thread's KB writes never race the sync
        // thread's git mutations on `kb_dir` (when the KB folder is itself synced, item 30).
        let kb_write_lock = Arc::new(Mutex::new(()));
        // Live, mutable sync config + state shared with the sync thread.
        let sync_cfgs = Arc::new(Mutex::new(workspace.sync.clone()));
        let sync_state = Arc::new(Mutex::new(HashMap::new()));
        let (sync_tx, sync_rx) = std::sync::mpsc::channel::<SyncJob>();
        // Live enrichment config + its worker channel (plan item 41).
        let enrich_cfg = Arc::new(Mutex::new(workspace.enrichment.clone()));
        let (enrich_tx, enrich_rx) = std::sync::mpsc::channel::<EnrichJob>();

        // Index / watch worker (the existing lifecycle) — now also nudges the sync thread on
        // a watched change, and serializes KB writes against sync via `kb_write_lock`.
        let bus = self.inner.events.clone();
        let worker_stop = Arc::clone(&stop);
        let worker_rescan = Arc::clone(&rescan);
        let worker_reconcile = Arc::clone(&reconcile);
        let worker_rebuild = Arc::clone(&rebuild);
        let worker_removals = Arc::clone(&removals);
        let worker_ingestion = Arc::clone(&ingestion);
        let worker_kb = Arc::clone(&kb);
        let worker_ws = workspace.clone();
        let worker_sync_tx = sync_tx.clone();
        let worker_enrich_tx = enrich_tx.clone();
        let worker_kb_lock = Arc::clone(&kb_write_lock);
        let worker_id = id.clone();
        let join = std::thread::Builder::new()
            .name(format!("looper-ws-{id}"))
            .spawn(move || {
                run_guarded(&bus, &worker_id, "index", || {
                    run_workspace(
                        &*worker_kb,
                        &bus,
                        &worker_ws,
                        &worker_stop,
                        &worker_rescan,
                        &worker_reconcile,
                        &worker_rebuild,
                        &worker_removals,
                        &worker_ingestion,
                        &worker_sync_tx,
                        &worker_enrich_tx,
                        &worker_kb_lock,
                    );
                });
            })
            .expect("spawn workspace worker thread");

        // Sync worker (plan item 30).
        let sync_join = std::thread::Builder::new()
            .name(format!("looper-sync-{id}"))
            .spawn({
                let worker = SyncWorker {
                    syncer: Arc::clone(&self.inner.syncer),
                    bus: self.inner.events.clone(),
                    wid: id.clone(),
                    kb_dir: kb_dir.clone(),
                    cfgs: Arc::clone(&sync_cfgs),
                    state: Arc::clone(&sync_state),
                    triggers: self.inner.triggers.clone(),
                    stop: Arc::clone(&stop),
                    rx: sync_rx,
                    kb_lock: Arc::clone(&kb_write_lock),
                };
                move || {
                    let bus = worker.bus.clone();
                    let wid = worker.wid.clone();
                    run_guarded(&bus, &wid, "sync", || run_sync_workspace(worker));
                }
            })
            .expect("spawn workspace sync thread");

        // Enrichment worker (plan item 41) — a third per-workspace thread so a slow Gemini call
        // never stalls indexing or sync.
        let enrich_join = std::thread::Builder::new()
            .name(format!("looper-enrich-{id}"))
            .spawn({
                let worker = EnrichWorker {
                    enricher: Arc::clone(&self.inner.enricher),
                    kb: Arc::clone(&kb),
                    bus: self.inner.events.clone(),
                    wid: id.clone(),
                    kb_dir: kb_dir.clone(),
                    config: Arc::clone(&enrich_cfg),
                    stop: Arc::clone(&stop),
                    rx: enrich_rx,
                };
                move || {
                    let bus = worker.bus.clone();
                    let wid = worker.wid.clone();
                    run_guarded(&bus, &wid, "enrich", || run_enrich_workspace(worker));
                }
            })
            .expect("spawn workspace enrich thread");

        workspaces.insert(
            id,
            WorkspaceHandle {
                kb,
                stop,
                rescan,
                reconcile,
                rebuild,
                removals,
                ingestion,
                join: Some(join),
                sync_join: Some(sync_join),
                sync_tx,
                sync_cfgs,
                sync_state,
                enrich_join: Some(enrich_join),
                enrich_tx,
                enrich_cfg,
            },
        );
        Ok(())
    }

    /// Stop and detach a single workspace's worker (clean watcher teardown).
    pub fn close_workspace(&self, workspace_id: &str) {
        let handle = lock(&self.inner.workspaces).remove(workspace_id);
        if let Some(handle) = handle {
            handle.stop_and_join();
        }
    }

    /// Search a specific open workspace's KB. Returns an empty result if that workspace is
    /// not currently open.
    ///
    /// # Errors
    /// Returns [`KbError`] on backend failure.
    pub fn search(
        &self,
        workspace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, KbError> {
        let kb = lock(&self.inner.workspaces)
            .get(workspace_id)
            .map(|handle| Arc::clone(&handle.kb));
        match kb {
            Some(kb) => kb.search(query, limit),
            None => Ok(Vec::new()),
        }
    }

    /// The number of documents in a workspace's KB index (0 if the workspace isn't open). The
    /// dashboard's "docs indexed" stat — the true index size, not this session's activity.
    #[must_use]
    pub fn doc_count(&self, workspace_id: &str) -> usize {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map_or(0, |handle| handle.kb.doc_count())
    }

    /// The absolute source path of every indexed document in a workspace (empty if not open) — the
    /// Activity carousel groups these by source folder for a per-folder doc count (item 45).
    #[must_use]
    pub fn source_paths(&self, workspace_id: &str) -> Vec<String> {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map_or_else(Vec::new, |handle| handle.kb.source_paths())
    }

    /// Every indexed document in a workspace as a lightweight catalog entry (path + title +
    /// labels) — a whole-KB catalog listing. Empty if the workspace isn't open.
    #[must_use]
    pub fn list_documents(&self, workspace_id: &str) -> Vec<DocSummary> {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map_or_else(Vec::new, |handle| handle.kb.list_documents())
    }

    /// Every tag in a workspace's KB with its document count, from the persisted tag → documents
    /// index (for tag-filter UIs). Empty if the workspace isn't open.
    #[must_use]
    pub fn tag_counts(&self, workspace_id: &str) -> Vec<TagCount> {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map_or_else(Vec::new, |handle| handle.kb.tag_counts())
    }

    /// Read the indexed (bundle) markdown for a source document in an open workspace — producer
    /// metadata + enrichment — or `None` if the workspace isn't open or the doc isn't indexed
    /// (item 51). The viewer falls back to the source file when this is `None`.
    #[must_use]
    pub fn read_indexed_document(&self, workspace_id: &str, source_path: &Path) -> Option<String> {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .and_then(|handle| handle.kb.read_indexed(source_path))
    }

    /// Request a force re-scan of an open workspace: the worker reconciles the index with the tree
    /// on its next poll (≤250 ms) — indexing new/changed docs and **evicting missing or
    /// now-ignored** ones (item 47). No-op if the workspace isn't open.
    pub fn rescan(&self, workspace_id: &str) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            handle.rescan.store(true, Ordering::Release);
        }
    }

    /// Request a **cheap path-set reconcile** of an open workspace (item 48): re-walk for the path
    /// set (no re-hashing) and apply only adds/removes — for a `.looperignore` change, where file
    /// contents are unchanged. No-op if the workspace isn't open.
    pub fn reconcile_index(&self, workspace_id: &str) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            handle.reconcile.store(true, Ordering::Release);
        }
    }

    /// Rebuild a workspace's index from scratch (item 47): on its next poll the worker clears the
    /// KB, resets the scan cursor, and re-indexes every doc. No-op if the workspace isn't open.
    pub fn rebuild_index(&self, workspace_id: &str) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            handle.rebuild.store(true, Ordering::Release);
        }
    }

    /// Remove a single document from an open workspace's index (item 47): the worker evicts it from
    /// the KB **and** forgets its cursor entry, so the two stay consistent. A still-on-disk,
    /// non-ignored doc re-adds on the next full scan — use `.looperignore` to exclude it
    /// permanently. No-op if the workspace isn't open.
    pub fn remove_document(&self, workspace_id: &str, source_path: PathBuf) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            lock(&handle.removals).push(source_path);
        }
    }

    /// Pause applying observed file changes to a workspace's index (item 50). **Nests** — each
    /// `pause_ingestion` must be matched by a `resume_ingestion`. While paused, changes queue
    /// (deduped by path) and apply on resume. No-op if the workspace isn't open.
    pub fn pause_ingestion(&self, workspace_id: &str) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            handle.ingestion.depth.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Resume ingestion (item 50): decrement the pause depth (saturating); the worker drains the
    /// backlog once it reaches zero. No-op if not paused / not open.
    pub fn resume_ingestion(&self, workspace_id: &str) {
        if let Some(handle) = lock(&self.inner.workspaces).get(workspace_id) {
            let _ =
                handle
                    .ingestion
                    .depth
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                        (depth > 0).then(|| depth - 1)
                    });
        }
    }

    /// A workspace's ingestion backlog (item 50): `(pending changes queued behind the pause, is it
    /// paused)`. `(0, false)` if the workspace isn't open.
    #[must_use]
    pub fn ingestion_backlog(&self, workspace_id: &str) -> (usize, bool) {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map_or((0, false), |handle| {
                let paused = handle.ingestion.depth.load(Ordering::Acquire) > 0;
                (lock(&handle.ingestion.pending).len(), paused)
            })
    }

    /// Stop all workspace workers and tear down their watchers.
    pub fn shutdown(&self) {
        let handles: Vec<WorkspaceHandle> = lock(&self.inner.workspaces)
            .drain()
            .map(|(_, h)| h)
            .collect();
        for handle in handles {
            handle.stop_and_join();
        }
    }

    /// Enable/update or disable Auto Sync for one folder (plan item 30): updates the live
    /// config the sync thread reads and triggers an immediate sync when enabling. Returns the
    /// folder's resulting state (the real outcome streams as `Sync*` events).
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't currently open.
    pub fn set_folder_sync(
        &self,
        workspace_id: &str,
        cfg: SyncFolderConfig,
    ) -> Result<SyncFolderState, CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;

        let folder = cfg.folder.clone();
        let is_kb = cfg.is_kb;
        let enabled = cfg.enabled;
        {
            let mut cfgs = lock(&handle.sync_cfgs);
            cfgs.retain(|c| c.folder != folder);
            if enabled {
                cfgs.push(cfg);
            }
        }

        let state = SyncFolderState {
            workspace_id: workspace_id.to_string(),
            folder: folder.clone(),
            is_kb,
            status: if enabled {
                SyncStatus::Syncing
            } else {
                SyncStatus::Off
            },
            last_sync: None,
            last_error: None,
        };
        {
            let mut map = lock(&handle.sync_state);
            if enabled {
                map.insert(folder.clone(), state.clone());
            } else {
                map.remove(&folder);
            }
        }

        let _ = handle.sync_tx.send(SyncJob::ReloadConfigs);
        if enabled {
            let _ = handle.sync_tx.send(SyncJob::SyncNow(folder));
        }
        Ok(state)
    }

    /// Current Auto Sync state for every enabled folder of an open workspace (empty if closed).
    #[must_use]
    pub fn sync_states(&self, workspace_id: &str) -> Vec<SyncFolderState> {
        lock(&self.inner.workspaces)
            .get(workspace_id)
            .map(|handle| lock(&handle.sync_state).values().cloned().collect())
            .unwrap_or_default()
    }

    /// Manually enrich one bundle doc of an open workspace (the per-doc Enrich button). Returns
    /// immediately; `EnrichStarted` → `EnrichSucceeded{delta}`/`EnrichFailed` stream as events.
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't currently open.
    pub fn enrich_document(
        &self,
        workspace_id: &str,
        concept_id: &str,
        path: &str,
    ) -> Result<(), CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;
        let _ = handle.enrich_tx.send(EnrichJob::Doc {
            concept_id: concept_id.to_string(),
            path: path.to_string(),
        });
        Ok(())
    }

    /// Update an open workspace's live enrichment config so the worker sees changes without a
    /// reopen. Persistence is the caller's job (`WorkspaceStore::set_enrichment_config`).
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't currently open.
    pub fn set_enrichment_config(
        &self,
        workspace_id: &str,
        config: EnrichmentConfig,
    ) -> Result<(), CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;
        *lock(&handle.enrich_cfg) = config;
        Ok(())
    }

    /// Force-enrich every doc of an open workspace now (the toolbar "Enrich all" button). Returns
    /// immediately; progress streams as `EnrichAll{Started,Finished}` + per-doc `EnrichSucceeded`.
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't currently open.
    pub fn enrich_workspace(&self, workspace_id: &str) -> Result<(), CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;
        let _ = handle.enrich_tx.send(EnrichJob::All);
        Ok(())
    }

    /// Trigger an immediate sync of one folder (also the retry-after-error path).
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't open.
    pub fn sync_now(&self, workspace_id: &str, folder: &str) -> Result<(), CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;
        let _ = handle.sync_tx.send(SyncJob::SyncNow(folder.to_string()));
        Ok(())
    }

    /// Resolve a paused conflict for one folder. The outcome streams as `Sync*` events.
    ///
    /// # Errors
    /// [`CoreError::WorkspaceNotOpen`] if the workspace isn't open.
    pub fn resolve_sync_conflict(
        &self,
        workspace_id: &str,
        folder: &str,
        how: SyncResolution,
    ) -> Result<(), CoreError> {
        let workspaces = lock(&self.inner.workspaces);
        let handle = workspaces
            .get(workspace_id)
            .ok_or_else(|| CoreError::WorkspaceNotOpen(workspace_id.to_string()))?;
        let _ = handle
            .sync_tx
            .send(SyncJob::Resolve(folder.to_string(), how));
        Ok(())
    }
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        // Tear down any still-running workers when the last Engine clone drops.
        let map = match self.workspaces.get_mut() {
            Ok(map) => map,
            Err(poisoned) => poisoned.into_inner(),
        };
        let handles: Vec<WorkspaceHandle> = map.drain().map(|(_, h)| h).collect();
        for handle in handles {
            handle.stop_and_join();
        }
    }
}

/// Running counts threaded through a workspace's lifecycle.
#[derive(Default)]
struct Counts {
    indexed: u32,
    removed: u32,
}

/// The per-workspace lifecycle, run on its own thread.
#[allow(clippy::too_many_arguments)] // internal worker — every arg is borrowed workspace context
fn run_workspace(
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    workspace: &Workspace,
    stop: &AtomicBool,
    rescan: &AtomicBool,
    reconcile: &AtomicBool,
    rebuild: &AtomicBool,
    removals: &Mutex<Vec<PathBuf>>,
    ingestion: &Ingestion,
    sync_tx: &Sender<SyncJob>,
    enrich_tx: &Sender<EnrichJob>,
    kb_lock: &Mutex<()>,
) {
    let wid = workspace.id.as_str();
    emit(
        bus,
        EngineEvent::WorkspaceOpening {
            workspace_id: wid.to_string(),
            name: workspace.name.clone(),
        },
    );

    // Canonicalize folders up front so the scan, the watcher, and concept-id derivation
    // all agree on paths — notify reports realpaths (e.g. `/private/var` on macOS), which
    // would otherwise fail to `strip_prefix` the configured `/var` folder.
    let folders = canonicalize_all(&workspace.folders);

    // Migrate any pre-item-29 hidden `.looper` links in these folders to the visible `kb`
    // name (best-effort; a real `.looper` file/dir is left untouched).
    for folder in &folders {
        let _ = link::migrate_legacy(folder, &workspace.kb_dir);
    }

    // 1. Discover git repos under the workspace folders.
    let repos = discover_repos(&folders)
        .into_iter()
        .map(|repo| RepoInfo {
            name: repo.name,
            branch: match repo.head {
                HeadState::Branch(name) => Some(name),
                HeadState::Detached(_) | HeadState::Unborn => None,
            },
            linked: matches!(
                link::state(&repo.work_dir, &workspace.kb_dir),
                LinkState::Linked
            ),
            work_dir: repo.work_dir.to_string_lossy().into_owned(),
        })
        .collect();
    emit(
        bus,
        EngineEvent::ReposDiscovered {
            workspace_id: wid.to_string(),
            repos,
        },
    );

    // 2. Open the scan engine. The KB dir already exists (`open_workspace` created it and
    //    opened the backend there); it + every folder's `kb` link are excluded from scanning.
    let excluded = canonicalize_all(&workspace.exclusion_paths());
    let mut config = ScanConfig::new(folders.clone());
    config.excluded_prefixes = excluded.clone();
    config.extensions = workspace.watched_extensions.clone();
    let cursor = workspace.kb_dir.join("scan-cursor.json");
    let mut scan = match ScanEngine::open(config, cursor) {
        Ok(scan) => scan,
        Err(error) => {
            emit(
                bus,
                warn(wid, format!("scan engine failed to open: {error}")),
            );
            return;
        }
    };

    // 3. Startup catch-up scan → reconcile the index with the tree (index new/changed, evict
    //    missing or now-`.looperignore`d). The same reconcile backs the force-rescan in step 5.
    let mut counts = Counts::default();
    scan_and_apply(
        &mut scan,
        kb,
        bus,
        wid,
        &folders,
        kb_lock,
        stop,
        &mut counts,
    );

    // After-scan enrichment hook (plan item 41): kick the enrich worker to enrich the whole
    // bundle when the workspace opted in. The worker re-checks the live config + resolves the key.
    if workspace.enrichment.enabled && workspace.enrichment.enrich_after_scan {
        let _ = enrich_tx.send(EnrichJob::AfterScan);
    }

    // 4. Watch for live changes (KB dir + `.looper` excluded from watching too).
    // Also watch `.looperignore` so a hand-edit while running triggers a reconcile (item 47).
    // The watched extensions mirror the scan config (item 70), so `.adoc` etc. fire live too.
    let mut filter = WatchFilter::with_extensions(workspace.watched_extensions.iter().cloned())
        .ignoring_default_dirs()
        .also_filename(".looperignore");
    for prefix in &excluded {
        filter = filter.ignoring_path(prefix.clone());
    }
    let watch_config = WatchConfig {
        roots: folders.clone(),
        filter,
        debounce: DEFAULT_DEBOUNCE,
        respect_gitignore: true,
    };
    let (watcher, rx) = match Watcher::start(watch_config) {
        Ok(pair) => pair,
        Err(error) => {
            emit(bus, warn(wid, format!("watcher failed to start: {error}")));
            return;
        }
    };

    // 5. Drain watch events → re-index, until asked to stop. A set rescan flag (item 47) triggers a
    //    full reconcile first (re-walk + cursor diff → index new/changed, evict missing/now-ignored).
    while !stop.load(Ordering::Acquire) {
        if rebuild.swap(false, Ordering::AcqRel) {
            // Rebuild from scratch (item 47): clear the KB + reset the cursor, then fall through to
            // a rescan that re-indexes every doc (now all "new" against the empty cursor).
            {
                let _kb_guard = lock(kb_lock);
                if let Err(error) = kb.clear() {
                    emit(bus, warn(wid, format!("index clear failed: {error}")));
                }
            }
            if let Err(error) = scan.reset() {
                emit(bus, warn(wid, format!("cursor reset failed: {error}")));
            }
            rescan.store(true, Ordering::Release);
        }
        if rescan.swap(false, Ordering::AcqRel) {
            let mut rc = Counts::default();
            scan_and_apply(&mut scan, kb, bus, wid, &folders, kb_lock, stop, &mut rc);
        }
        if reconcile.swap(false, Ordering::AcqRel) {
            // A `.looperignore` change (item 48): cheap path-set reconcile, no re-hashing.
            let mut rc = Counts::default();
            reconcile_and_apply(&mut scan, kb, bus, wid, &folders, kb_lock, stop, &mut rc);
        }
        // Worker-routed single-doc removals (item 47): evict from the KB + forget the cursor entry
        // so both stay consistent (a still-on-disk doc re-adds on the next full scan).
        let pending = {
            let mut queue = lock(removals);
            std::mem::take(&mut *queue)
        };
        for path in pending {
            {
                let _kb_guard = lock(kb_lock);
                match kb.remove(&path) {
                    Ok(()) => emit(
                        bus,
                        EngineEvent::DocRemoved {
                            workspace_id: wid.to_string(),
                            path: path.to_string_lossy().into_owned(),
                        },
                    ),
                    Err(error) => emit(bus, warn(wid, format!("remove failed: {error}"))),
                }
            }
            if let Err(error) = scan.forget(&path) {
                emit(bus, warn(wid, format!("cursor forget failed: {error}")));
            }
        }
        // Drain the paused-ingestion backlog once unpaused (item 50): apply the deduped changes
        // that arrived while paused.
        if ingestion.depth.load(Ordering::Acquire) == 0 {
            let drained = {
                let mut queue = lock(&ingestion.pending);
                std::mem::take(&mut *queue)
            };
            for path in drained {
                ingest_path(
                    &mut scan,
                    kb,
                    bus,
                    wid,
                    &folders,
                    kb_lock,
                    sync_tx,
                    reconcile,
                    &mut counts,
                    &path,
                );
            }
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(WatchMessage::Event(event)) => {
                if ingestion.depth.load(Ordering::Acquire) > 0 {
                    // Paused (item 50): queue the change (deduped by path) to apply on resume.
                    lock(&ingestion.pending).insert(event.path);
                } else {
                    ingest_path(
                        &mut scan,
                        kb,
                        bus,
                        wid,
                        &folders,
                        kb_lock,
                        sync_tx,
                        reconcile,
                        &mut counts,
                        &event.path,
                    );
                }
            }
            Ok(WatchMessage::Error(message)) => emit(bus, warn(wid, message)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(watcher); // releases all OS watches
}

/// Apply a computed change set under `kb_lock` (index new/changed, **evict removed/now-ignored**)
/// and emit the scan lifecycle events. Shared by the full re-scan and the reconcile.
#[allow(clippy::too_many_arguments)] // internal scan helper — every arg is borrowed worker context
fn apply_scan(
    changes: &[Change],
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    wid: &str,
    folders: &[PathBuf],
    kb_lock: &Mutex<()>,
    stop: &AtomicBool,
    counts: &mut Counts,
) {
    let total = changes.len() as u32;
    emit(
        bus,
        EngineEvent::ScanStarted {
            workspace_id: wid.to_string(),
            total,
        },
    );
    for (done, change) in changes.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        {
            let _kb_guard = lock(kb_lock);
            apply_and_emit(kb, bus, wid, folders, change, counts);
        }
        emit(
            bus,
            EngineEvent::ScanProgress {
                workspace_id: wid.to_string(),
                done: done as u32 + 1,
                total,
                path: change.path.to_string_lossy().into_owned(),
            },
        );
    }
    emit(
        bus,
        EngineEvent::ScanCompleted {
            workspace_id: wid.to_string(),
            indexed: counts.indexed,
            removed: counts.removed,
        },
    );
}

/// Re-walk + diff against the persisted cursor (full content hashing), then apply — backs the
/// startup catch-up and the on-demand force-rescan (item 47).
#[allow(clippy::too_many_arguments)] // internal scan helper — every arg is borrowed worker context
fn scan_and_apply(
    scan: &mut ScanEngine,
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    wid: &str,
    folders: &[PathBuf],
    kb_lock: &Mutex<()>,
    stop: &AtomicBool,
    counts: &mut Counts,
) {
    match scan.full_scan() {
        Ok(changes) => apply_scan(&changes, kb, bus, wid, folders, kb_lock, stop, counts),
        Err(error) => emit(bus, warn(wid, format!("scan failed: {error}"))),
    }
}

/// **Cheap** path-set reconcile (no re-hashing) + apply — for a `.looperignore` change, where only
/// the ignore rules moved: evict now-ignored docs, index newly-un-ignored ones, skip re-hashing the
/// unchanged majority (item 48).
#[allow(clippy::too_many_arguments)] // internal scan helper — every arg is borrowed worker context
fn reconcile_and_apply(
    scan: &mut ScanEngine,
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    wid: &str,
    folders: &[PathBuf],
    kb_lock: &Mutex<()>,
    stop: &AtomicBool,
    counts: &mut Counts,
) {
    match scan.reconcile() {
        Ok(changes) => apply_scan(&changes, kb, bus, wid, folders, kb_lock, stop, counts),
        Err(error) => emit(bus, warn(wid, format!("reconcile failed: {error}"))),
    }
}

/// Observe one changed path and apply it to the index — the live-ingestion step shared by the watch
/// loop and the paused-backlog drain (item 50). A `.looperignore` path instead requests a re-scan.
#[allow(clippy::too_many_arguments)] // internal worker step — every arg is borrowed context
fn ingest_path(
    scan: &mut ScanEngine,
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    wid: &str,
    folders: &[PathBuf],
    kb_lock: &Mutex<()>,
    sync_tx: &Sender<SyncJob>,
    reconcile: &AtomicBool,
    counts: &mut Counts,
    path: &Path,
) {
    if path.file_name().is_some_and(|n| n == ".looperignore") {
        // A `.looperignore` change (items 47, 48) → cheap path-set reconcile on the next tick.
        reconcile.store(true, Ordering::Release);
        return;
    }
    if let Some(change) = scan.observe(path) {
        emit(
            bus,
            EngineEvent::FileChanged {
                workspace_id: wid.to_string(),
                path: change.path.to_string_lossy().into_owned(),
                kind: map_change_kind(change.kind),
            },
        );
        // Nudge the sync thread: a change under a synced folder debounces into a push.
        if let Some(folder) = containing_folder(&change.path, folders) {
            let _ = sync_tx.send(SyncJob::Touched(folder.to_string_lossy().into_owned()));
        }
        {
            let _kb_guard = lock(kb_lock);
            apply_and_emit(kb, bus, wid, folders, &change, counts);
        }
        let _ = scan.flush();
    }
}

/// Apply one change to the KB and emit the corresponding event.
fn apply_and_emit(
    kb: &dyn Kb,
    bus: &broadcast::Sender<EngineEvent>,
    wid: &str,
    folders: &[PathBuf],
    change: &Change,
    counts: &mut Counts,
) {
    match change.kind {
        ChangeKind::Removed => match kb.remove(&change.path) {
            Ok(()) => {
                counts.removed += 1;
                emit(
                    bus,
                    EngineEvent::DocRemoved {
                        workspace_id: wid.to_string(),
                        path: change.path.to_string_lossy().into_owned(),
                    },
                );
            }
            Err(error) => emit(bus, warn(wid, error.to_string())),
        },
        ChangeKind::Created | ChangeKind::Modified | ChangeKind::Moved => {
            // For a move, drop the old concept first so the KB keeps no stale copy.
            if let (ChangeKind::Moved, Some(from)) = (change.kind, &change.moved_from) {
                let _ = kb.remove(from);
            }
            let Some(concept_id) = concept_id_for(&change.path, folders) else {
                return; // under no workspace folder; nothing to index
            };
            let raw = match std::fs::read_to_string(&change.path) {
                Ok(raw) => raw,
                Err(error) => {
                    emit(bus, warn(wid, format!("could not read source: {error}")));
                    return;
                }
            };
            // Prepare the source into the markdown-envelope the KB indexes (item 70): markdown
            // passes through; AsciiDoc is metadata-lifted (doctitle/attrs → frontmatter, body
            // verbatim). On the rare prepare failure, fall back to the raw content.
            let extension = change
                .path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default();
            let content = match looper_ingest::prepare(extension, &raw, &concept_id) {
                Ok(content) => content,
                Err(error) => {
                    emit(bus, warn(wid, format!("ingest prepare failed: {error}")));
                    raw
                }
            };
            match kb.index(&SourceDoc {
                source_path: change.path.clone(),
                concept_id,
                content,
            }) {
                Ok(concept) => {
                    counts.indexed += 1;
                    emit(
                        bus,
                        EngineEvent::DocIndexed {
                            workspace_id: wid.to_string(),
                            concept_id: concept.concept_id,
                            okf_id: concept.okf_id,
                            path: change.path.to_string_lossy().into_owned(),
                            title: concept.title,
                            labels: concept.labels,
                        },
                    );
                }
                Err(error) => emit(bus, warn(wid, error.to_string())),
            }
        }
    }
}

/// Concept id for `path`: the containing folder's name plus the path relative to it,
/// **including** the file extension, `/`-joined (e.g. `notes/specs/auth.md`). `None` if
/// `path` is under no workspace folder. The extension is kept so distinct formats with the
/// same stem (`foo.md` vs `foo.adoc`) get distinct ids + bundles (item 70).
fn concept_id_for(path: &Path, folders: &[PathBuf]) -> Option<String> {
    for folder in folders {
        let Ok(relative) = path.strip_prefix(folder) else {
            continue;
        };
        let mut segments: Vec<String> = Vec::new();
        if let Some(name) = folder.file_name() {
            segments.push(name.to_string_lossy().into_owned());
        }
        segments.extend(
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned()),
        );
        return Some(segments.join("/"));
    }
    None
}

/// Canonicalize each path, falling back to the original when it doesn't resolve.
fn canonicalize_all(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect()
}

fn map_change_kind(kind: ChangeKind) -> EngineChangeKind {
    match kind {
        ChangeKind::Created => EngineChangeKind::Created,
        ChangeKind::Modified => EngineChangeKind::Modified,
        ChangeKind::Removed => EngineChangeKind::Removed,
        ChangeKind::Moved => EngineChangeKind::Moved,
    }
}

fn emit(bus: &broadcast::Sender<EngineEvent>, event: EngineEvent) {
    // A send error only means no subscribers are attached right now; that's fine.
    let _ = bus.send(event);
}

fn warn(workspace_id: &str, message: String) -> EngineEvent {
    EngineEvent::Warning {
        workspace_id: workspace_id.to_string(),
        message,
    }
}

/// Run a worker body, converting a **panic** into an `EngineEvent::InternalError` (item 42) so a
/// crashed worker lights the UI's error alert instead of silently dying. The global panic hook
/// (`looper-observability`) still records the full location + backtrace to the file log first; this
/// only adds the user-facing signal naming which worker/workspace stopped.
fn run_guarded(
    bus: &broadcast::Sender<EngineEvent>,
    workspace_id: &str,
    worker: &str,
    body: impl FnOnce(),
) {
    // The body owns already-`Arc`/reference-captured state; asserting unwind-safety is sound
    // because a panic ends this worker — no other code resumes using that state.
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let message = panic_payload_message(payload.as_ref());
        emit(
            bus,
            EngineEvent::InternalError {
                error: InternalErrorDraft {
                    severity: InternalErrorSeverity::Critical,
                    source: format!("worker:{worker}"),
                    summary: format!("The {worker} worker stopped after an internal panic"),
                    detail: Some(message),
                    workspace_id: Some(workspace_id.to_string()),
                },
            },
        );
    }
}

/// Best-effort extraction of a panic message from `catch_unwind`'s boxed payload.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---- Auto Sync worker (plan item 30) ------------------------------------------------

/// Everything the sync thread needs (bundled to keep `run_sync_workspace` to one argument).
struct SyncWorker {
    syncer: Arc<dyn Syncer>,
    bus: broadcast::Sender<EngineEvent>,
    wid: String,
    kb_dir: PathBuf,
    cfgs: Arc<Mutex<Vec<SyncFolderConfig>>>,
    state: Arc<Mutex<HashMap<String, SyncFolderState>>>,
    triggers: SyncTriggers,
    stop: Arc<AtomicBool>,
    rx: Receiver<SyncJob>,
    kb_lock: Arc<Mutex<()>>,
}

/// The per-workspace sync loop: on-open + periodic + on-change (debounced) syncing, plus
/// `SyncNow` / `Resolve` jobs from the engine API. All git mutations for a workspace run here,
/// so resolves never race an in-flight cycle.
fn run_sync_workspace(w: SyncWorker) {
    let poll = Duration::from_millis(500); // bounds stop latency + drives the periodic check
    let mut last_sync: HashMap<String, Instant> = HashMap::new();
    let mut debounce_due: HashMap<String, Instant> = HashMap::new();

    // Seed enabled folders as Syncing so the opening snapshot is meaningful.
    let targets: Vec<(String, bool)> = lock(&w.cfgs)
        .iter()
        .filter(|c| c.enabled)
        .map(|c| {
            (
                c.folder.clone(),
                paths_equal(Path::new(&c.folder), &w.kb_dir),
            )
        })
        .collect();
    for (folder, is_kb) in &targets {
        set_status(&w, folder, *is_kb, SyncStatus::Syncing, None);
    }
    emit_sync_snapshot(&w);

    if w.triggers.on_open {
        for folder in enabled_folders(&w.cfgs) {
            run_one(&w, &folder);
            last_sync.insert(folder, Instant::now());
        }
    }

    while !w.stop.load(Ordering::Acquire) {
        match w.rx.recv_timeout(poll) {
            Ok(SyncJob::SyncNow(folder)) => {
                run_one(&w, &folder);
                last_sync.insert(folder, Instant::now());
            }
            Ok(SyncJob::Resolve(folder, how)) => {
                run_resolve(&w, &folder, how);
                last_sync.insert(folder, Instant::now());
            }
            Ok(SyncJob::Touched(touched)) => {
                if let Some(debounce) = w.triggers.on_change_debounce {
                    // Only push-capable (PullPush) folders react to a local edit.
                    for cfg in enabled_pull_push_configs(&w.cfgs) {
                        if paths_equal(Path::new(&cfg.folder), Path::new(&touched)) {
                            debounce_due.insert(cfg.folder, Instant::now() + debounce);
                        }
                    }
                }
            }
            Ok(SyncJob::ReloadConfigs) => emit_sync_snapshot(&w),
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                // Debounced on-change syncs that have come due.
                let ready: Vec<String> = debounce_due
                    .iter()
                    .filter(|(_, &due)| due <= now)
                    .map(|(folder, _)| folder.clone())
                    .collect();
                for folder in ready {
                    debounce_due.remove(&folder);
                    if !is_paused(&w.state, &folder) {
                        run_one(&w, &folder);
                        last_sync.insert(folder, now);
                    }
                }
                // Periodic re-sync of non-paused enabled folders.
                if let Some(interval) = w.triggers.periodic {
                    for folder in enabled_folders(&w.cfgs) {
                        let elapsed = last_sync
                            .get(&folder)
                            .map_or(interval, |t| now.duration_since(*t));
                        if elapsed >= interval && !is_paused(&w.state, &folder) {
                            run_one(&w, &folder);
                            last_sync.insert(folder, now);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Run one sync cycle for `folder`, updating state and emitting `Sync*` events.
fn run_one(w: &SyncWorker, folder: &str) {
    let Some(cfg) = find_cfg(&w.cfgs, folder) else {
        return; // disabled / removed since the job was queued
    };
    let is_kb = paths_equal(Path::new(folder), &w.kb_dir);
    set_status(w, folder, is_kb, SyncStatus::Syncing, None);
    emit(
        &w.bus,
        EngineEvent::SyncStarted {
            workspace_id: w.wid.clone(),
            folder: folder.to_string(),
        },
    );
    let outcome = {
        // Serialize KB-dir git mutations against the index thread's KB writes.
        let _guard = is_kb.then(|| lock(&w.kb_lock));
        if is_kb {
            ensure_kb_gitignore(&w.kb_dir);
        }
        w.syncer.sync(Path::new(folder), &cfg)
    };
    finish(w, folder, is_kb, outcome);
}

/// Resolve a paused conflict for `folder`, then emit the resulting events.
fn run_resolve(w: &SyncWorker, folder: &str, how: SyncResolution) {
    let Some(cfg) = find_cfg(&w.cfgs, folder) else {
        return;
    };
    let is_kb = paths_equal(Path::new(folder), &w.kb_dir);
    set_status(w, folder, is_kb, SyncStatus::Syncing, None);
    emit(
        &w.bus,
        EngineEvent::SyncStarted {
            workspace_id: w.wid.clone(),
            folder: folder.to_string(),
        },
    );
    let outcome = {
        let _guard = is_kb.then(|| lock(&w.kb_lock));
        w.syncer.resolve(Path::new(folder), &cfg, how)
    };
    finish(w, folder, is_kb, outcome);
}

/// Map a backend outcome to the folder's new state + the corresponding event.
fn finish(w: &SyncWorker, folder: &str, is_kb: bool, outcome: Result<SyncOutcome, SyncError>) {
    match outcome {
        Ok(SyncOutcome::UpToDate) => {
            set_status(w, folder, is_kb, SyncStatus::Synced, Some(now_stamp()));
            emit(
                &w.bus,
                EngineEvent::SyncSucceeded {
                    workspace_id: w.wid.clone(),
                    folder: folder.to_string(),
                    pulled: false,
                    pushed: false,
                },
            );
        }
        Ok(SyncOutcome::Updated { pulled, pushed }) => {
            set_status(w, folder, is_kb, SyncStatus::Synced, Some(now_stamp()));
            emit(
                &w.bus,
                EngineEvent::SyncSucceeded {
                    workspace_id: w.wid.clone(),
                    folder: folder.to_string(),
                    pulled,
                    pushed,
                },
            );
        }
        Ok(SyncOutcome::Conflict { files }) => {
            let paths: Vec<String> = files.into_iter().map(|f| f.path).collect();
            set_status_error(
                w,
                folder,
                is_kb,
                SyncStatus::Conflict {
                    paths: paths.clone(),
                },
                "merge conflict",
            );
            emit(
                &w.bus,
                EngineEvent::SyncConflict {
                    workspace_id: w.wid.clone(),
                    folder: folder.to_string(),
                    paths,
                },
            );
        }
        Err(error) => {
            let reason = error.to_string();
            set_status_error(
                w,
                folder,
                is_kb,
                SyncStatus::Error {
                    reason: reason.clone(),
                },
                &reason,
            );
            emit(
                &w.bus,
                EngineEvent::SyncFailed {
                    workspace_id: w.wid.clone(),
                    folder: folder.to_string(),
                    reason,
                },
            );
        }
    }
}

fn set_status(
    w: &SyncWorker,
    folder: &str,
    is_kb: bool,
    status: SyncStatus,
    last_sync: Option<String>,
) {
    let mut map = lock(&w.state);
    let entry = map
        .entry(folder.to_string())
        .or_insert_with(|| base_state(&w.wid, folder, is_kb));
    entry.status = status;
    if last_sync.is_some() {
        entry.last_sync = last_sync;
        entry.last_error = None;
    }
}

fn set_status_error(w: &SyncWorker, folder: &str, is_kb: bool, status: SyncStatus, error: &str) {
    let mut map = lock(&w.state);
    let entry = map
        .entry(folder.to_string())
        .or_insert_with(|| base_state(&w.wid, folder, is_kb));
    entry.status = status;
    entry.last_error = Some(error.to_string());
}

fn base_state(wid: &str, folder: &str, is_kb: bool) -> SyncFolderState {
    SyncFolderState {
        workspace_id: wid.to_string(),
        folder: folder.to_string(),
        is_kb,
        status: SyncStatus::Off,
        last_sync: None,
        last_error: None,
    }
}

/// Emit a snapshot of all known folder states (on open + on config change).
fn emit_sync_snapshot(w: &SyncWorker) {
    let states: Vec<SyncFolderState> = lock(&w.state).values().cloned().collect();
    emit(
        &w.bus,
        EngineEvent::SyncFolders {
            workspace_id: w.wid.clone(),
            states,
        },
    );
}

fn enabled_folders(cfgs: &Mutex<Vec<SyncFolderConfig>>) -> Vec<String> {
    lock(cfgs)
        .iter()
        .filter(|c| c.enabled)
        .map(|c| c.folder.clone())
        .collect()
}

fn enabled_pull_push_configs(cfgs: &Mutex<Vec<SyncFolderConfig>>) -> Vec<SyncFolderConfig> {
    lock(cfgs)
        .iter()
        .filter(|c| c.enabled && c.direction == SyncDirection::PullPush)
        .cloned()
        .collect()
}

fn find_cfg(cfgs: &Mutex<Vec<SyncFolderConfig>>, folder: &str) -> Option<SyncFolderConfig> {
    lock(cfgs)
        .iter()
        .find(|c| c.enabled && c.folder == folder)
        .cloned()
}

fn is_paused(state: &Mutex<HashMap<String, SyncFolderState>>, folder: &str) -> bool {
    matches!(
        lock(state).get(folder).map(|s| &s.status),
        Some(SyncStatus::Error { .. } | SyncStatus::Conflict { .. })
    )
}

/// The workspace folder containing `path` (prefix match on canonical paths), if any.
fn containing_folder<'a>(path: &Path, folders: &'a [PathBuf]) -> Option<&'a PathBuf> {
    folders.iter().find(|folder| path.starts_with(folder))
}

/// True if two paths resolve to the same location (canonicalizing both; falls back to `==`).
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Ensure the KB dir's `.gitignore` excludes Looper's machine-local sidecars, so syncing the
/// KB folder never propagates them across machines. Best-effort.
fn ensure_kb_gitignore(kb_dir: &Path) {
    let path = kb_dir.join(".gitignore");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.contains("scan-cursor.json") {
        return;
    }
    let managed =
        "# Looper-managed (machine-local; do not sync)\nscan-cursor.json\n.htmlcache/\n.corpus/\n";
    let combined = if current.is_empty() {
        managed.to_string()
    } else {
        format!("{}\n{managed}", current.trim_end())
    };
    let _ = std::fs::write(&path, combined);
}

fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

// ---- Enrichment worker (plan item 41) -----------------------------------------------

/// Everything the enrichment thread needs (bundled to keep `run_enrich_workspace` to one arg).
struct EnrichWorker {
    enricher: Arc<dyn Enricher>,
    /// The workspace KB, so a finished enrichment can re-sync the catalog from the rewritten
    /// bundle (the catalog's tags reflect the KB output).
    kb: Arc<dyn Kb>,
    bus: broadcast::Sender<EngineEvent>,
    wid: String,
    kb_dir: PathBuf,
    config: Arc<Mutex<EnrichmentConfig>>,
    stop: Arc<AtomicBool>,
    rx: Receiver<EnrichJob>,
}

/// The per-workspace enrichment loop: manual per-doc enrich + enrich-all-after-scan. Runs on its
/// own thread so a slow Gemini call never stalls indexing or sync.
///
/// Enrichment is deliberately **not** taken under `kb_write_lock`: the OKF enricher writes each
/// bundle `.md` atomically, and holding a shared lock across a minutes-long API call would block
/// the index/sync threads — so we accept that a concurrent re-index of the very doc being enriched
/// is last-writer-wins (enrichment is re-derivable). Each job resolves the API key fresh.
fn run_enrich_workspace(w: EnrichWorker) {
    let poll = Duration::from_millis(500); // bounds stop latency
    while !w.stop.load(Ordering::Acquire) {
        match w.rx.recv_timeout(poll) {
            Ok(EnrichJob::Doc { concept_id, path }) => enrich_one(&w, &concept_id, &path),
            Ok(EnrichJob::AfterScan) => enrich_after_scan(&w),
            Ok(EnrichJob::All) => enrich_all_now(&w),
            Err(RecvTimeoutError::Timeout) => {} // loop to re-check `stop`
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Enrich one bundle doc (manual button), emitting `EnrichStarted` → `EnrichSucceeded`/`Failed`.
fn enrich_one(w: &EnrichWorker, concept_id: &str, path: &str) {
    // The enricher resolves its own key from the source (the engine never handles the key); a
    // missing key surfaces as `MissingApiKey`, the same whether mock- or OKF-backed (and tests
    // don't depend on the ambient `GEMINI_API_KEY`).
    let source = lock(&w.config).api_key_source;
    emit(
        &w.bus,
        EngineEvent::EnrichStarted {
            workspace_id: w.wid.clone(),
            path: path.to_string(),
        },
    );
    match w.enricher.enrich(&w.kb_dir, concept_id, source, &w.wid) {
        Ok(delta) => {
            // The enricher rewrote the bundle (KB output) with new tags/content; re-sync the
            // catalog from it so the catalog's tags reflect the output before the UI reacts.
            w.kb.refresh_indexed(Path::new(path));
            emit(
                &w.bus,
                EngineEvent::EnrichSucceeded {
                    workspace_id: w.wid.clone(),
                    path: path.to_string(),
                    delta,
                },
            );
        }
        Err(err) => emit_enrich_failed(w, path, err.to_string()),
    }
}

/// Enrich every bundle doc once (after-scan hook), emitting `EnrichSucceeded` per changed doc.
fn enrich_after_scan(w: &EnrichWorker) {
    let config = lock(&w.config).clone();
    if config.enabled && config.enrich_after_scan {
        enrich_all_now(w);
    }
}

/// Enrich every bundle doc in one batch, emitting the `EnrichAll*` lifecycle (which drives the
/// toolbar spinner) plus an `EnrichSucceeded` per changed doc. Used by both the after-scan hook and
/// the force "Enrich all" button.
fn enrich_all_now(w: &EnrichWorker) {
    emit(
        &w.bus,
        EngineEvent::EnrichAllStarted {
            workspace_id: w.wid.clone(),
        },
    );
    // The enricher resolves its own key from the source; a missing key surfaces as an `Err` below
    // (handled like any enrich-all failure: a warning + `EnrichAllFinished { enriched: 0 }`).
    let source = lock(&w.config).api_key_source;
    let enriched = match w.enricher.enrich_all(&w.kb_dir, source, &w.wid) {
        Ok(changed) => {
            for doc in &changed {
                // Re-sync each rewritten bundle into the catalog before announcing it.
                w.kb.refresh_indexed(Path::new(&doc.source_path));
                emit(
                    &w.bus,
                    EngineEvent::EnrichSucceeded {
                        workspace_id: w.wid.clone(),
                        path: doc.source_path.clone(),
                        delta: doc.delta.clone(),
                    },
                );
            }
            changed.len() as u32
        }
        Err(err) => {
            emit(&w.bus, warn(&w.wid, format!("enrich-all failed: {err}")));
            0
        }
    };
    emit(
        &w.bus,
        EngineEvent::EnrichAllFinished {
            workspace_id: w.wid.clone(),
            enriched,
        },
    );
}

/// Emit an `EnrichFailed` for `path`.
fn emit_enrich_failed(w: &EnrichWorker, path: &str, reason: impl Into<String>) {
    emit(
        &w.bus,
        EngineEvent::EnrichFailed {
            workspace_id: w.wid.clone(),
            path: path.to_string(),
            reason: reason.into(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use looper_enrichment::{EnrichedDoc, EnrichmentApiKeySource, EnrichmentDelta, MockEnricher};
    use looper_ipc::{ConflictFile, SyncStrategy};
    use looper_kb::MockKbProvider;
    use looper_sync::MockSyncer;
    use looper_workspace::WorkspaceStore;

    fn sync_cfg(folder: &str) -> SyncFolderConfig {
        SyncFolderConfig {
            folder: folder.to_string(),
            is_kb: false,
            is_git_repo: true,
            enabled: true,
            strategy: SyncStrategy::Git,
            branch: "main".to_string(),
            direction: SyncDirection::PullPush,
            dolt: None,
        }
    }

    /// Triggers that only fire on an explicit `SyncNow`/`Resolve` (deterministic for tests).
    fn manual_triggers() -> SyncTriggers {
        SyncTriggers {
            periodic: None,
            on_change_debounce: None,
            on_open: false,
        }
    }

    fn test_engine() -> Engine {
        Engine::new(
            Arc::new(MockKbProvider),
            Arc::new(MockSyncer::default()),
            Arc::new(MockEnricher::new()),
        )
    }

    #[test]
    fn run_guarded_emits_internal_error_on_panic() {
        let (bus, mut rx) = broadcast::channel::<EngineEvent>(8);
        // Silence the default panic printer so the captured panic doesn't spam test output.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        run_guarded(&bus, "ws-1", "index", || panic!("boom in the worker"));
        std::panic::set_hook(prev);

        match rx.try_recv().expect("an InternalError event was emitted") {
            EngineEvent::InternalError { error } => {
                assert_eq!(error.severity, InternalErrorSeverity::Critical);
                assert_eq!(error.source, "worker:index");
                assert_eq!(error.workspace_id.as_deref(), Some("ws-1"));
                assert!(error
                    .detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains("boom in the worker"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn run_guarded_is_silent_when_the_body_succeeds() {
        let (bus, mut rx) = broadcast::channel::<EngineEvent>(8);
        run_guarded(&bus, "ws-1", "index", || { /* no panic */ });
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    /// Drain the bus until `done` matches (or the timeout elapses), returning all events seen.
    fn collect_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        timeout: Duration,
        done: impl Fn(&EngineEvent) -> bool,
    ) -> Vec<EngineEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(event) => {
                    let finished = done(&event);
                    events.push(event);
                    if finished {
                        break;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        events
    }

    fn make_workspace(tmp: &Path, folders: Vec<PathBuf>) -> Workspace {
        let mut store = WorkspaceStore::open(tmp.join("workspaces.json")).unwrap();
        let id = store.create("Notes", folders, tmp.join("kb")).unwrap();
        store.get(&id).unwrap().clone()
    }

    #[test]
    fn manual_enrich_emits_started_then_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.md"), "# Alpha\nhi").unwrap();
        let workspace = make_workspace(tmp.path(), vec![folder]);
        let wid = workspace.id.as_str().to_string();

        let enricher = Arc::new(MockEnricher::new());
        enricher.script(EnrichmentDelta {
            labels_added: vec!["tag-a".into()],
            content_added: vec!["AI Enrichment".into()],
        });
        let engine = Engine::with_triggers(
            Arc::new(MockKbProvider),
            Arc::new(MockSyncer::default()),
            enricher.clone(),
            manual_triggers(),
        );
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();
        collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::ScanCompleted { .. })
        });

        engine
            .enrich_document(&wid, "notes/a", "/abs/notes/a.md")
            .unwrap();
        let seen = collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::EnrichSucceeded { .. })
        });

        assert!(seen.iter().any(|e| matches!(
            e,
            EngineEvent::EnrichStarted { path, .. } if path == "/abs/notes/a.md"
        )));
        let delta = seen
            .iter()
            .find_map(|e| match e {
                EngineEvent::EnrichSucceeded { path, delta, .. } if path == "/abs/notes/a.md" => {
                    Some(delta.clone())
                }
                _ => None,
            })
            .expect("EnrichSucceeded event");
        assert_eq!(delta.labels_added, vec!["tag-a"]);
        // The enricher was called once, targeting the concept id.
        let calls = enricher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "notes/a");
    }

    #[test]
    fn after_scan_enriches_all_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.md"), "# Alpha\nhi").unwrap();
        let mut workspace = make_workspace(tmp.path(), vec![folder]);
        workspace.enrichment = EnrichmentConfig {
            enabled: true,
            api_key_source: EnrichmentApiKeySource::Env,
            enrich_after_scan: true,
            ..Default::default()
        };

        let enricher = Arc::new(MockEnricher::new());
        enricher.script_all(vec![EnrichedDoc {
            source_path: "/abs/notes/a.md".into(),
            delta: EnrichmentDelta {
                labels_added: vec!["auto".into()],
                content_added: vec![],
            },
        }]);
        let engine = Engine::with_triggers(
            Arc::new(MockKbProvider),
            Arc::new(MockSyncer::default()),
            enricher.clone(),
            manual_triggers(),
        );
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();

        // After `ScanCompleted`, the after-scan hook enriches the whole bundle in one batch.
        let seen = collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::EnrichSucceeded { .. })
        });
        assert!(seen.iter().any(|e| matches!(
            e,
            EngineEvent::EnrichSucceeded { path, .. } if path == "/abs/notes/a.md"
        )));
        assert_eq!(enricher.all_calls().len(), 1);
    }

    #[test]
    fn opens_workspace_then_discovers_scans_indexes_and_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(folder.join("specs")).unwrap();
        std::fs::write(folder.join("a.md"), "# Alpha\nhello").unwrap();
        std::fs::write(folder.join("specs/b.md"), "# Beta\nworld").unwrap();

        let workspace = make_workspace(tmp.path(), vec![folder.clone()]);
        let engine = test_engine();
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();

        // Startup: ScanStarted(total=2) → two DocIndexed → ScanCompleted(indexed=2).
        let startup = collect_until(&mut events, Duration::from_secs(10), |e| {
            matches!(e, EngineEvent::ScanCompleted { .. })
        });
        assert!(
            startup
                .iter()
                .any(|e| matches!(e, EngineEvent::ScanStarted { total: 2, .. })),
            "expected ScanStarted total=2, got {startup:#?}"
        );
        let indexed = startup
            .iter()
            .filter(|e| matches!(e, EngineEvent::DocIndexed { .. }))
            .count();
        assert_eq!(indexed, 2, "expected two DocIndexed, got {startup:#?}");
        assert!(startup
            .iter()
            .any(|e| matches!(e, EngineEvent::ScanCompleted { indexed: 2, .. })));

        // Live: once watching is established, a newly created file is indexed.
        std::thread::sleep(Duration::from_millis(800));
        std::fs::write(folder.join("c.md"), "# Gamma\nnew").unwrap();
        let live = collect_until(
            &mut events,
            Duration::from_secs(20),
            |e| matches!(e, EngineEvent::DocIndexed { path, .. } if path.ends_with("c.md")),
        );
        assert!(
            live.iter().any(
                |e| matches!(e, EngineEvent::DocIndexed { path, .. } if path.ends_with("c.md"))
            ),
            "expected a live DocIndexed for c.md, got {live:#?}"
        );

        engine.shutdown();
    }

    #[test]
    fn watched_extensions_gate_adoc_discovery_and_indexing() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.md"), "# Alpha\nhello").unwrap();
        std::fs::write(folder.join("b.adoc"), "= Beta\n\nworld").unwrap();

        // Enable `.adoc` alongside `.md` for this workspace (item 70).
        let mut workspace = make_workspace(tmp.path(), vec![folder.clone()]);
        workspace.watched_extensions = vec!["md".to_string(), "adoc".to_string()];

        let engine = test_engine();
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();

        // Both the `.md` and the `.adoc` are discovered + indexed; the adoc concept id keeps
        // its extension (item 70 native-extension bundles).
        let startup = collect_until(&mut events, Duration::from_secs(10), |e| {
            matches!(e, EngineEvent::ScanCompleted { .. })
        });
        assert!(
            startup
                .iter()
                .any(|e| matches!(e, EngineEvent::ScanStarted { total: 2, .. })),
            "expected ScanStarted total=2 (md + adoc), got {startup:#?}"
        );
        // The adoc concept id keeps its extension, and its title is lifted from the `= Beta`
        // doctitle (item 70 ingest), not the filename — proof the lift is wired in.
        assert!(
            startup.iter().any(|e| matches!(
                e,
                EngineEvent::DocIndexed { concept_id, path, title, .. }
                    if concept_id == "notes/b.adoc" && path.ends_with("b.adoc") && title == "Beta"
            )),
            "expected the .adoc to index with an extension-bearing id + doctitle title, got {startup:#?}"
        );

        engine.shutdown();
    }

    #[test]
    fn rejects_double_open_of_the_same_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("f");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = make_workspace(tmp.path(), vec![folder]);

        let engine = test_engine();
        engine.open_workspace(workspace.clone()).unwrap();
        let again = engine.open_workspace(workspace);
        assert!(matches!(again, Err(CoreError::WorkspaceAlreadyOpen(_))));
        engine.shutdown();
    }

    #[test]
    fn ingestion_pause_nests_and_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("f");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = make_workspace(tmp.path(), vec![folder]);
        let id = workspace.id.as_str().to_string();

        let engine = test_engine();
        engine.open_workspace(workspace).unwrap();
        assert_eq!(
            engine.ingestion_backlog(&id),
            (0, false),
            "not paused initially"
        );

        engine.pause_ingestion(&id);
        engine.pause_ingestion(&id); // nested
        assert_eq!(
            engine.ingestion_backlog(&id),
            (0, true),
            "paused at depth 2"
        );

        engine.resume_ingestion(&id);
        assert_eq!(
            engine.ingestion_backlog(&id),
            (0, true),
            "still paused at depth 1"
        );

        engine.resume_ingestion(&id);
        assert_eq!(
            engine.ingestion_backlog(&id),
            (0, false),
            "resumed at depth 0"
        );

        engine.resume_ingestion(&id); // saturating — must not underflow
        assert_eq!(
            engine.ingestion_backlog(&id),
            (0, false),
            "resume past zero is a no-op"
        );

        engine.shutdown();
    }

    #[test]
    fn concept_id_uses_folder_name_and_relative_path() {
        let folders = vec![PathBuf::from("/home/me/notes")];
        assert_eq!(
            concept_id_for(Path::new("/home/me/notes/specs/auth.md"), &folders).as_deref(),
            Some("notes/specs/auth.md")
        );
        assert_eq!(concept_id_for(Path::new("/elsewhere/x.md"), &folders), None);
    }

    #[test]
    fn auto_sync_emits_events_and_tracks_state() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = make_workspace(tmp.path(), vec![folder.clone()]);
        let wid = workspace.id.as_str().to_string();
        let folder_str = folder.to_string_lossy().into_owned();

        let mock = Arc::new(MockSyncer::default());
        let engine = Engine::with_triggers(
            Arc::new(MockKbProvider),
            mock.clone(),
            Arc::new(MockEnricher::new()),
            manual_triggers(),
        );
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();

        // Enabling sync triggers an immediate cycle (MockSyncer returns UpToDate).
        let state = engine.set_folder_sync(&wid, sync_cfg(&folder_str)).unwrap();
        assert!(matches!(state.status, SyncStatus::Syncing));

        let seen = collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::SyncSucceeded { .. })
        });
        assert!(
            seen.iter()
                .any(|e| matches!(e, EngineEvent::SyncStarted { .. })),
            "expected SyncStarted, got {seen:#?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, EngineEvent::SyncSucceeded { .. })),
            "expected SyncSucceeded, got {seen:#?}"
        );
        assert!(mock.sync_calls() >= 1);

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            engine
                .sync_states(&wid)
                .iter()
                .any(|s| matches!(s.status, SyncStatus::Synced)),
            "expected a Synced state"
        );

        engine.shutdown();
    }

    #[test]
    fn auto_sync_conflict_pauses_then_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = make_workspace(tmp.path(), vec![folder.clone()]);
        let wid = workspace.id.as_str().to_string();
        let folder_str = folder.to_string_lossy().into_owned();

        let mock = Arc::new(MockSyncer::default());
        mock.script_sync(SyncOutcome::Conflict {
            files: vec![ConflictFile {
                path: "a.md".to_string(),
                detail: None,
            }],
        });
        let engine = Engine::with_triggers(
            Arc::new(MockKbProvider),
            mock.clone(),
            Arc::new(MockEnricher::new()),
            manual_triggers(),
        );
        let mut events = engine.subscribe();
        engine.open_workspace(workspace).unwrap();

        engine.set_folder_sync(&wid, sync_cfg(&folder_str)).unwrap();
        let seen = collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::SyncConflict { .. })
        });
        assert!(
            seen.iter()
                .any(|e| matches!(e, EngineEvent::SyncConflict { .. })),
            "expected SyncConflict, got {seen:#?}"
        );

        // Resolving runs `Syncer::resolve` (MockSyncer → UpToDate) and clears the pause.
        engine
            .resolve_sync_conflict(&wid, &folder_str, SyncResolution::UseMine)
            .unwrap();
        let resolved = collect_until(&mut events, Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::SyncSucceeded { .. })
        });
        assert!(
            resolved
                .iter()
                .any(|e| matches!(e, EngineEvent::SyncSucceeded { .. })),
            "expected SyncSucceeded after resolve, got {resolved:#?}"
        );
        assert_eq!(mock.resolve_calls(), 1);

        engine.shutdown();
    }
}
