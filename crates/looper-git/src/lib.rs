//! `looper-git` — git repository discovery & tracking for Looper.
//!
//! Discovers git repositories under a set of root directories and produces a
//! [`TrackedRepo`] model (work dir, git dir, name, HEAD state) using pure-Rust `gix`
//! (so installers stay free of system libgit2). History/blame for the contributors
//! milestone, and a clean/dirty signal, come later.
//!
//! Dependency rule: **leaf** crate. See `../../AGENTS.md`.
//! Implements plan item 09 (`../../specs/plan/09-git-discovery.md`).

mod error;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use error::GitError;

/// Maximum directory recursion depth while discovering repositories.
const MAX_DEPTH: usize = 64;

/// The state of a repository's HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD points at a branch (short name, e.g. `main`).
    Branch(String),
    /// Detached HEAD at the given short commit id.
    Detached(String),
    /// A repository with no commits yet (unborn branch).
    Unborn,
}

/// A discovered, tracked git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRepo {
    /// The working directory of the repository.
    pub work_dir: PathBuf,
    /// The `.git` directory.
    pub git_dir: PathBuf,
    /// A display name (the work dir's file name).
    pub name: String,
    /// The current HEAD state.
    pub head: HeadState,
}

/// Discover all git repositories under `roots`.
///
/// Walks each root, treating any directory that contains a `.git` entry as a
/// repository (and not descending into it). Skips `node_modules`, `target`, hidden
/// directories, and symlinks. Repositories are de-duplicated by canonical path.
#[must_use]
pub fn discover_repos(roots: &[PathBuf]) -> Vec<TrackedRepo> {
    let mut repos = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        discover_into(root, &mut repos, &mut seen, 0);
    }
    repos
}

fn discover_into(
    dir: &Path,
    repos: &mut Vec<TrackedRepo>,
    seen: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }

    if dir.join(".git").exists() {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if seen.insert(canonical) {
            if let Ok(repo) = open_repo(dir) {
                repos.push(repo);
            }
        }
        return; // a repository's contents belong to it; do not descend
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `file_type` does not follow symlinks, so symlinked dirs report `is_dir() == false`.
        if file_type.is_dir() && !is_pruned(&entry.file_name()) {
            discover_into(&entry.path(), repos, seen, depth + 1);
        }
    }
}

fn is_pruned(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name == "node_modules" || name == "target" || name.starts_with('.')
}

/// Open the repository at `work_dir` and read its tracked state.
///
/// # Errors
/// Returns [`GitError`] if the repository cannot be opened or HEAD cannot be read.
pub fn open_repo(work_dir: &Path) -> Result<TrackedRepo, GitError> {
    let repo = gix::open(work_dir).map_err(|source| GitError::Open {
        path: work_dir.to_path_buf(),
        source: Box::new(source),
    })?;

    let resolved_work_dir = repo
        .workdir()
        .map_or_else(|| work_dir.to_path_buf(), Path::to_path_buf);
    let git_dir = repo.git_dir().to_path_buf();
    let name = resolved_work_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let head = head_state(&repo, work_dir)?;

    Ok(TrackedRepo {
        work_dir: resolved_work_dir,
        git_dir,
        name,
        head,
    })
}

/// Derive a repository's **true name** from its `origin` remote URL (item 49) — the name the repo
/// is known by upstream, e.g. `git@github.com:acme/widgets.git` → `widgets`. Returns `None` when
/// `work_dir` isn't a repo, has no `origin` remote, or the URL yields no usable name; callers then
/// fall back to the local directory name.
#[must_use]
pub fn repo_true_name(work_dir: &Path) -> Option<String> {
    use gix::bstr::ByteSlice;
    let repo = gix::open(work_dir).ok()?;
    let snapshot = repo.config_snapshot();
    let url = snapshot.string("remote.origin.url")?;
    repo_name_from_remote_url(url.to_str().ok()?)
}

/// Parse the repository name from a git remote URL (item 49): the final path segment with a trailing
/// `/` and `.git` stripped. Handles HTTPS, `ssh://`, `file://`, and scp-like SSH
/// (`git@host:owner/repo`). Pure + fully testable.
#[must_use]
pub fn repo_name_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    // The name is the last segment, after the final `/` (URL path) or `:` (scp-like SSH host:path).
    let name = trimmed.rsplit(['/', ':']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn head_state(repo: &gix::Repository, path: &Path) -> Result<HeadState, GitError> {
    let head = repo.head().map_err(|source| GitError::Head {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;

    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            HeadState::Branch(reference.name.shorten().to_string())
        }
        gix::head::Kind::Detached { target, .. } => {
            HeadState::Detached(target.to_hex_with_len(7).to_string())
        }
        gix::head::Kind::Unborn(_) => HeadState::Unborn,
    })
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

    fn init_repo_with_commit(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-b", "main"]);
        run_git(
            path,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "init",
            ],
        );
    }

    #[test]
    fn discovers_repos_skips_non_repos_and_reads_head() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo_with_commit(&root.join("repo-a"));
        init_repo_with_commit(&root.join("nested/repo-b"));
        std::fs::create_dir_all(root.join("not-a-repo")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();

        let repos = discover_repos(&[root.to_path_buf()]);
        let names: HashSet<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains("repo-a"));
        assert!(names.contains("repo-b"), "should find nested repos");
        assert!(!names.contains("not-a-repo"));
        assert!(!names.contains("pkg"), "node_modules should be pruned");

        let a = repos.iter().find(|r| r.name == "repo-a").unwrap();
        assert_eq!(a.head, HeadState::Branch("main".to_string()));
    }

    #[test]
    fn open_repo_on_non_repo_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(open_repo(tmp.path()).is_err());
    }

    #[test]
    fn fresh_repo_has_unborn_head() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fresh");
        std::fs::create_dir_all(&path).unwrap();
        run_git(&path, &["init", "-b", "main"]);
        let repo = open_repo(&path).expect("open fresh repo");
        assert_eq!(repo.head, HeadState::Unborn);
    }

    #[test]
    fn repo_name_from_remote_url_handles_url_forms() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://github.com/acme/widgets.git", Some("widgets")),
            ("https://github.com/acme/widgets", Some("widgets")),
            ("git@github.com:acme/widgets.git", Some("widgets")),
            ("git@github.com:acme/widgets", Some("widgets")),
            ("ssh://git@github.com/acme/widgets.git", Some("widgets")),
            (
                "https://example.com/a/b/c/deep-repo.git/",
                Some("deep-repo"),
            ),
            ("file:///srv/git/local-repo.git", Some("local-repo")),
            ("", None),
            ("/", None),
        ];
        for (url, want) in cases {
            assert_eq!(
                repo_name_from_remote_url(url).as_deref(),
                *want,
                "url={url}"
            );
        }
    }

    #[test]
    fn repo_true_name_reads_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("widgets-local");
        init_repo_with_commit(&path);
        // No remote yet → None, so callers fall back to the local directory name.
        assert_eq!(repo_true_name(&path), None);

        run_git(
            &path,
            &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
        );
        assert_eq!(repo_true_name(&path).as_deref(), Some("widgets"));
    }
}
