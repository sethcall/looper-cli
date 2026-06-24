//! `looper-ipc` — shared IPC data-transfer objects for Looper.
//!
//! The single source of truth for command inputs/outputs and event payloads crossing
//! the Rust<->TypeScript boundary. `ts-rs` generates the matching TypeScript types
//! (see `just ts-sync`); the frontend imports those instead of hand-writing DTOs.
//!
//! Dependency rule: **leaf** crate; must NOT depend on `tauri` (so domain crates can
//! construct DTOs). See `../../AGENTS.md`.
//! Implements plan item 07 (`../../specs/plan/07-ipc-and-type-gen.md`).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Static information about the running application, returned by the `app_info` command.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct AppInfo {
    /// Product name.
    pub name: String,
    /// Semantic version of the application.
    pub version: String,
}

/// A change classification for engine file events (the boundary mirror of the scan layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum EngineChangeKind {
    /// A new tracked file appeared.
    Created,
    /// An existing file changed.
    Modified,
    /// A tracked file was removed.
    Removed,
    /// A file moved to a new path.
    Moved,
}

/// A git repository discovered in a workspace, as surfaced to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct RepoInfo {
    /// Display name (the work-dir's file name).
    pub name: String,
    /// Current branch, if HEAD is on one.
    pub branch: Option<String>,
    /// Absolute path of the repository working directory.
    pub work_dir: String,
    /// Whether the repo is `.looper`-linked to the workspace KB.
    pub linked: bool,
}

/// Typed events emitted by the engine as it opens workspaces, scans, indexes, and watches.
/// `looper-app` forwards these to the frontend verbatim (a discriminated union on `type`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
#[serde(tag = "type")]
pub enum EngineEvent {
    /// A workspace lifecycle has begun.
    WorkspaceOpening { workspace_id: String, name: String },
    /// Git repositories under the workspace folders were discovered.
    ReposDiscovered {
        workspace_id: String,
        repos: Vec<RepoInfo>,
    },
    /// The startup catch-up scan found `total` changes to apply.
    ScanStarted { workspace_id: String, total: u32 },
    /// Progress applying the startup scan.
    ScanProgress {
        workspace_id: String,
        done: u32,
        total: u32,
        path: String,
    },
    /// A document was indexed (created or updated) into the KB.
    DocIndexed {
        workspace_id: String,
        concept_id: String,
        okf_id: String,
        path: String,
        /// Display title as known to the index (frontmatter → first `# H1` → filename).
        title: String,
        /// Labels/tags from the document's frontmatter; empty when none.
        labels: Vec<String>,
    },
    /// A document was removed from the KB.
    DocRemoved { workspace_id: String, path: String },
    /// The startup scan finished.
    ScanCompleted {
        workspace_id: String,
        indexed: u32,
        removed: u32,
    },
    /// A live file change was observed by the watcher.
    FileChanged {
        workspace_id: String,
        path: String,
        kind: EngineChangeKind,
    },
    /// A non-fatal problem (watch backend hiccup, an unreadable file, etc.).
    Warning {
        workspace_id: String,
        message: String,
    },
    /// Snapshot of every sync target's state, emitted when a workspace opens (item 30).
    SyncFolders {
        workspace_id: String,
        states: Vec<SyncFolderState>,
    },
    /// A folder's sync cycle began.
    SyncStarted {
        workspace_id: String,
        folder: String,
    },
    /// Progress within a sync cycle (`phase` = "commit" | "fetch" | "merge" | "push").
    SyncProgress {
        workspace_id: String,
        folder: String,
        phase: String,
    },
    /// A folder finished syncing cleanly.
    SyncSucceeded {
        workspace_id: String,
        folder: String,
        pulled: bool,
        pushed: bool,
    },
    /// A folder's sync failed for a non-conflict reason (auth, network, no remote, …).
    SyncFailed {
        workspace_id: String,
        folder: String,
        reason: String,
    },
    /// A folder hit a merge conflict and is now paused awaiting resolution.
    SyncConflict {
        workspace_id: String,
        folder: String,
        paths: Vec<String>,
    },
    /// A document's Gemini enrichment began (item 41).
    EnrichStarted { workspace_id: String, path: String },
    /// A document was enriched; `delta` is what Gemini added (tags + content sections).
    EnrichSucceeded {
        workspace_id: String,
        path: String,
        delta: EnrichmentDelta,
    },
    /// A document's enrichment failed (no key, tool error, timeout, …).
    EnrichFailed {
        workspace_id: String,
        path: String,
        reason: String,
    },
    /// A force "enrich all" pass for a workspace began (drives the toolbar spinner).
    EnrichAllStarted { workspace_id: String },
    /// A force "enrich all" pass finished; `enriched` is how many docs actually changed.
    EnrichAllFinished { workspace_id: String, enriched: u32 },
    /// An **internal** failure the app should capture into its error tracker (item 42): a
    /// worker-thread panic or another unexpected engine fault. `looper-app` stamps it with an
    /// `id`/`at` and persists it; the frontend surfaces a red status-bar alert. Distinct from
    /// [`EngineEvent::Warning`], which is for soft/expected messages that must *not* alarm.
    InternalError { error: InternalErrorDraft },
}

/// How loud an [`InternalError`] is — drives whether the status-bar alert lights (item 42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum InternalErrorSeverity {
    /// Recorded for review but does not raise the red alert (e.g. a failed IPC command).
    Warning,
    /// Raises the alert: something unexpected failed (e.g. enrichment crashed).
    Error,
    /// Raises the alert; a worker thread panicked and may have stopped making progress.
    Critical,
}

/// A reported internal failure **before** the app assigns it an identity — what every capture
/// channel (engine event, failed command, frontend fault) hands to the tracker (item 42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct InternalErrorDraft {
    pub severity: InternalErrorSeverity,
    /// Short origin label, e.g. `"worker:index"`, `"enrichment"`, `"command:enrich_document"`,
    /// `"frontend"`.
    pub source: String,
    /// One-line human summary shown in the list row.
    pub summary: String,
    /// Optional longer detail (a reason string, a panic message, a stack) shown when expanded.
    pub detail: Option<String>,
    /// The workspace the failure belongs to, when applicable.
    pub workspace_id: Option<String>,
}

/// A captured internal failure as persisted and surfaced to the UI (item 42): an
/// [`InternalErrorDraft`] stamped with a stable `id` and a capture time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct InternalError {
    /// Stable identifier (capture-time millis + a per-process sequence) — a React key + ack target.
    pub id: String,
    /// Capture time, milliseconds since the Unix epoch.
    pub at: u64,
    pub severity: InternalErrorSeverity,
    pub source: String,
    pub summary: String,
    pub detail: Option<String>,
    pub workspace_id: Option<String>,
}

/// A workspace as surfaced to the UI (paths stringified).
/// The knowledge-base backend for a workspace. OKF is the only one today; the `Kb`/`KbProvider`
/// seam makes others possible, and the engine selects the backend per workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum KbBackend {
    /// OKF — the Open Knowledge Format producer (markdown bundle + stable ids + sidecar + viz).
    #[default]
    Okf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct WorkspaceInfo {
    /// Stable id (slug derived from the name).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Folders that comprise the workspace.
    pub folders: Vec<String>,
    /// Directory holding the workspace's knowledge base.
    pub kb_dir: String,
    /// Max rendered-HTML files kept in the document-viewer LRU cache (plan item 27).
    pub html_cache_size: u32,
    /// The knowledge-base backend (OKF today; the seam allows others).
    pub backend: KbBackend,
}

/// Request payload to create a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct CreateWorkspaceRequest {
    /// Display name.
    pub name: String,
    /// Folders to harvest.
    pub folders: Vec<String>,
    /// Docs-only repositories selected as specs folders at creation time.
    #[serde(default)]
    pub specs_folders: Vec<String>,
    /// Directory for the knowledge base.
    pub kb_dir: String,
}

/// A direct child directory found while setting up a workspace from a parent folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct WorkspaceFolderCandidate {
    /// Display name, derived from the directory's final path segment.
    pub name: String,
    /// Absolute, canonical folder path.
    pub path: String,
    /// Whether the folder itself is a Git repository.
    pub is_git_repo: bool,
    /// Whether the folder appears to contain markdown content.
    pub has_markdown: bool,
}

/// Request payload to edit an existing workspace's properties (plan item 27).
///
/// Folders and the KB directory are **not** editable yet — changing them needs real work
/// (re-index / relink), tracked by the "Editable source folders" / "Editable KB folder"
/// tasks — so only the name and the document-viewer cache size can change here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct UpdateWorkspaceRequest {
    /// Id of the workspace to update.
    pub id: String,
    /// New display name.
    pub name: String,
    /// New document-viewer LRU cache size.
    pub html_cache_size: u32,
}

/// A workspace folder's link status, for the sidebar's per-folder "link" toggle (item 29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct FolderLink {
    /// The folder's path (as stored on the workspace).
    pub path: String,
    /// Whether a visible `kb` link to the workspace KB currently exists in the folder.
    pub linked: bool,
}

/// A knowledge-base search result, surfaced to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct SearchHit {
    /// The concept id that matched.
    pub concept_id: String,
    /// The concept's title.
    pub title: String,
    /// A short snippet around the match.
    pub snippet: String,
    /// The source file path (so the UI can open the matched document).
    pub path: String,
    /// Labels/tags from the document's frontmatter.
    pub labels: Vec<String>,
}

// ---- Catalog + tag index -------------------------------------------------------

/// A lightweight catalog entry for one indexed document — a whole-KB listing (path + title +
/// labels) for browsing or visualizing the corpus (file trees, tag filters, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct DocSummary {
    /// The **source** file path (the UI's open key + tree path) — shown prominently to the user.
    pub path: String,
    /// The **knowledge-base output** path: the emitted bundle file this doc is indexed as.
    /// Consumers read this (augmented) version for content.
    pub kb_path: String,
    /// Display title (frontmatter `title:` → first `# H1` → filename), when known.
    pub title: Option<String>,
    /// Labels/tags from the **bundle** (KB output) frontmatter; empty when none.
    pub labels: Vec<String>,
}

/// One tag and how many indexed documents carry it — for tag-filter UIs, from the KB's persisted
/// tag → documents index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct TagCount {
    /// The tag (a frontmatter label).
    pub tag: String,
    /// Number of indexed documents carrying this tag.
    pub count: u32,
}

// ---- Enrichment (plan item 41) ------------------------------------------------------

/// Where the Gemini API key comes from for a workspace's enrichment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum EnrichmentApiKeySource {
    /// Reuse the shell's `GEMINI_API_KEY`.
    #[default]
    Env,
    /// Use a key the user supplied (stored in the OS keychain, never in config).
    Specified,
}

/// Which embedding backend a workspace uses for corpus analysis (the Enrichment tab, item 44).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum EmbeddingBackend {
    /// Local `bge-small-en-v1.5` via fastembed (ONNX) — free, private, offline, no rate limit.
    /// The default; it beat the Gemini baseline on duplicate recall in our bench.
    #[default]
    Local,
    /// Gemini cloud embeddings (`gemini-embedding-001`) — needs a Gemini API key.
    Gemini,
}

/// Per-workspace Gemini configuration: doc enrichment (item 41) + the corpus-analysis embedding
/// backend (item 44), which share one Gemini key. The key value itself is never stored here —
/// it is read from `GEMINI_API_KEY` or the OS keychain at run time.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct EnrichmentConfig {
    /// Whether enrichment is enabled for this workspace.
    pub enabled: bool,
    /// Where the API key comes from.
    pub api_key_source: EnrichmentApiKeySource,
    /// Auto-enrich every doc after a scan completes (cost-guarded; see the UI warning).
    pub enrich_after_scan: bool,
    /// Which embedding backend corpus analysis uses (item 44). `#[serde(default)]` → Local for
    /// configs written before this field.
    #[serde(default)]
    pub embedding_backend: EmbeddingBackend,
}

/// What Gemini enrichment added to a bundle document — the "delta" the user reviews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct EnrichmentDelta {
    /// Labels/tags newly added to the document's frontmatter.
    pub labels_added: Vec<String>,
    /// Headings of the content sections enrichment added (the AI-enrichment body section).
    pub content_added: Vec<String>,
}

/// A document's runtime enrichment status (a discriminated union on `type`), per path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
#[serde(tag = "type")]
pub enum EnrichmentStatus {
    /// Not enriched yet (no run for this doc).
    Off,
    /// An enrichment run is in progress.
    Enriching,
    /// Successfully enriched.
    Enriched,
    /// The last enrichment attempt failed.
    Error { reason: String },
}

// ---- Workspace Linking (plan items 49, 55) ------------------------------------------

/// How a generated link's on-disk name is chosen (item 49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum LinkNaming {
    /// Use the target folder's local directory name.
    #[default]
    LocalDirName,
    /// Use the target's git **repo name** (derived from its remote), when the target is a repo.
    GitTrueName,
    /// Use the user-provided `custom_name`.
    Custom,
}

/// Which target a link points at (items 49, 55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum LinkTargetKind {
    /// The workspace's KB folder.
    #[default]
    Kb,
    /// A specs folder (identified by [`LinkConfig::specs_folder`]).
    Specs,
}

/// One generated link's settings, keyed by its **host** folder and target (item 55). Every normal
/// source folder links to every specs folder + the KB; this overrides one such link's name +
/// `.gitignore`. A `(host, target)` pair with no entry uses the defaults (KB → `kb`, specs → its
/// local dir name, no `.gitignore`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct LinkConfig {
    /// The non-spec "host" source folder this link lives in.
    pub host: String,
    /// Whether the target is the KB or a specs folder.
    pub kind: LinkTargetKind,
    /// The specs folder's path when `kind == Specs`; `None` for the KB.
    pub specs_folder: Option<String>,
    /// How this link is named.
    pub naming: LinkNaming,
    /// The custom link name when `naming == Custom`.
    pub custom_name: Option<String>,
    /// When enabled, applying this link also adds its path to the host repo's `.gitignore`.
    pub gitignore: bool,
}

/// A workspace's Linking configuration (items 49, 55): which folders are docs-only **specs
/// folders** (link targets, marked Authoritative), plus per-`(host, target)` link naming +
/// `.gitignore` overrides. There is no global policy — every normal source folder links to every
/// specs folder + the KB; the left-nav **Link** checkbox per host turns those links on/off.
/// `#[serde(default)]` keeps older/partial configs loadable — including the pre-item-55 shape
/// (`policy` + `targets`), whose now-removed fields are ignored while `specs_folders` is preserved.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export, export_to = "../../../bindings/")]
pub struct LinkingConfig {
    /// Workspace folders (by path) annotated as docs-only specs folders. A subset of the
    /// workspace's folders; never the KB folder.
    pub specs_folders: Vec<String>,
    /// Per-`(host, target)` link naming + `.gitignore` overrides; absent pairs use defaults.
    pub links: Vec<LinkConfig>,
}

/// Read-only context for rendering the Linking editor's per-host tree (item 55): the normal source
/// folders (hosts) with a glimpse of their real contents, and the link targets (KB + specs) with
/// git info for the naming controls.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct LinkingPreview {
    /// Normal (non-spec) source folders — the link hosts.
    pub hosts: Vec<HostPreview>,
    /// Link targets: the KB first, then each specs folder.
    pub targets: Vec<TargetPreview>,
}

/// One source folder (a link host) in the Linking preview (item 55).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct HostPreview {
    /// Absolute path (shown on hover).
    pub path: String,
    /// Last path component plus its parent (e.g. `vf/looper`) — a friendlier on-disk label.
    pub short_path: String,
    /// A few of the folder's real (non-symlink) entries, for a `tree`-style glimpse.
    pub entries: Vec<String>,
    /// Whether this folder is a git repo (shows a git icon; gates the `.gitignore` toggle).
    pub is_git_repo: bool,
    /// Whether this host currently holds Looper-generated links (drives the Link toggle).
    pub linked: bool,
}

/// One link target (the KB or a specs folder) in the Linking preview (item 55).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct TargetPreview {
    /// Whether this is the KB or a specs folder.
    pub kind: LinkTargetKind,
    /// The specs folder's path when `kind == Specs`; `None` for the KB.
    pub specs_folder: Option<String>,
    /// The target's absolute path.
    pub path: String,
    /// The target folder's local directory name (the default link name).
    pub name: String,
    /// Whether the target is a git repository — gates the "Git repo name" naming option.
    pub is_git_repo: bool,
    /// The target's git repo name (from its remote), when discoverable.
    pub git_name: Option<String>,
}

/// What applying a link's `.gitignore` toggle will do on Save, computed against the **live** file
/// (item 55) — drives the inline diff preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum GitignoreChange {
    /// The file is absent (or empty) — it will be created with the line.
    Create,
    /// The file exists — the line will be appended at the end.
    Append,
    /// The line is present and will be removed.
    Remove,
    /// Turning it on, but the line is already there — nothing to do.
    #[default]
    AlreadyPresent,
    /// Turning it off, but the line isn't there — nothing to do.
    AlreadyAbsent,
}

/// A preview of the `.gitignore` change a link's toggle will make on Save (item 55).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct GitignorePreview {
    /// Absolute path of the host's `.gitignore`.
    pub path: String,
    /// The anchored line, e.g. `/kb`.
    pub line: String,
    /// What will happen on Save, relative to the file as it is now.
    pub change: GitignoreChange,
    /// The file's last few existing lines, for context above an append/remove.
    pub context: Vec<String>,
}

// ---- Tidy Up / .looperignore (plan item 46) -----------------------------------------

/// A preview of what writing patterns to a folder's `.looperignore` will do — so the user sees the
/// exact target path, whether it's created or edited, the current contents, and the lines that get
/// appended. The write is confirmed only after this preview.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct LooperignorePreview {
    /// Absolute path of the `.looperignore` that will be written.
    pub path: String,
    /// Whether the file already exists (an **edit**, appending) or will be **created**.
    pub exists: bool,
    /// The file's current contents (empty when it doesn't exist) — shown above the additions.
    pub existing: String,
    /// The new lines appended at the end, already de-duplicated against `existing` (empty → nothing
    /// to do, everything is already ignored).
    pub additions: Vec<String>,
}

// ---- Activity carousel (plan item 45) -----------------------------------------------

/// One file-change mined from git history — seeds the per-folder Activity carousel so a freshly
/// opened Looper shows real activity immediately (item 45), bounded to a recent window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct FileActivity {
    /// Absolute path of the changed markdown file.
    pub path: String,
    /// Commit time, milliseconds since the Unix epoch.
    pub at: f64,
}

// ---- Ingestion control (plan item 50) -----------------------------------------------

/// A workspace's ingestion backlog: changes queued behind an ingestion pause, and whether ingestion
/// is currently paused (item 50).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct IngestionBacklog {
    /// Number of changes queued behind the pause.
    pub pending: u32,
    /// Whether ingestion is currently paused.
    pub paused: bool,
}

// ---- Auto Sync (plan item 30) -------------------------------------------------------

/// The sync backend strategy for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum SyncStrategy {
    /// Use git as a fileshare (the GitFileshare backend).
    Git,
    /// Dolt — designed, not yet implemented (a stub backend).
    Dolt,
}

/// Which way a folder auto-syncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum SyncDirection {
    /// Auto-commit, pull, and push (full fileshare).
    #[default]
    PullPush,
    /// Auto-pull only; the user commits + pushes manually.
    PullOnly,
}

/// Connection settings for the Dolt backend (stub — not yet wired).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
#[serde(default)]
pub struct DoltConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub branch: String,
    pub user: String,
}

fn default_sync_branch() -> String {
    "main".to_string()
}

/// Per-folder Auto Sync configuration (persisted on the workspace; item 30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct SyncFolderConfig {
    /// The folder's path (as stored on the workspace; the KB folder is also a target).
    pub folder: String,
    /// Whether this target is the workspace's KB folder (called out in the UI).
    pub is_kb: bool,
    /// Whether the folder is already a git repo (computed backend-side; gates the Git option).
    pub is_git_repo: bool,
    /// Whether Auto Sync is enabled for this folder.
    pub enabled: bool,
    /// The backend strategy.
    pub strategy: SyncStrategy,
    /// The branch to sync (Git), defaulting to `main`.
    #[serde(default = "default_sync_branch")]
    pub branch: String,
    /// Pull + push vs pull-only.
    #[serde(default)]
    pub direction: SyncDirection,
    /// Dolt connection settings (when `strategy == Dolt`).
    pub dolt: Option<DoltConfig>,
}

/// A folder's runtime sync status (a discriminated union on `type`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
#[serde(tag = "type")]
pub enum SyncStatus {
    /// Auto Sync is not enabled for this folder.
    Off,
    /// A sync cycle is in progress.
    Syncing,
    /// Up to date with the remote.
    Synced,
    /// A non-conflict failure paused the folder.
    Error { reason: String },
    /// A merge conflict paused the folder, awaiting resolution.
    Conflict { paths: Vec<String> },
}

/// A single conflicted file surfaced to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct ConflictFile {
    pub path: String,
    pub detail: Option<String>,
}

/// A folder's full sync state, surfaced to the UI (item 30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct SyncFolderState {
    pub workspace_id: String,
    pub folder: String,
    pub is_kb: bool,
    pub status: SyncStatus,
    /// ISO-8601 timestamp of the last successful sync, if any.
    pub last_sync: Option<String>,
    /// The last error / conflict message, if any.
    pub last_error: Option<String>,
}

/// How the user chose to resolve a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub enum SyncResolution {
    /// Keep our version of each conflicted file.
    UseMine,
    /// Keep their version of each conflicted file.
    UseTheirs,
    /// The user edited the working tree; commit + push the result.
    MarkResolved,
}

/// The resolved git executable Auto Sync will use (plan item 30.11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct GitInfo {
    /// Absolute path to the git binary that was found / configured.
    pub path: String,
    /// `git --version` output.
    pub version: String,
}

/// Result of a per-folder Auto Sync "Test" — safe, read-only checks (plan item 30.11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../bindings/")]
pub struct SyncTestReport {
    /// Overall: the folder looks ready to sync.
    pub ok: bool,
    /// The resolved git binary, if one was found.
    pub git: Option<GitInfo>,
    /// The folder is a git repository.
    pub is_repo: bool,
    /// Current branch, if on one.
    pub branch: Option<String>,
    /// A remote/upstream is configured.
    pub has_remote: bool,
    /// `None` = not tested (no remote / skipped); `Some` = `git ls-remote` reachability + auth.
    pub remote_reachable: Option<bool>,
    /// A human-readable summary or the first failure encountered.
    pub summary: String,
}
