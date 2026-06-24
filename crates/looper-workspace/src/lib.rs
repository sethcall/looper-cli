//! `looper-workspace` — workspace model, persistence, and `.looper` linking.
//!
//! A [`Workspace`] is a named set of folders plus a KB directory (multiple may be active
//! at once). [`WorkspaceStore`] persists them as versioned JSON via `looper-state`.
//! [`Workspace::exclusion_paths`] yields the paths scanning/watching must skip (the KB
//! dir + every folder's `kb` link) to avoid an indexing feedback loop. Cross-platform
//! `kb` linking lives in [`link`].
//!
//! Dependency rule: may depend on `looper-state`. Repository *discovery* is orchestrated
//! by the core engine (using `looper-git`), not here. See `../../AGENTS.md`.
//! Implements plan item 10 (`../../specs/plan/10-workspace-model-and-linking.md`).

mod error;
pub mod gitignore;
pub mod link;
pub mod linking;

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use looper_ipc::{LinkingConfig, WorkspaceFolderCandidate};
use looper_state::{Store, Versioned};
use serde::{Deserialize, Serialize};

pub use error::WorkspaceError;
pub use link::LinkState;

/// A stable workspace identifier (slug derived from the name at creation; survives renames).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Default number of rendered-HTML files kept in a workspace's document-viewer LRU cache
/// (plan item 27). Tunable per workspace, but only via edit-properties — never at setup.
pub const DEFAULT_HTML_CACHE_SIZE: usize = 50;

fn default_html_cache_size() -> usize {
    DEFAULT_HTML_CACHE_SIZE
}

/// A workspace: a named set of folders plus the directory holding its knowledge base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Stable identifier.
    pub id: WorkspaceId,
    /// Human-readable name.
    pub name: String,
    /// Folders that comprise the workspace.
    pub folders: Vec<PathBuf>,
    /// Directory holding the workspace's knowledge base.
    pub kb_dir: PathBuf,
    /// Max rendered-HTML files kept in the document-viewer LRU cache (plan item 27).
    /// `#[serde(default)]` keeps workspaces written before this field loadable.
    #[serde(default = "default_html_cache_size")]
    pub html_cache_size: usize,
    /// Per-folder Auto Sync configuration (plan item 30). One entry per enabled folder /
    /// KB target; `#[serde(default)]` keeps pre-item-30 workspaces loadable.
    #[serde(default)]
    pub sync: Vec<looper_ipc::SyncFolderConfig>,
    /// The knowledge-base backend. `#[serde(default)]` → OKF for workspaces written before this
    /// field; OKF is the only backend today, but the engine selects per workspace.
    #[serde(default)]
    pub backend: looper_ipc::KbBackend,
    /// Gemini enrichment configuration (plan item 41). `#[serde(default)]` → disabled for
    /// workspaces written before this field. The key value lives in env/keychain, never here.
    #[serde(default)]
    pub enrichment: looper_ipc::EnrichmentConfig,
    /// Workspace Linking configuration (plan item 49): specs-folder roles + link target policy.
    /// `#[serde(default)]` → an empty config (the pre-item-49 `kb`-only behavior) for older
    /// workspaces.
    #[serde(default)]
    pub linking: looper_ipc::LinkingConfig,
}

impl Workspace {
    /// Paths that scanning and watching must exclude: the KB directory and every
    /// folder's `kb` link (prevents Looper's own writes from re-triggering indexing).
    #[must_use]
    pub fn exclusion_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.folders.len() + 1);
        paths.push(self.kb_dir.clone());
        for folder in &self.folders {
            paths.push(link::link_path(folder));
        }
        paths
    }
}

/// Discover candidate source folders from the direct children of `parent`.
///
/// This is intentionally shallow: workspace setup commonly starts from a folder that contains
/// sibling repositories, and deep recursion would surface vendored or nested dependency repos.
///
/// # Errors
/// Returns [`WorkspaceError`] if `parent` cannot be read or is not a directory.
pub fn discover_folder_candidates(
    parent: impl AsRef<Path>,
) -> Result<Vec<WorkspaceFolderCandidate>, WorkspaceError> {
    let parent = parent.as_ref();
    let root = dunce::canonicalize(parent).map_err(|source| WorkspaceError::FolderDiscovery {
        path: parent.to_path_buf(),
        source,
    })?;
    if !root.is_dir() {
        return Err(WorkspaceError::NotADirectory(root));
    }

    let entries = std::fs::read_dir(&root).map_err(|source| WorkspaceError::FolderDiscovery {
        path: root.clone(),
        source,
    })?;
    let mut candidates = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || is_pruned_candidate(&entry.file_name()) {
            continue;
        }

        let path = dunce::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_git_repo = path.join(".git").exists() && looper_git::open_repo(&path).is_ok();
        let has_markdown = contains_markdown(&path);
        candidates.push(WorkspaceFolderCandidate {
            name,
            path: path.to_string_lossy().into_owned(),
            is_git_repo,
            has_markdown,
        });
    }

    candidates.sort_by(|a, b| {
        alphanumeric_cmp(&a.name, &b.name).then_with(|| alphanumeric_cmp(&a.path, &b.path))
    });
    Ok(candidates)
}

fn alphanumeric_cmp(a: &str, b: &str) -> Ordering {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut a_index = 0;
    let mut b_index = 0;

    while a_index < a_bytes.len() && b_index < b_bytes.len() {
        let a_byte = a_bytes[a_index];
        let b_byte = b_bytes[b_index];

        if a_byte.is_ascii_digit() && b_byte.is_ascii_digit() {
            let a_start = a_index;
            let b_start = b_index;
            while a_index < a_bytes.len() && a_bytes[a_index].is_ascii_digit() {
                a_index += 1;
            }
            while b_index < b_bytes.len() && b_bytes[b_index].is_ascii_digit() {
                b_index += 1;
            }

            let a_digits = &a[a_start..a_index];
            let b_digits = &b[b_start..b_index];
            let a_number = a_digits.trim_start_matches('0');
            let b_number = b_digits.trim_start_matches('0');
            let a_number = if a_number.is_empty() { "0" } else { a_number };
            let b_number = if b_number.is_empty() { "0" } else { b_number };

            let ord = a_number
                .len()
                .cmp(&b_number.len())
                .then_with(|| a_number.cmp(b_number))
                .then_with(|| a_digits.len().cmp(&b_digits.len()));
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }

        let ord = a_byte
            .to_ascii_lowercase()
            .cmp(&b_byte.to_ascii_lowercase());
        if ord != Ordering::Equal {
            return ord;
        }

        a_index += 1;
        b_index += 1;
    }

    a_bytes.len().cmp(&b_bytes.len())
}

fn is_pruned_candidate(name: &OsStr) -> bool {
    matches!(
        name.to_string_lossy().as_ref(),
        ".git" | "node_modules" | "target"
    )
}

fn contains_markdown(dir: &Path) -> bool {
    const MAX_MARKDOWN_PROBE_DEPTH: usize = 5;

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .max_depth(Some(MAX_MARKDOWN_PROBE_DEPTH))
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            entry
                .path()
                .file_name()
                .is_none_or(|name| !is_pruned_candidate(name))
        });

    for entry in builder.build().filter_map(Result::ok) {
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            && is_markdown_path(entry.path())
        {
            return true;
        }
    }
    false
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "mdx" | "markdown"))
}

/// The persisted document of all workspaces.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WorkspacesDoc {
    workspaces: Vec<Workspace>,
}

impl Versioned for WorkspacesDoc {
    const SCHEMA_VERSION: u32 = 1;
}

/// A persisted collection of workspaces.
pub struct WorkspaceStore {
    store: Store<WorkspacesDoc>,
    doc: WorkspacesDoc,
}

impl WorkspaceStore {
    /// Open (and load) the workspace store backed by the JSON file at `path`.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] if the existing file cannot be read/parsed.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let store = Store::new(path);
        let doc = store.load()?;
        Ok(Self { store, doc })
    }

    /// All workspaces.
    #[must_use]
    pub fn list(&self) -> &[Workspace] {
        &self.doc.workspaces
    }

    /// Look up a workspace by id.
    #[must_use]
    pub fn get(&self, id: &WorkspaceId) -> Option<&Workspace> {
        self.doc.workspaces.iter().find(|w| &w.id == id)
    }

    /// Create a workspace and persist it, returning its assigned id.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] if persistence fails.
    pub fn create(
        &mut self,
        name: impl Into<String>,
        folders: Vec<PathBuf>,
        kb_dir: PathBuf,
    ) -> Result<WorkspaceId, WorkspaceError> {
        self.create_with_linking(name, folders, kb_dir, LinkingConfig::default())
    }

    /// Create a workspace with its Linking configuration and persist it once, returning its id.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] if persistence fails.
    pub fn create_with_linking(
        &mut self,
        name: impl Into<String>,
        folders: Vec<PathBuf>,
        kb_dir: PathBuf,
        linking: LinkingConfig,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let name = name.into();
        let id = self.unique_id(&name);
        self.doc.workspaces.push(Workspace {
            id: id.clone(),
            name,
            folders,
            kb_dir,
            html_cache_size: default_html_cache_size(),
            sync: Vec::new(),
            backend: looper_ipc::KbBackend::default(),
            enrichment: looper_ipc::EnrichmentConfig::default(),
            linking,
        });
        self.persist()?;
        Ok(id)
    }

    /// Rename a workspace (its id is unchanged).
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn rename(
        &mut self,
        id: &WorkspaceId,
        name: impl Into<String>,
    ) -> Result<(), WorkspaceError> {
        self.workspace_mut(id)?.name = name.into();
        self.persist()
    }

    /// Replace a workspace's folders.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn set_folders(
        &mut self,
        id: &WorkspaceId,
        folders: Vec<PathBuf>,
    ) -> Result<(), WorkspaceError> {
        self.workspace_mut(id)?.folders = folders;
        self.persist()
    }

    /// Set a workspace's document-viewer LRU cache size (plan item 27).
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn set_html_cache_size(
        &mut self,
        id: &WorkspaceId,
        size: usize,
    ) -> Result<(), WorkspaceError> {
        self.workspace_mut(id)?.html_cache_size = size;
        self.persist()
    }

    /// Set a workspace's Gemini enrichment configuration (plan item 41).
    ///
    /// The API key value is never stored here — only the source + flags. Key resolution (env var
    /// or OS keychain) lives in `looper-enrichment`.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn set_enrichment_config(
        &mut self,
        id: &WorkspaceId,
        config: looper_ipc::EnrichmentConfig,
    ) -> Result<(), WorkspaceError> {
        self.workspace_mut(id)?.enrichment = config;
        self.persist()
    }

    /// Set a workspace's Linking configuration (plan item 49): specs-folder roles + link policy.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn set_linking_config(
        &mut self,
        id: &WorkspaceId,
        config: looper_ipc::LinkingConfig,
    ) -> Result<(), WorkspaceError> {
        self.workspace_mut(id)?.linking = config;
        self.persist()
    }

    /// Upsert a folder's Auto Sync config (plan item 30); a disabled config is removed.
    ///
    /// Entries are keyed by `cfg.folder`. When `cfg.enabled` is false the entry is dropped,
    /// so a disabled folder leaves no residue.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn set_folder_sync_config(
        &mut self,
        id: &WorkspaceId,
        cfg: looper_ipc::SyncFolderConfig,
    ) -> Result<(), WorkspaceError> {
        let workspace = self.workspace_mut(id)?;
        workspace.sync.retain(|c| c.folder != cfg.folder);
        if cfg.enabled {
            workspace.sync.push(cfg);
        }
        self.persist()
    }

    /// Remove a workspace.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::NotFound`] for an unknown id, or a persistence error.
    pub fn remove(&mut self, id: &WorkspaceId) -> Result<(), WorkspaceError> {
        let before = self.doc.workspaces.len();
        self.doc.workspaces.retain(|w| &w.id != id);
        if self.doc.workspaces.len() == before {
            return Err(WorkspaceError::NotFound(id.to_string()));
        }
        self.persist()
    }

    fn workspace_mut(&mut self, id: &WorkspaceId) -> Result<&mut Workspace, WorkspaceError> {
        self.doc
            .workspaces
            .iter_mut()
            .find(|w| &w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))
    }

    fn persist(&self) -> Result<(), WorkspaceError> {
        self.store.save(&self.doc)?;
        Ok(())
    }

    fn unique_id(&self, name: &str) -> WorkspaceId {
        let base = {
            let slug = slugify(name);
            if slug.is_empty() {
                "workspace".to_string()
            } else {
                slug
            }
        };
        if !self.id_taken(&base) {
            return WorkspaceId(base);
        }
        let mut n = 2usize;
        loop {
            let candidate = format!("{base}-{n}");
            if !self.id_taken(&candidate) {
                return WorkspaceId(candidate);
            }
            n += 1;
        }
    }

    fn id_taken(&self, id: &str) -> bool {
        self.doc.workspaces.iter().any(|w| w.id.0 == id)
    }
}

/// Lowercase, collapse non-alphanumeric runs to single `-`, and trim leading/trailing `-`.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn slugify_cases() {
        assert_eq!(slugify("My Docs"), "my-docs");
        assert_eq!(slugify("  Hello,  World!! "), "hello-world");
        assert_eq!(slugify("already-slug"), "already-slug");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn discovers_direct_child_folder_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        let docs = root.join("docs");
        let plain = root.join("plain");
        let nested_repo = plain.join("nested-repo");
        let target = root.join("target");

        for dir in [&repo, &docs, &plain, &nested_repo, &target] {
            std::fs::create_dir_all(dir).unwrap();
        }
        run_git(&repo, &["init"]);
        run_git(&nested_repo, &["init"]);
        std::fs::write(repo.join("README.md"), "# Repo\n").unwrap();
        std::fs::write(docs.join("guide.md"), "# Guide\n").unwrap();
        std::fs::write(nested_repo.join("nested.md"), "# Nested\n").unwrap();

        let candidates = discover_folder_candidates(root).unwrap();
        let names: Vec<_> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["docs", "plain", "repo"],
            "candidate folders sort alphabetically, target is pruned, nested repos are not surfaced"
        );

        let repo = candidates.iter().find(|c| c.name == "repo").unwrap();
        assert!(repo.is_git_repo);
        assert!(repo.has_markdown, "git repos still get markdown labels");

        let docs = candidates.iter().find(|c| c.name == "docs").unwrap();
        assert!(!docs.is_git_repo);
        assert!(docs.has_markdown);

        let plain = candidates.iter().find(|c| c.name == "plain").unwrap();
        assert!(!plain.is_git_repo);
        assert!(
            plain.has_markdown,
            "markdown probe sees shallow nested docs"
        );
    }

    #[test]
    fn discovery_sorts_candidates_alphanumerically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        for name in ["repo-10", "repo-2", "repo-1", "Repo-03", "alpha"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }

        let candidates = discover_folder_candidates(root).unwrap();
        let names: Vec<_> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "repo-1", "repo-2", "Repo-03", "repo-10"]
        );
    }

    #[test]
    fn markdown_probe_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        let ignored = repo.join("ignored");

        std::fs::create_dir_all(&ignored).unwrap();
        run_git(&repo, &["init"]);
        std::fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(ignored.join("README.md"), "# Ignored\n").unwrap();

        let candidates = discover_folder_candidates(root).unwrap();
        let repo = candidates.iter().find(|c| c.name == "repo").unwrap();

        assert!(repo.is_git_repo);
        assert!(
            !repo.has_markdown,
            "ignored markdown should not make a candidate look document-bearing"
        );
    }

    #[test]
    fn create_list_get_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        let mut store = WorkspaceStore::open(&path).unwrap();
        assert!(store.list().is_empty());

        let id = store
            .create("My Docs", vec![tmp.path().join("a")], tmp.path().join("kb"))
            .unwrap();
        assert_eq!(id.as_str(), "my-docs");
        assert_eq!(store.list().len(), 1);
        assert!(store.get(&id).is_some());

        // Persisted across reopen.
        let mut reopened = WorkspaceStore::open(&path).unwrap();
        assert_eq!(reopened.list().len(), 1);
        assert_eq!(reopened.get(&id).unwrap().name, "My Docs");

        reopened.rename(&id, "Renamed").unwrap();
        assert_eq!(reopened.get(&id).unwrap().name, "Renamed");
        assert_eq!(reopened.get(&id).unwrap().id.as_str(), "my-docs"); // id stable

        reopened.remove(&id).unwrap();
        assert!(reopened.list().is_empty());
        assert!(matches!(
            reopened.remove(&id),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn html_cache_size_defaults_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        let mut store = WorkspaceStore::open(&path).unwrap();
        let id = store.create("Docs", vec![], tmp.path().join("kb")).unwrap();
        // New workspaces default to DEFAULT_HTML_CACHE_SIZE.
        assert_eq!(
            store.get(&id).unwrap().html_cache_size,
            DEFAULT_HTML_CACHE_SIZE
        );

        // The setter changes it and the change survives a reopen.
        store.set_html_cache_size(&id, 8).unwrap();
        let reopened = WorkspaceStore::open(&path).unwrap();
        assert_eq!(reopened.get(&id).unwrap().html_cache_size, 8);
    }

    #[test]
    fn enrichment_config_defaults_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        let mut store = WorkspaceStore::open(&path).unwrap();
        let id = store.create("Docs", vec![], tmp.path().join("kb")).unwrap();
        // New workspaces default to enrichment off, env-var source.
        assert_eq!(
            store.get(&id).unwrap().enrichment,
            looper_ipc::EnrichmentConfig::default()
        );
        assert!(!store.get(&id).unwrap().enrichment.enabled);

        // The setter persists across a reopen (the config holds no key value — only the source).
        store
            .set_enrichment_config(
                &id,
                looper_ipc::EnrichmentConfig {
                    enabled: true,
                    api_key_source: looper_ipc::EnrichmentApiKeySource::Specified,
                    enrich_after_scan: true,
                    embedding_backend: looper_ipc::EmbeddingBackend::Gemini,
                },
            )
            .unwrap();
        let reopened = WorkspaceStore::open(&path).unwrap();
        let enrichment = &reopened.get(&id).unwrap().enrichment;
        assert!(enrichment.enabled);
        assert_eq!(
            enrichment.api_key_source,
            looper_ipc::EnrichmentApiKeySource::Specified
        );
        assert!(enrichment.enrich_after_scan);
    }

    #[test]
    fn linking_config_defaults_and_persists() {
        use looper_ipc::{LinkConfig, LinkNaming, LinkTargetKind, LinkingConfig};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        let mut store = WorkspaceStore::open(&path).unwrap();
        let id = store.create("Docs", vec![], tmp.path().join("kb")).unwrap();
        // New workspaces default to an empty linking config.
        assert_eq!(store.get(&id).unwrap().linking, LinkingConfig::default());

        let config = LinkingConfig {
            specs_folders: vec!["/ws/specs".to_string()],
            links: vec![LinkConfig {
                host: "/ws/code".to_string(),
                kind: LinkTargetKind::Kb,
                specs_folder: None,
                naming: LinkNaming::Custom,
                custom_name: Some("brain".to_string()),
                gitignore: true,
            }],
        };
        store.set_linking_config(&id, config.clone()).unwrap();

        // The full config survives a reopen.
        let reopened = WorkspaceStore::open(&path).unwrap();
        assert_eq!(reopened.get(&id).unwrap().linking, config);
    }

    #[test]
    fn pre_item_55_linking_shape_loads_with_specs_preserved() {
        // A workspaces.json written by item 49 (policy + targets) must still load: the now-removed
        // fields are ignored, specs_folders is kept, and links defaults to empty.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"data":{"workspaces":[
                {"id":"legacy","name":"L","folders":[],"kb_dir":"/tmp/kb",
                 "linking":{"specs_folders":["/ws/specs"],"policy":"Both",
                   "targets":[{"kind":"Kb","specs_folder":null,"naming":"Custom",
                     "custom_name":"brain","gitignore":true}]}}
            ]}}"#,
        )
        .unwrap();
        let store = WorkspaceStore::open(&path).unwrap();
        let ws = store.get(&WorkspaceId("legacy".to_string())).unwrap();
        assert_eq!(ws.linking.specs_folders, vec!["/ws/specs".to_string()]);
        assert!(
            ws.linking.links.is_empty(),
            "old per-target config is dropped"
        );
    }

    #[test]
    fn workspaces_without_cache_size_load_with_default() {
        // A workspaces.json written before html_cache_size existed must still load.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"data":{"workspaces":[
                {"id":"legacy","name":"Legacy","folders":[],"kb_dir":"/tmp/kb"}
            ]}}"#,
        )
        .unwrap();
        let store = WorkspaceStore::open(&path).unwrap();
        let ws = store.get(&WorkspaceId("legacy".to_string())).unwrap();
        assert_eq!(ws.html_cache_size, DEFAULT_HTML_CACHE_SIZE);
        assert!(
            ws.sync.is_empty(),
            "sync defaults to empty for legacy workspaces"
        );
        assert_eq!(
            ws.linking,
            looper_ipc::LinkingConfig::default(),
            "linking defaults to empty for legacy workspaces"
        );
    }

    #[test]
    fn folder_sync_config_upsert_and_remove() {
        use looper_ipc::{SyncDirection, SyncFolderConfig, SyncStrategy};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");
        let mut store = WorkspaceStore::open(&path).unwrap();
        let id = store.create("Docs", vec![], tmp.path().join("kb")).unwrap();

        let mut cfg = SyncFolderConfig {
            folder: "/ws/a".to_string(),
            is_kb: false,
            is_git_repo: true,
            enabled: true,
            strategy: SyncStrategy::Git,
            branch: "main".to_string(),
            direction: SyncDirection::PullPush,
            dolt: None,
        };
        store.set_folder_sync_config(&id, cfg.clone()).unwrap();
        assert_eq!(store.get(&id).unwrap().sync.len(), 1);

        // Upsert (same folder) replaces, doesn't duplicate.
        cfg.direction = SyncDirection::PullOnly;
        store.set_folder_sync_config(&id, cfg.clone()).unwrap();
        let synced = &store.get(&id).unwrap().sync;
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].direction, SyncDirection::PullOnly);

        // Survives a reopen.
        let mut reopened = WorkspaceStore::open(&path).unwrap();
        assert_eq!(reopened.get(&id).unwrap().sync.len(), 1);

        // Disabling removes the entry.
        cfg.enabled = false;
        reopened.set_folder_sync_config(&id, cfg).unwrap();
        assert!(reopened.get(&id).unwrap().sync.is_empty());
    }

    #[test]
    fn ids_are_disambiguated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(tmp.path().join("ws.json")).unwrap();
        let a = store
            .create("Docs", vec![], tmp.path().join("kb1"))
            .unwrap();
        let b = store
            .create("Docs", vec![], tmp.path().join("kb2"))
            .unwrap();
        assert_eq!(a.as_str(), "docs");
        assert_eq!(b.as_str(), "docs-2");
    }

    #[test]
    fn exclusion_paths_cover_kb_and_folder_links() {
        let ws = Workspace {
            id: WorkspaceId("w".to_string()),
            name: "w".to_string(),
            folders: vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")],
            kb_dir: PathBuf::from("/ws/kb"),
            html_cache_size: DEFAULT_HTML_CACHE_SIZE,
            sync: Vec::new(),
            backend: looper_ipc::KbBackend::default(),
            enrichment: looper_ipc::EnrichmentConfig::default(),
            linking: looper_ipc::LinkingConfig::default(),
        };
        let ex = ws.exclusion_paths();
        assert!(ex.contains(&PathBuf::from("/ws/kb")));
        assert!(ex.contains(&PathBuf::from("/ws/a/kb")));
        assert!(ex.contains(&PathBuf::from("/ws/b/kb")));
    }
}
