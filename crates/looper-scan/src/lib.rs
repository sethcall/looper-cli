//! `looper-scan` — markdown discovery, fingerprinting, and change detection.
//!
//! Walks workspace roots (gitignore-aware, via the `ignore` crate), collects tracked
//! markdown (`*.md`), and fingerprints each file (`blake3` hash + size + mtime). A
//! persisted [`Snapshot`] is the *cursor*: on startup [`ScanEngine::full_scan`] re-walks
//! and [`diff`]s against the cursor to recover changes made while Looper was off
//! (Created / Modified / Removed / Moved). Live watcher events are folded in one path at
//! a time via [`ScanEngine::observe`] (kept decoupled from `looper-watcher` — the core
//! passes paths), with [`ScanEngine::flush`] persisting after a batch. Source files are
//! never modified.
//!
//! Dependency rule: may depend on `looper-state`. See `../../AGENTS.md`.
//! Implements plan item 11 (`../../specs/plan/11-markdown-scan-engine.md`).

mod error;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use looper_state::{Store, Versioned};
use serde::{Deserialize, Serialize};

pub use error::ScanError;

/// Directory names that mark "documentation" locations (`doc|docs|spec|specs`).
const DOCS_DIR_NAMES: [&str; 4] = ["doc", "docs", "spec", "specs"];

/// Directory names always pruned from the walk (in addition to gitignore).
const PRUNED_DIR_NAMES: [&str; 3] = [".git", "node_modules", "target"];

/// A content + metadata fingerprint of a markdown file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// `blake3` content hash (hex).
    pub hash: String,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time, milliseconds since the Unix epoch (`0` if unavailable).
    pub mtime_ms: u64,
}

/// The kind of change detected for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A new tracked file appeared.
    Created,
    /// An existing file's content changed.
    Modified,
    /// A tracked file disappeared.
    Removed,
    /// A file moved (same content hash at a new path).
    Moved,
}

/// A detected change to a tracked markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The (current) path of the file. For [`ChangeKind::Moved`] this is the new path.
    pub path: PathBuf,
    /// What changed.
    pub kind: ChangeKind,
    /// The new fingerprint (`None` for [`ChangeKind::Removed`]).
    pub fingerprint: Option<FileFingerprint>,
    /// For [`ChangeKind::Moved`], the previous path.
    pub moved_from: Option<PathBuf>,
}

/// A snapshot of tracked markdown fingerprints — the persisted scan cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, FileFingerprint>,
}

impl Versioned for Snapshot {
    const SCHEMA_VERSION: u32 = 1;
}

impl Snapshot {
    /// Number of tracked files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the snapshot is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The fingerprint for `path`, if tracked.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&FileFingerprint> {
        self.files.get(path)
    }

    /// Iterate over (path, fingerprint) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &FileFingerprint)> {
        self.files.iter()
    }
}

/// Configuration for a scan.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Root directories to scan.
    pub roots: Vec<PathBuf>,
    /// Honor `.gitignore` (and `.ignore` / global / exclude files). Default `true`.
    pub respect_gitignore: bool,
    /// Path prefixes to exclude (e.g. a workspace's KB dir and `.looper` links).
    pub excluded_prefixes: Vec<PathBuf>,
    /// File extensions to index, lowercase, no dot (e.g. `["md", "adoc"]`). Defaults to
    /// `["md"]`; the engine sets it from the workspace's watched extensions (item 70).
    pub extensions: Vec<String>,
}

impl ScanConfig {
    /// A gitignore-respecting config for `roots` with no extra exclusions, watching `.md`.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            respect_gitignore: true,
            excluded_prefixes: Vec::new(),
            extensions: vec!["md".to_string()],
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.excluded_prefixes.iter().any(|p| path.starts_with(p))
    }

    /// Whether `path`'s extension is in the configured watched set (case-insensitive).
    fn is_tracked(&self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        self.extensions.iter().any(|x| x.eq_ignore_ascii_case(ext))
    }
}

/// Whether `path` is a markdown file (`.md`, case-insensitive).
#[must_use]
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// Whether `path` lies within a `doc|docs|spec|specs` directory.
#[must_use]
pub fn is_in_docs_dir(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, std::path::Component::Normal(name)
            if name.to_str().is_some_and(|n| DOCS_DIR_NAMES.contains(&n)))
    })
}

/// Compute a fingerprint for the file at `path` (streams the content through blake3).
///
/// # Errors
/// Returns an I/O error if the file cannot be read.
pub fn fingerprint(path: &Path) -> std::io::Result<FileFingerprint> {
    let meta = std::fs::metadata(path)?;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Ok(FileFingerprint {
        hash: hasher.finalize().to_hex().to_string(),
        size: meta.len(),
        mtime_ms,
    })
}

/// Walk `config.roots` and collect every tracked markdown file **path** (honoring `.gitignore` /
/// `.looperignore` / exclusions) — **without** fingerprinting. The cheap path for a reconcile that
/// only needs the membership set, not content hashes (item 48).
#[must_use]
pub fn walk_markdown_paths(config: &ScanConfig) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in &config.roots {
        let mut builder = WalkBuilder::new(root);
        builder
            .git_ignore(config.respect_gitignore)
            .git_global(config.respect_gitignore)
            .git_exclude(config.respect_gitignore)
            .ignore(config.respect_gitignore)
            .parents(config.respect_gitignore)
            .require_git(false)
            .hidden(false);
        // Always honor Looper's own ignore file (item 46): how the user excludes vendored noise,
        // independent of git. Layered like `.gitignore` (per-directory, nested-aware).
        builder.add_custom_ignore_filename(".looperignore");

        let excluded = config.excluded_prefixes.clone();
        builder.filter_entry(move |entry| {
            if entry.file_type().is_some_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| PRUNED_DIR_NAMES.contains(&n))
            {
                return false;
            }
            !excluded.iter().any(|p| entry.path().starts_with(p))
        });

        for entry in builder.build().filter_map(Result::ok) {
            let path = entry.path();
            if entry.file_type().is_some_and(|t| t.is_file()) && config.is_tracked(path) {
                paths.push(path.to_path_buf());
            }
        }
    }
    paths
}

/// Walk `config.roots` and fingerprint every tracked markdown file into a [`Snapshot`].
#[must_use]
pub fn scan_tracked_markdown(config: &ScanConfig) -> Snapshot {
    let mut files = BTreeMap::new();
    for path in walk_markdown_paths(config) {
        if let Ok(fp) = fingerprint(&path) {
            files.insert(path, fp);
        }
    }
    Snapshot { files }
}

/// Diff two snapshots into a list of [`Change`]s, detecting moves by matching content
/// hashes between removed and added paths.
#[must_use]
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();

    // Modified: present in both with a different hash.
    for (path, new_fp) in new.iter() {
        if let Some(old_fp) = old.get(path) {
            if old_fp.hash != new_fp.hash {
                changes.push(Change {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                    fingerprint: Some(new_fp.clone()),
                    moved_from: None,
                });
            }
        }
    }

    let removed: Vec<(&PathBuf, &FileFingerprint)> =
        old.iter().filter(|(p, _)| new.get(p).is_none()).collect();
    let added: Vec<(&PathBuf, &FileFingerprint)> =
        new.iter().filter(|(p, _)| old.get(p).is_none()).collect();

    // Index removed files by content hash so additions can be matched as moves.
    let mut removed_by_hash: HashMap<&str, Vec<&PathBuf>> = HashMap::new();
    for (path, fp) in &removed {
        removed_by_hash
            .entry(fp.hash.as_str())
            .or_default()
            .push(path);
    }
    let mut consumed: HashSet<&PathBuf> = HashSet::new();

    for (path, fp) in &added {
        if let Some(candidates) = removed_by_hash.get(fp.hash.as_str()) {
            if let Some(from) = candidates.iter().find(|p| !consumed.contains(**p)) {
                consumed.insert(*from);
                changes.push(Change {
                    path: (*path).clone(),
                    kind: ChangeKind::Moved,
                    fingerprint: Some((*fp).clone()),
                    moved_from: Some((*from).clone()),
                });
                continue;
            }
        }
        changes.push(Change {
            path: (*path).clone(),
            kind: ChangeKind::Created,
            fingerprint: Some((*fp).clone()),
            moved_from: None,
        });
    }

    for (path, _) in &removed {
        if !consumed.contains(path) {
            changes.push(Change {
                path: (*path).clone(),
                kind: ChangeKind::Removed,
                fingerprint: None,
                moved_from: None,
            });
        }
    }

    changes
}

/// The stateful scan engine: owns the persisted cursor and reconciles it with the tree.
pub struct ScanEngine {
    config: ScanConfig,
    store: Store<Snapshot>,
    snapshot: Snapshot,
}

impl ScanEngine {
    /// Open the engine, loading the persisted cursor from `cursor_path`.
    ///
    /// # Errors
    /// Returns [`ScanError`] if the cursor cannot be loaded.
    pub fn open(config: ScanConfig, cursor_path: impl Into<PathBuf>) -> Result<Self, ScanError> {
        let store = Store::new(cursor_path);
        let snapshot = store.load()?;
        Ok(Self {
            config,
            store,
            snapshot,
        })
    }

    /// The current cursor snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Reset the cursor to empty + persist, so the next [`Self::full_scan`] treats every file as new
    /// — the "rebuild from scratch" path (item 47).
    ///
    /// # Errors
    /// Returns [`ScanError`] if persisting the cleared cursor fails.
    pub fn reset(&mut self) -> Result<(), ScanError> {
        self.snapshot = Snapshot::default();
        self.persist()
    }

    /// Forget `path` from the cursor + persist, so the index and cursor stay consistent after a
    /// single-doc `Kb::remove` (item 47). If the file still exists on disk + isn't ignored, the next
    /// `full_scan` re-adds it (a transient remove); to exclude it permanently use `.looperignore`.
    ///
    /// # Errors
    /// Returns [`ScanError`] if persisting fails.
    pub fn forget(&mut self, path: &Path) -> Result<(), ScanError> {
        if self.snapshot.files.remove(path).is_some() {
            self.persist()?;
        }
        Ok(())
    }

    /// Re-walk for the current path **set** (honoring `.looperignore`) **without** re-hashing
    /// unchanged files, and diff against the cursor: `Removed` for paths gone or now-ignored,
    /// `Created` (fingerprinted) for paths newly present or un-ignored. Modifications are **not**
    /// detected — this is the cheap path for a `.looperignore` change, where file *contents* are
    /// unchanged and only the ignore rules moved (item 48). Avoids re-hashing the unchanged majority.
    ///
    /// # Errors
    /// Returns [`ScanError`] if persisting the updated cursor fails.
    pub fn reconcile(&mut self) -> Result<Vec<Change>, ScanError> {
        let walked: std::collections::BTreeSet<PathBuf> =
            walk_markdown_paths(&self.config).into_iter().collect();
        let mut changes = Vec::new();

        // Removed: in the cursor but no longer walked (now ignored / deleted).
        let gone: Vec<PathBuf> = self
            .snapshot
            .files
            .keys()
            .filter(|p| !walked.contains(*p))
            .cloned()
            .collect();
        for path in gone {
            self.snapshot.files.remove(&path);
            changes.push(Change {
                path,
                kind: ChangeKind::Removed,
                fingerprint: None,
                moved_from: None,
            });
        }

        // Added: walked but not in the cursor (newly un-ignored / created) → fingerprint + track.
        for path in &walked {
            if !self.snapshot.files.contains_key(path) {
                if let Ok(fp) = fingerprint(path) {
                    self.snapshot.files.insert(path.clone(), fp.clone());
                    changes.push(Change {
                        path: path.clone(),
                        kind: ChangeKind::Created,
                        fingerprint: Some(fp),
                        moved_from: None,
                    });
                }
            }
        }

        self.persist()?;
        Ok(changes)
    }

    /// Re-walk the roots, diff against the cursor (the startup catch-up), then adopt and
    /// persist the fresh snapshot.
    ///
    /// # Errors
    /// Returns [`ScanError`] if persisting the new cursor fails.
    pub fn full_scan(&mut self) -> Result<Vec<Change>, ScanError> {
        let fresh = scan_tracked_markdown(&self.config);
        let changes = diff(&self.snapshot, &fresh);
        self.snapshot = fresh;
        self.persist()?;
        Ok(changes)
    }

    /// React to a single changed path (typically from the watcher), updating the
    /// in-memory cursor. Call [`Self::flush`] to persist after a batch. Returns the
    /// detected change, if any (paths that are not tracked markdown, or unchanged, yield
    /// `None`).
    pub fn observe(&mut self, path: &Path) -> Option<Change> {
        if !self.config.is_tracked(path) || self.config.is_excluded(path) {
            return None;
        }
        let previous = self.snapshot.files.get(path).cloned();
        match fingerprint(path) {
            Ok(fp) => {
                let kind = match &previous {
                    None => ChangeKind::Created,
                    Some(prev) if prev.hash != fp.hash => ChangeKind::Modified,
                    Some(_) => return None, // unchanged
                };
                self.snapshot.files.insert(path.to_path_buf(), fp.clone());
                Some(Change {
                    path: path.to_path_buf(),
                    kind,
                    fingerprint: Some(fp),
                    moved_from: None,
                })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if previous.is_some() {
                    self.snapshot.files.remove(path);
                    Some(Change {
                        path: path.to_path_buf(),
                        kind: ChangeKind::Removed,
                        fingerprint: None,
                        moved_from: None,
                    })
                } else {
                    None
                }
            }
            // Transient read error (e.g. a mid-write file): ignore; the watcher re-fires.
            Err(_) => None,
        }
    }

    /// Persist the current cursor.
    ///
    /// # Errors
    /// Returns [`ScanError`] if the write fails.
    pub fn flush(&mut self) -> Result<(), ScanError> {
        self.persist()
    }

    fn persist(&self) -> Result<(), ScanError> {
        self.store.save(&self.snapshot)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn markdown_and_docs_dir_classification() {
        assert!(is_markdown(Path::new("a/b.md")));
        assert!(is_markdown(Path::new("a/b.MD")));
        assert!(!is_markdown(Path::new("a/b.txt")));
        assert!(is_in_docs_dir(Path::new("/x/docs/a.md")));
        assert!(is_in_docs_dir(Path::new("/x/spec/a.md")));
        assert!(!is_in_docs_dir(Path::new("/x/src/a.md")));
    }

    #[test]
    fn fingerprint_is_content_sensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.md");
        write(&p, "hello");
        let f1 = fingerprint(&p).unwrap();
        assert_eq!(f1.size, 5);
        write(&p, "hello world");
        let f2 = fingerprint(&p).unwrap();
        assert_ne!(f1.hash, f2.hash);
        assert_eq!(f2.size, 11);
    }

    #[test]
    fn scan_collects_markdown_and_honors_ignores() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("docs/a.md"), "a");
        write(&root.join("README.md"), "r");
        write(&root.join("notes.txt"), "t"); // not markdown
        write(&root.join(".gitignore"), "ignored.md\n");
        write(&root.join("ignored.md"), "x"); // gitignored
        write(&root.join("node_modules/pkg/b.md"), "b"); // pruned
        write(&root.join("kb/index.md"), "k"); // excluded prefix
        write(&root.join(".looperignore"), "vendored/\n");
        write(&root.join("vendored/c.md"), "c"); // looperignored

        let mut config = ScanConfig::new(vec![root.to_path_buf()]);
        config.excluded_prefixes = vec![root.join("kb")];
        let snap = scan_tracked_markdown(&config);

        assert!(snap.get(&root.join("docs/a.md")).is_some());
        assert!(snap.get(&root.join("README.md")).is_some());
        assert!(snap.get(&root.join("notes.txt")).is_none());
        assert!(snap.get(&root.join("ignored.md")).is_none(), "gitignored");
        assert!(
            snap.get(&root.join("node_modules/pkg/b.md")).is_none(),
            "pruned"
        );
        assert!(
            snap.get(&root.join("kb/index.md")).is_none(),
            "excluded prefix"
        );
        assert!(
            snap.get(&root.join("vendored/c.md")).is_none(),
            "looperignored"
        );
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn scan_honors_configured_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("a.md"), "a");
        write(&root.join("b.adoc"), "b");
        write(&root.join("c.txt"), "c");

        // Default config watches `.md` only — `.adoc` is not collected.
        let md_only = ScanConfig::new(vec![root.to_path_buf()]);
        let snap = scan_tracked_markdown(&md_only);
        assert!(snap.get(&root.join("a.md")).is_some());
        assert!(snap.get(&root.join("b.adoc")).is_none());
        assert_eq!(snap.len(), 1);

        // Enabling `adoc` collects both (still not `.txt`); observe() agrees.
        let mut both = ScanConfig::new(vec![root.to_path_buf()]);
        both.extensions = vec!["md".to_string(), "adoc".to_string()];
        let snap = scan_tracked_markdown(&both);
        assert!(snap.get(&root.join("a.md")).is_some());
        assert!(snap.get(&root.join("b.adoc")).is_some());
        assert!(snap.get(&root.join("c.txt")).is_none());
        assert_eq!(snap.len(), 2);

        // The live-watch path uses the same set: a tracked `.adoc` is observed (Created on a
        // fresh cursor), a non-watched `.txt` is ignored.
        let mut engine = ScanEngine::open(both, tmp.path().join("cursor.json")).unwrap();
        assert_eq!(
            engine.observe(&root.join("b.adoc")).unwrap().kind,
            ChangeKind::Created
        );
        assert!(engine.observe(&root.join("c.txt")).is_none());
    }

    #[test]
    fn diff_detects_created_modified_removed_moved() {
        let fp = |h: &str| FileFingerprint {
            hash: h.to_string(),
            size: 1,
            mtime_ms: 0,
        };
        let mut old = Snapshot::default();
        old.files.insert("keep.md".into(), fp("H_keep"));
        old.files.insert("edit.md".into(), fp("H_old"));
        old.files.insert("gone.md".into(), fp("H_gone"));
        old.files.insert("from.md".into(), fp("H_move"));

        let mut new = Snapshot::default();
        new.files.insert("keep.md".into(), fp("H_keep")); // unchanged
        new.files.insert("edit.md".into(), fp("H_new")); // modified
        new.files.insert("fresh.md".into(), fp("H_fresh")); // created
        new.files.insert("to.md".into(), fp("H_move")); // moved from from.md

        let changes = diff(&old, &new);
        let kind = |p: &str| {
            changes
                .iter()
                .find(|c| c.path.as_path() == Path::new(p))
                .map(|c| c.kind)
        };
        assert_eq!(kind("edit.md"), Some(ChangeKind::Modified));
        assert_eq!(kind("fresh.md"), Some(ChangeKind::Created));
        assert_eq!(kind("gone.md"), Some(ChangeKind::Removed));
        assert_eq!(kind("to.md"), Some(ChangeKind::Moved));
        assert_eq!(
            changes
                .iter()
                .find(|c| c.path.as_path() == Path::new("to.md"))
                .unwrap()
                .moved_from,
            Some(PathBuf::from("from.md"))
        );
        assert!(kind("keep.md").is_none(), "unchanged file yields no change");
    }

    #[test]
    fn engine_catch_up_and_live_observe_with_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        let cursor = tmp.path().join("cursor.json");
        write(&root.join("a.md"), "a");
        write(&root.join("b.md"), "b");

        // First full scan: everything is Created.
        let mut engine = ScanEngine::open(ScanConfig::new(vec![root.clone()]), &cursor).unwrap();
        let changes = engine.full_scan().unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.kind == ChangeKind::Created));

        // Reopen with the persisted cursor + no tree changes -> no changes.
        let mut engine = ScanEngine::open(ScanConfig::new(vec![root.clone()]), &cursor).unwrap();
        assert!(engine.full_scan().unwrap().is_empty(), "cursor caught up");

        // Live: modify a, remove b, add c.
        write(&root.join("a.md"), "a2");
        std::fs::remove_file(root.join("b.md")).unwrap();
        write(&root.join("c.md"), "c");
        assert_eq!(
            engine.observe(&root.join("a.md")).unwrap().kind,
            ChangeKind::Modified
        );
        assert_eq!(
            engine.observe(&root.join("b.md")).unwrap().kind,
            ChangeKind::Removed
        );
        assert_eq!(
            engine.observe(&root.join("c.md")).unwrap().kind,
            ChangeKind::Created
        );
        assert!(engine.observe(&root.join("notes.txt")).is_none());
        engine.flush().unwrap();

        // The persisted cursor now matches the tree.
        let mut engine = ScanEngine::open(ScanConfig::new(vec![root]), &cursor).unwrap();
        assert!(engine.full_scan().unwrap().is_empty());
    }

    #[test]
    fn reconcile_evicts_now_ignored_and_re_adds_unignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        let cursor = tmp.path().join("cursor.json");
        write(&root.join("a.md"), "a");
        write(&root.join("vendored/b.md"), "b");

        let mut engine = ScanEngine::open(ScanConfig::new(vec![root.clone()]), &cursor).unwrap();
        assert_eq!(engine.full_scan().unwrap().len(), 2, "both tracked");

        // Ignore the vendored subtree (no content change) → reconcile drops only it.
        write(&root.join(".looperignore"), "vendored/\n");
        let changes = engine.reconcile().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Removed);
        assert!(changes[0].path.ends_with("vendored/b.md"));

        // Un-ignore it → reconcile re-adds it as Created.
        write(&root.join(".looperignore"), "\n");
        let back = engine.reconcile().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].kind, ChangeKind::Created);
        assert!(back[0].path.ends_with("vendored/b.md"));
    }
}
