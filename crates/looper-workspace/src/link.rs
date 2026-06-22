//! Cross-platform `kb` directory links.
//!
//! A `kb` entry placed in a workspace folder points at the workspace's KB directory, so
//! documents in that folder can reach the knowledge base. The name is **visible** (plan
//! item 29 — a dotfile like the old `.looper` is routinely ignored by tooling, but this
//! link is meant to be discoverable). It is a POSIX **symlink** on Unix and a directory
//! **junction** on Windows (junctions need no admin / Developer Mode, unlike Windows
//! symlinks). Paths are compared with `dunce` canonicalization to avoid Windows UNC
//! (`\\?\`) mismatches.

use std::io;
use std::path::{Path, PathBuf};

use crate::WorkspaceError;

/// The visible link name placed inside a workspace folder (plan item 29).
pub const LINK_NAME: &str = "kb";

/// The pre-item-29 hidden link name, migrated to [`LINK_NAME`] by [`migrate_legacy`].
pub const LEGACY_LINK_NAME: &str = ".looper";

/// The state of a folder's `kb` link relative to an expected KB directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// The link exists and points at the expected KB directory.
    Linked,
    /// No link entry exists.
    Missing,
    /// The link points somewhere other than the expected KB directory.
    Mismatched,
    /// The link path exists but is a real file/directory (not our link).
    Occupied,
}

/// The path of the `kb` link inside `folder`.
#[must_use]
pub fn link_path(folder: &Path) -> PathBuf {
    folder.join(LINK_NAME)
}

/// Inspect the `kb` link in `folder` relative to `kb_dir`.
#[must_use]
pub fn state(folder: &Path, kb_dir: &Path) -> LinkState {
    state_named(folder, LINK_NAME, kb_dir)
}

/// Ensure `folder/kb` links to `kb_dir`, creating or repairing it as needed.
///
/// # Errors
/// Returns [`WorkspaceError::LinkOccupied`] if a real file/dir sits at the link path, or
/// an I/O error if creating/removing the link fails.
pub fn ensure(folder: &Path, kb_dir: &Path) -> Result<LinkState, WorkspaceError> {
    ensure_named(folder, LINK_NAME, kb_dir)
}

/// Remove `folder/kb` if it is our link. A no-op if it is missing; refuses to remove a
/// real file/directory.
///
/// # Errors
/// Returns [`WorkspaceError`] if the entry is occupied by a real file/dir, or removal fails.
pub fn remove(folder: &Path) -> Result<(), WorkspaceError> {
    remove_named(folder, LINK_NAME)
}

// --- generalized, name-parameterized links (item 49) ---------------------------------
//
// Item 49 links arbitrary targets (the KB *and* specs folders) into code-repo roots under
// configurable names, so the single-`kb` helpers above delegate to these.

/// Inspect a link named `name` inside `root` relative to `target`.
fn state_named(root: &Path, name: &str, target: &Path) -> LinkState {
    let link = root.join(name);
    let Ok(meta) = std::fs::symlink_metadata(&link) else {
        return LinkState::Missing;
    };
    if !is_dir_link(&link, &meta) {
        return LinkState::Occupied;
    }
    match read_link_target(&link) {
        Ok(t) if same_path(&t, target) => LinkState::Linked,
        Ok(_) => LinkState::Mismatched,
        Err(_) => LinkState::Occupied,
    }
}

/// Ensure `root/name` links to `target`, creating or repairing it as needed.
fn ensure_named(root: &Path, name: &str, target: &Path) -> Result<LinkState, WorkspaceError> {
    let link = root.join(name);
    match state_named(root, name, target) {
        LinkState::Linked => Ok(LinkState::Linked),
        LinkState::Missing => {
            create_dir_link(target, &link)?;
            Ok(LinkState::Linked)
        }
        LinkState::Mismatched => {
            remove_dir_link(&link)?;
            create_dir_link(target, &link)?;
            Ok(LinkState::Linked)
        }
        LinkState::Occupied => Err(WorkspaceError::LinkOccupied(link)),
    }
}

/// Remove `root/name` if it is our link (no-op if missing; refuses a real file/dir).
fn remove_named(root: &Path, name: &str) -> Result<(), WorkspaceError> {
    let link = root.join(name);
    match std::fs::symlink_metadata(&link) {
        Err(_) => Ok(()),
        Ok(meta) if is_dir_link(&link, &meta) => remove_dir_link(&link),
        Ok(_) => Err(WorkspaceError::LinkOccupied(link)),
    }
}

/// A generated link to (re)create in a code-repo root (item 49): its on-disk `name` and the
/// directory it `target`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredLink {
    /// The link's on-disk name in the repo root.
    pub name: String,
    /// The absolute directory the link points at (the KB or a specs folder).
    pub target: PathBuf,
}

/// What [`reconcile`] changed: the link names now present (for `.gitignore` add) and stale links
/// removed (for `.gitignore` cleanup + logging).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Names of the desired links that are now present (created, repaired, or already correct).
    pub linked: Vec<String>,
    /// Names of stale Looper-generated links removed because they're no longer desired.
    pub removed: Vec<String>,
}

/// Whether `repo_root` holds at least one Looper-generated link — a symlink/junction pointing at
/// one of `known_targets` (item 49). Drives the left-nav "linked?" checkbox regardless of how the
/// links are named or which policy is active.
#[must_use]
pub fn has_generated_link(repo_root: &Path, known_targets: &[PathBuf]) -> bool {
    let Ok(entries) = std::fs::read_dir(repo_root) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !is_dir_link(&path, &meta) {
            continue;
        }
        if let Ok(target) = read_link_target(&path) {
            if known_targets.iter().any(|known| same_path(&target, known)) {
                return true;
            }
        }
    }
    false
}

/// Reconcile the Looper-generated links in `repo_root` to exactly `desired` (item 49).
///
/// Creates or repairs every desired link, then removes any **Looper-generated** link whose target
/// is one of `known_targets` but is no longer desired — so policy changes (specs-only ↔ KB-only ↔
/// both ↔ off) and renames converge cleanly. A real file/dir, or a symlink pointing outside
/// `known_targets`, is **never** touched, so unrelated user files are safe.
///
/// `known_targets` should be the workspace's full set of linkable targets (the KB + every specs
/// folder) so a link dropped from the policy is recognizable as ours to remove.
///
/// # Errors
/// Returns [`WorkspaceError`] if creating/repairing/removing a link fails; a desired link whose
/// path is occupied by a real file/dir yields [`WorkspaceError::LinkOccupied`].
pub fn reconcile(
    repo_root: &Path,
    desired: &[DesiredLink],
    known_targets: &[PathBuf],
) -> Result<ReconcileOutcome, WorkspaceError> {
    for d in desired {
        ensure_named(repo_root, &d.name, &d.target)?;
    }
    let desired_names: std::collections::HashSet<&str> =
        desired.iter().map(|d| d.name.as_str()).collect();

    let mut removed = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_root) {
        for entry in entries.filter_map(Result::ok) {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if desired_names.contains(name.as_ref()) {
                continue; // a desired link we just ensured
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !is_dir_link(&path, &meta) {
                continue; // a real file/dir — never ours
            }
            let Ok(target) = read_link_target(&path) else {
                continue;
            };
            if known_targets.iter().any(|known| same_path(&target, known)) {
                remove_dir_link(&path)?;
                removed.push(name.into_owned());
            }
        }
    }

    Ok(ReconcileOutcome {
        linked: desired.iter().map(|d| d.name.clone()).collect(),
        removed,
    })
}

/// Migrate a pre-item-29 hidden `.looper` link to the visible `kb` name (plan item 29).
///
/// If `folder` holds a legacy `.looper` link **that we own**, it is removed; when it
/// pointed at `kb_dir`, the user's intent is preserved by creating the new `kb` link. A
/// `.looper` that is a real file/dir, or absent, is left untouched. Best-effort by design
/// — callers ignore the result (a failed migration just leaves the legacy link in place).
///
/// # Errors
/// Returns [`WorkspaceError`] if removing the legacy link or creating the new one fails.
pub fn migrate_legacy(folder: &Path, kb_dir: &Path) -> Result<(), WorkspaceError> {
    let legacy = folder.join(LEGACY_LINK_NAME);
    let Ok(meta) = std::fs::symlink_metadata(&legacy) else {
        return Ok(()); // nothing there
    };
    if !is_dir_link(&legacy, &meta) {
        return Ok(()); // a real `.looper` file/dir — not ours to touch
    }
    let pointed_at_kb = read_link_target(&legacy).is_ok_and(|target| same_path(&target, kb_dir));
    remove_dir_link(&legacy)?;
    if pointed_at_kb {
        ensure(folder, kb_dir)?;
    }
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// --- platform-specific link primitives ------------------------------------------------

#[cfg(unix)]
fn is_dir_link(_link: &Path, meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}
#[cfg(windows)]
fn is_dir_link(link: &Path, _meta: &std::fs::Metadata) -> bool {
    junction::exists(link).unwrap_or(false)
}

#[cfg(unix)]
fn read_link_target(link: &Path) -> io::Result<PathBuf> {
    std::fs::read_link(link)
}
#[cfg(windows)]
fn read_link_target(link: &Path) -> io::Result<PathBuf> {
    junction::get_target(link)
}

#[cfg(unix)]
fn create_dir_link(target: &Path, link: &Path) -> Result<(), WorkspaceError> {
    std::os::unix::fs::symlink(target, link).map_err(|source| WorkspaceError::Link {
        link: link.to_path_buf(),
        target: target.to_path_buf(),
        source,
    })
}
#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> Result<(), WorkspaceError> {
    junction::create(target, link).map_err(|source| WorkspaceError::Link {
        link: link.to_path_buf(),
        target: target.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn remove_dir_link(link: &Path) -> Result<(), WorkspaceError> {
    std::fs::remove_file(link).map_err(|source| WorkspaceError::Unlink {
        link: link.to_path_buf(),
        source,
    })
}
#[cfg(windows)]
fn remove_dir_link(link: &Path) -> Result<(), WorkspaceError> {
    std::fs::remove_dir(link).map_err(|source| WorkspaceError::Unlink {
        link: link.to_path_buf(),
        source,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn linking_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("repo");
        let kb = tmp.path().join("kb");
        let kb2 = tmp.path().join("kb2");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::create_dir_all(&kb2).unwrap();

        assert_eq!(state(&folder, &kb), LinkState::Missing);
        assert_eq!(ensure(&folder, &kb).unwrap(), LinkState::Linked);
        assert_eq!(state(&folder, &kb), LinkState::Linked);
        assert!(link_path(&folder).exists());

        // Idempotent.
        assert_eq!(ensure(&folder, &kb).unwrap(), LinkState::Linked);

        // Repair a mismatched link to a different KB dir.
        assert_eq!(state(&folder, &kb2), LinkState::Mismatched);
        assert_eq!(ensure(&folder, &kb2).unwrap(), LinkState::Linked);
        assert_eq!(state(&folder, &kb2), LinkState::Linked);

        // Remove.
        remove(&folder).unwrap();
        assert_eq!(state(&folder, &kb2), LinkState::Missing);
    }

    #[test]
    fn refuses_to_clobber_a_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("repo");
        let kb = tmp.path().join("kb");
        std::fs::create_dir_all(folder.join(LINK_NAME)).unwrap(); // a real `kb` dir
        std::fs::create_dir_all(&kb).unwrap();

        assert_eq!(state(&folder, &kb), LinkState::Occupied);
        assert!(matches!(
            ensure(&folder, &kb),
            Err(WorkspaceError::LinkOccupied(_))
        ));
    }

    #[test]
    fn migrates_legacy_looper_link_to_kb() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("repo");
        let kb = tmp.path().join("kb");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&kb).unwrap();

        // A pre-item-29 hidden link pointing at the KB.
        std::os::unix::fs::symlink(&kb, folder.join(LEGACY_LINK_NAME)).unwrap();

        migrate_legacy(&folder, &kb).unwrap();

        // The legacy link is gone; the visible `kb` link preserves the user's intent.
        assert!(std::fs::symlink_metadata(folder.join(LEGACY_LINK_NAME)).is_err());
        assert_eq!(state(&folder, &kb), LinkState::Linked);
    }

    #[test]
    fn migrate_leaves_a_real_dotfile_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("repo");
        let kb = tmp.path().join("kb");
        std::fs::create_dir_all(folder.join(LEGACY_LINK_NAME)).unwrap(); // a real .looper dir
        std::fs::create_dir_all(&kb).unwrap();

        migrate_legacy(&folder, &kb).unwrap();

        // A real `.looper` directory is not ours — untouched, and no `kb` link is created.
        assert!(folder.join(LEGACY_LINK_NAME).is_dir());
        assert_eq!(state(&folder, &kb), LinkState::Missing);
    }

    #[test]
    fn reconcile_creates_repairs_and_evicts_generated_links() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("code-repo");
        let kb = tmp.path().join("kb");
        let specs = tmp.path().join("specs");
        let elsewhere = tmp.path().join("elsewhere");
        for d in [&repo, &kb, &specs, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let known = vec![kb.clone(), specs.clone()];

        // A real user file + a user symlink to an unrelated dir must survive every reconcile.
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.join("mine")).unwrap();

        // Both: create a KB link ("kb") and a specs link ("specs").
        let out = reconcile(
            &repo,
            &[
                DesiredLink {
                    name: "kb".into(),
                    target: kb.clone(),
                },
                DesiredLink {
                    name: "specs".into(),
                    target: specs.clone(),
                },
            ],
            &known,
        )
        .unwrap();
        assert_eq!(out.linked, vec!["kb".to_string(), "specs".to_string()]);
        assert!(out.removed.is_empty());
        assert_eq!(state_named(&repo, "kb", &kb), LinkState::Linked);
        assert_eq!(state_named(&repo, "specs", &specs), LinkState::Linked);

        // KB-only: the specs link (a known target, no longer desired) is evicted; kb stays.
        let out = reconcile(
            &repo,
            &[DesiredLink {
                name: "kb".into(),
                target: kb.clone(),
            }],
            &known,
        )
        .unwrap();
        assert_eq!(out.removed, vec!["specs".to_string()]);
        assert_eq!(state_named(&repo, "kb", &kb), LinkState::Linked);
        assert!(std::fs::symlink_metadata(repo.join("specs")).is_err());

        // Rename the KB link kb → brain: the old name is evicted, the new one created.
        let out = reconcile(
            &repo,
            &[DesiredLink {
                name: "brain".into(),
                target: kb.clone(),
            }],
            &known,
        )
        .unwrap();
        assert_eq!(out.removed, vec!["kb".to_string()]);
        assert_eq!(state_named(&repo, "brain", &kb), LinkState::Linked);
        assert!(std::fs::symlink_metadata(repo.join("kb")).is_err());

        // Disable all: every generated link is removed.
        let out = reconcile(&repo, &[], &known).unwrap();
        assert_eq!(out.removed, vec!["brain".to_string()]);

        // The user's file + unrelated symlink are untouched throughout.
        assert!(repo.join("README.md").is_file());
        assert!(std::fs::symlink_metadata(repo.join("mine"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
