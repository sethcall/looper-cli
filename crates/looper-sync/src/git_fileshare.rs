//! The GitFileshare sync backend — "use git as a fileshare" (plan items 30.04 + 30.11).
//!
//! Drives the configured `git` binary for all mutations (fetch/merge/push/checkout) — gix's
//! push/merge support is immature — while `looper-git` (gix) supplies the read-only repo probe.
//! The binary is resolved from a custom path or `PATH` via [`GitLocator`], held behind a lock so
//! the app can change it live. Commands run with `GIT_TERMINAL_PROMPT=0` so a missing credential
//! fails fast instead of hanging; authentication relies on the user's existing git config /
//! credential helper / SSH.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use looper_git::{open_repo, HeadState};
use looper_ipc::{
    ConflictFile, GitInfo, SyncDirection, SyncFolderConfig, SyncResolution, SyncStrategy,
    SyncTestReport,
};

use crate::{SyncBackend, SyncError, SyncOutcome, SyncProbe};

/// The git binary Auto Sync drives (plan item 30.11). Resolved from a custom path or `PATH`,
/// held behind a lock so the app can change it without a restart.
pub struct GitLocator {
    bin: Mutex<PathBuf>,
}

impl GitLocator {
    /// Wrap an explicit binary path.
    #[must_use]
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin: Mutex::new(bin),
        }
    }

    /// Resolve from a custom path (if non-empty + present) or `PATH`, falling back to "git".
    #[must_use]
    pub fn resolved(custom: Option<&str>) -> Self {
        Self::new(resolve_git(custom).unwrap_or_else(|| PathBuf::from("git")))
    }

    /// The current binary path.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.bin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Change the binary (a custom path, or re-detect on `PATH` when `custom` is `None`).
    pub fn set(&self, custom: Option<&str>) {
        let resolved = resolve_git(custom).unwrap_or_else(|| PathBuf::from("git"));
        *self.bin.lock().unwrap_or_else(PoisonError::into_inner) = resolved;
    }
}

/// Resolve the git binary: a custom path (validated to exist), else the first `git` on `PATH`.
#[must_use]
pub fn resolve_git(custom: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = custom {
        if !path.trim().is_empty() {
            let candidate = PathBuf::from(path);
            return candidate.is_file().then_some(candidate);
        }
    }
    let exe = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(exe))
            .find(|candidate| candidate.is_file())
    })
}

/// Resolve git + read its version (plan item 30.11). `None` if no runnable git is found.
#[must_use]
pub fn detect_git(custom: Option<&str>) -> Option<GitInfo> {
    let path = resolve_git(custom)?;
    let version = Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())?;
    Some(GitInfo {
        path: path.to_string_lossy().into_owned(),
        version,
    })
}

/// Auto-sync a folder by treating its git repository as a fileshare.
pub struct GitFileshareBackend {
    git: Arc<GitLocator>,
}

impl GitFileshareBackend {
    /// Build a backend over a (shared, live) git binary locator.
    #[must_use]
    pub fn new(git: Arc<GitLocator>) -> Self {
        Self { git }
    }

    fn ctx<'a>(&self, bin: &'a Path, folder: &'a Path) -> GitCtx<'a> {
        GitCtx { bin, dir: folder }
    }
}

impl SyncBackend for GitFileshareBackend {
    fn strategy(&self) -> SyncStrategy {
        SyncStrategy::Git
    }

    fn probe(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError> {
        let bin = self.git.current();
        probe(&self.ctx(&bin, folder), folder, cfg)
    }

    fn sync(&self, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError> {
        let bin = self.git.current();
        sync(&self.ctx(&bin, folder), folder, cfg)
    }

    fn resolve(
        &self,
        folder: &Path,
        cfg: &SyncFolderConfig,
        how: SyncResolution,
    ) -> Result<SyncOutcome, SyncError> {
        let bin = self.git.current();
        resolve(&self.ctx(&bin, folder), folder, cfg, how)
    }
}

/// A git command context: the resolved binary + the working directory.
struct GitCtx<'a> {
    bin: &'a Path,
    dir: &'a Path,
}

impl GitCtx<'_> {
    fn raw(&self, args: &[&str]) -> Result<Output, SyncError> {
        Command::new(self.bin)
            .current_dir(self.dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(SyncError::Io)
    }

    fn run(&self, args: &[&str]) -> Result<String, SyncError> {
        let out = self.raw(args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(self.classify(args, &out))
        }
    }

    fn classify(&self, args: &[&str], out: &Output) -> SyncError {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if is_auth_error(&stderr) {
            SyncError::Auth {
                folder: self.dir.to_path_buf(),
                stderr,
            }
        } else {
            SyncError::GitCli {
                args: args.join(" "),
                code: out.status.code(),
                stderr,
            }
        }
    }

    /// The upstream tracking ref for `branch` (e.g. "origin/main"), if configured.
    fn upstream_ref(&self, branch: &str) -> Option<String> {
        let spec = format!("{branch}@{{upstream}}");
        self.run(&["rev-parse", "--abbrev-ref", &spec])
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn ahead_behind(&self, branch: &str, upstream: &str) -> Result<(u32, u32), SyncError> {
        let range = format!("{branch}...{upstream}");
        let out = self.run(&["rev-list", "--left-right", "--count", &range])?;
        let mut nums = out.split_whitespace().filter_map(|n| n.parse::<u32>().ok());
        Ok((nums.next().unwrap_or(0), nums.next().unwrap_or(0)))
    }

    fn conflicted_paths(&self) -> Result<Vec<String>, SyncError> {
        let out = self.run(&["diff", "--name-only", "--diff-filter=U"])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    fn has_staged_changes(&self) -> Result<bool, SyncError> {
        Ok(!self.raw(&["diff", "--cached", "--quiet"])?.status.success())
    }
}

// ---- the sync cycle ----------------------------------------------------------------

fn probe(ctx: &GitCtx, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncProbe, SyncError> {
    let Ok(repo) = open_repo(folder) else {
        return Ok(SyncProbe {
            manageable: false,
            has_remote: false,
            branch: None,
            ahead: 0,
            behind: 0,
            conflicted: false,
            detail: Some("not a git repository".to_string()),
        });
    };
    let branch = branch_name(repo.head);
    let has_remote = !ctx.run(&["remote"]).unwrap_or_default().is_empty();
    let (ahead, behind) = match ctx.upstream_ref(&cfg.branch) {
        Some(up) => ctx.ahead_behind(&cfg.branch, &up).unwrap_or((0, 0)),
        None => (0, 0),
    };
    let conflicted = !ctx.conflicted_paths().unwrap_or_default().is_empty();
    Ok(SyncProbe {
        manageable: true,
        has_remote,
        branch,
        ahead,
        behind,
        conflicted,
        detail: None,
    })
}

/// Confirm `folder` is a repo on `cfg.branch` with an upstream; return the remote name.
fn preflight(ctx: &GitCtx, folder: &Path, cfg: &SyncFolderConfig) -> Result<String, SyncError> {
    let repo = open_repo(folder).map_err(|_| SyncError::NotARepo(folder.to_path_buf()))?;
    let current = branch_name(repo.head).ok_or_else(|| SyncError::NotOnSyncBranch {
        expected: cfg.branch.clone(),
        actual: "detached or unborn HEAD".to_string(),
    })?;
    if current != cfg.branch {
        return Err(SyncError::NotOnSyncBranch {
            expected: cfg.branch.clone(),
            actual: current,
        });
    }
    let upstream = ctx
        .upstream_ref(&cfg.branch)
        .ok_or_else(|| SyncError::NoRemote {
            folder: folder.to_path_buf(),
        })?;
    Ok(upstream
        .split_once('/')
        .map_or_else(|| "origin".to_string(), |(remote, _)| remote.to_string()))
}

fn sync(ctx: &GitCtx, folder: &Path, cfg: &SyncFolderConfig) -> Result<SyncOutcome, SyncError> {
    let remote = preflight(ctx, folder, cfg)?;
    // An unresolved in-progress merge means we're already paused on a conflict — report it
    // (don't commit over the markers). Makes sync idempotent-safe + survives restart.
    let existing = ctx.conflicted_paths()?;
    if !existing.is_empty() {
        return Ok(SyncOutcome::Conflict {
            files: existing.into_iter().map(conflict_file).collect(),
        });
    }
    let upstream = format!("{remote}/{}", cfg.branch);
    match cfg.direction {
        SyncDirection::PullOnly => pull_only(ctx, &cfg.branch, &remote, &upstream),
        SyncDirection::PullPush => pull_push(ctx, &cfg.branch, &remote, &upstream),
    }
}

fn pull_push(
    ctx: &GitCtx,
    branch: &str,
    remote: &str,
    upstream: &str,
) -> Result<SyncOutcome, SyncError> {
    ctx.run(&["add", "-A"])?;
    let committed = if ctx.has_staged_changes()? {
        ctx.run(&["commit", "--no-edit", "-m", &commit_message()])?;
        true
    } else {
        false
    };

    ctx.run(&["fetch", remote, branch])?;

    let (ahead, behind) = ctx.ahead_behind(branch, upstream)?;
    let mut pulled = false;
    if behind > 0 {
        if ahead == 0 {
            ctx.run(&["merge", "--ff-only", upstream])?;
        } else {
            let out = ctx.raw(&["merge", "--no-edit", upstream])?;
            if !out.status.success() {
                let conflicts = ctx.conflicted_paths()?;
                if !conflicts.is_empty() {
                    return Ok(SyncOutcome::Conflict {
                        files: conflicts.into_iter().map(conflict_file).collect(),
                    });
                }
                return Err(ctx.classify(&["merge", "--no-edit", upstream], &out));
            }
        }
        pulled = true;
    }

    let (ahead_now, _) = ctx.ahead_behind(branch, upstream)?;
    let pushed = ahead_now > 0 && push(ctx, remote, branch)?;

    if pulled || pushed || committed {
        Ok(SyncOutcome::Updated { pulled, pushed })
    } else {
        Ok(SyncOutcome::UpToDate)
    }
}

fn pull_only(
    ctx: &GitCtx,
    branch: &str,
    remote: &str,
    upstream: &str,
) -> Result<SyncOutcome, SyncError> {
    ctx.run(&["fetch", remote, branch])?;
    let (_, behind) = ctx.ahead_behind(branch, upstream)?;
    if behind == 0 {
        return Ok(SyncOutcome::UpToDate);
    }
    let out = ctx.raw(&["merge", "--ff-only", upstream])?;
    if out.status.success() {
        Ok(SyncOutcome::Updated {
            pulled: true,
            pushed: false,
        })
    } else {
        Err(SyncError::GitCli {
            args: "merge --ff-only".to_string(),
            code: out.status.code(),
            stderr: "pull-only: local changes block a fast-forward — commit/push or stash them"
                .to_string(),
        })
    }
}

/// Push `branch` to `remote`. A non-fast-forward rejection is benign (`false`); auth/other fail.
fn push(ctx: &GitCtx, remote: &str, branch: &str) -> Result<bool, SyncError> {
    let out = ctx.raw(&["push", remote, branch])?;
    if out.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_auth_error(&stderr) {
        return Err(SyncError::Auth {
            folder: ctx.dir.to_path_buf(),
            stderr: stderr.into_owned(),
        });
    }
    if stderr.contains("non-fast-forward")
        || stderr.contains("rejected")
        || stderr.contains("fetch first")
    {
        Ok(false)
    } else {
        Err(ctx.classify(&["push", remote, branch], &out))
    }
}

fn resolve(
    ctx: &GitCtx,
    folder: &Path,
    cfg: &SyncFolderConfig,
    how: SyncResolution,
) -> Result<SyncOutcome, SyncError> {
    let remote = preflight(ctx, folder, cfg)?;
    let conflicts = ctx.conflicted_paths()?;
    match how {
        SyncResolution::UseMine => {
            for path in &conflicts {
                ctx.run(&["checkout", "--ours", "--", path])?;
            }
        }
        SyncResolution::UseTheirs => {
            for path in &conflicts {
                ctx.run(&["checkout", "--theirs", "--", path])?;
            }
        }
        SyncResolution::MarkResolved => {
            ctx.run(&["add", "-A"])?;
            if !ctx.conflicted_paths()?.is_empty() {
                return Err(unresolved_error());
            }
            if !ctx.raw(&["diff", "--check"])?.status.success() {
                return Err(unresolved_error());
            }
        }
    }
    ctx.run(&["add", "-A"])?;
    ctx.run(&["commit", "--no-edit"])?;
    let pushed = push(ctx, &remote, &cfg.branch)?;
    Ok(SyncOutcome::Updated {
        pulled: true,
        pushed,
    })
}

// ---- the per-folder Test (plan item 30.11; safe, read-only) -------------------------

/// Run safe, read-only checks on a folder: git runs, it's a repo, `git status` works, and (when
/// a remote is configured) `git ls-remote` reaches it. Never mutates the working tree.
#[must_use]
pub fn git_test(git_bin: &Path, folder: &Path, cfg: &SyncFolderConfig) -> SyncTestReport {
    let mut report = SyncTestReport {
        ok: false,
        git: None,
        is_repo: false,
        branch: None,
        has_remote: false,
        remote_reachable: None,
        summary: String::new(),
    };

    // 1. git runs.
    let version = match Command::new(git_bin).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            report.summary = format!("git not found / not runnable at {}", git_bin.display());
            return report;
        }
    };
    report.git = Some(GitInfo {
        path: git_bin.to_string_lossy().into_owned(),
        version,
    });

    // 2. is a git repository?
    let ctx = GitCtx {
        bin: git_bin,
        dir: folder,
    };
    match open_repo(folder) {
        Ok(repo) => {
            report.is_repo = true;
            report.branch = branch_name(repo.head);
        }
        Err(_) => {
            report.summary = "not a git repository — run `git init` here to enable Git sync".into();
            return report;
        }
    }

    // 3. git status works (read-only).
    if let Err(error) = ctx.run(&["status", "--porcelain"]) {
        report.summary = format!("`git status` failed: {error}");
        return report;
    }

    // 4. upstream + ls-remote reachability (read-only; only the repo's own remote).
    let upstream = ctx.upstream_ref(&cfg.branch);
    report.has_remote = upstream.is_some();
    if let Some(up) = &upstream {
        let remote = up.split_once('/').map_or("origin", |(remote, _)| remote);
        let reachable = matches!(
            ctx.raw(&["ls-remote", "--heads", remote, &cfg.branch]),
            Ok(out) if out.status.success()
        );
        report.remote_reachable = Some(reachable);
    }

    report.ok = report.is_repo && report.has_remote && report.remote_reachable != Some(false);
    report.summary = summarize(&report);
    report
}

fn summarize(report: &SyncTestReport) -> String {
    let mut parts = Vec::new();
    if let Some(branch) = &report.branch {
        parts.push(format!("on `{branch}`"));
    }
    parts.push(
        if report.has_remote {
            "upstream configured"
        } else {
            "no upstream — push the branch first"
        }
        .to_string(),
    );
    match report.remote_reachable {
        Some(true) => parts.push("remote reachable".to_string()),
        Some(false) => parts.push("remote NOT reachable (network or auth)".to_string()),
        None => {}
    }
    let lead = if report.ok {
        "Ready to sync"
    } else {
        "Not ready"
    };
    format!("{lead} — {}.", parts.join(", "))
}

// ---- shared helpers ----------------------------------------------------------------

fn branch_name(head: HeadState) -> Option<String> {
    match head {
        HeadState::Branch(b) => Some(b),
        HeadState::Detached(_) | HeadState::Unborn => None,
    }
}

fn is_auth_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("authentication failed")
        || s.contains("could not read username")
        || s.contains("permission denied")
        || s.contains("publickey")
        || s.contains("invalid username or password")
}

fn commit_message() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("looper auto-sync @ {secs}")
}

fn conflict_file(path: String) -> ConflictFile {
    ConflictFile { path, detail: None }
}

fn unresolved_error() -> SyncError {
    SyncError::GitCli {
        args: "resolve".to_string(),
        code: None,
        stderr: "unresolved conflicts remain — fix the marked files first".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A backend over the system `git` on PATH.
    fn backend() -> GitFileshareBackend {
        GitFileshareBackend::new(Arc::new(GitLocator::new(PathBuf::from("git"))))
    }

    /// Run a git command with a fixed identity, asserting success.
    fn g(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "t@e.st")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "t@e.st")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn config_identity(dir: &Path) {
        g(dir, &["config", "user.name", "Test"]);
        g(dir, &["config", "user.email", "t@e.st"]);
    }

    fn cfg(folder: &Path, direction: SyncDirection) -> SyncFolderConfig {
        SyncFolderConfig {
            folder: folder.to_string_lossy().into_owned(),
            is_kb: false,
            is_git_repo: true,
            enabled: true,
            strategy: SyncStrategy::Git,
            branch: "main".to_string(),
            direction,
            dolt: None,
        }
    }

    /// A bare remote + two clones (a, b) sharing `main` with one initial commit.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        g(
            tmp.path(),
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        g(
            tmp.path(),
            &["clone", remote.to_str().unwrap(), a.to_str().unwrap()],
        );
        config_identity(&a);
        fs::write(a.join("readme.md"), "base\n").unwrap();
        g(&a, &["add", "-A"]);
        g(&a, &["commit", "-m", "init"]);
        g(&a, &["push", "-u", "origin", "main"]);
        g(
            tmp.path(),
            &["clone", remote.to_str().unwrap(), b.to_str().unwrap()],
        );
        config_identity(&b);
        (tmp, a, b)
    }

    #[test]
    fn resolve_git_finds_git_on_path() {
        assert!(resolve_git(None).is_some(), "git should resolve on PATH");
        assert!(resolve_git(Some("/definitely/not/git")).is_none());
        assert!(detect_git(None).is_some());
    }

    #[test]
    fn probe_reports_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = backend()
            .probe(tmp.path(), &cfg(tmp.path(), SyncDirection::PullPush))
            .unwrap();
        assert!(!probe.manageable);
    }

    #[test]
    fn pulls_remote_changes() {
        let (_tmp, a, b) = setup();
        fs::write(b.join("readme.md"), "base\nfrom-b\n").unwrap();
        g(&b, &["commit", "-am", "b change"]);
        g(&b, &["push"]);

        let out = backend()
            .sync(&a, &cfg(&a, SyncDirection::PullPush))
            .unwrap();
        assert!(matches!(out, SyncOutcome::Updated { pulled: true, .. }));
        assert!(fs::read_to_string(a.join("readme.md"))
            .unwrap()
            .contains("from-b"));
    }

    #[test]
    fn commits_and_pushes_local_changes() {
        let (_tmp, a, b) = setup();
        fs::write(a.join("note.md"), "hello\n").unwrap();

        let out = backend()
            .sync(&a, &cfg(&a, SyncDirection::PullPush))
            .unwrap();
        assert!(matches!(out, SyncOutcome::Updated { pushed: true, .. }));

        backend()
            .sync(&b, &cfg(&b, SyncDirection::PullPush))
            .unwrap();
        assert!(b.join("note.md").exists());
    }

    #[test]
    fn detects_and_resolves_conflict_use_mine() {
        let (_tmp, a, b) = setup();
        fs::write(a.join("readme.md"), "base\nA-version\n").unwrap();
        fs::write(b.join("readme.md"), "base\nB-version\n").unwrap();
        g(&b, &["commit", "-am", "b"]);
        g(&b, &["push"]);

        let c = cfg(&a, SyncDirection::PullPush);
        match backend().sync(&a, &c).unwrap() {
            SyncOutcome::Conflict { files } => {
                assert!(files.iter().any(|f| f.path.contains("readme.md")));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }

        backend().resolve(&a, &c, SyncResolution::UseMine).unwrap();
        assert!(fs::read_to_string(a.join("readme.md"))
            .unwrap()
            .contains("A-version"));

        backend()
            .sync(&b, &cfg(&b, SyncDirection::PullPush))
            .unwrap();
        assert!(fs::read_to_string(b.join("readme.md"))
            .unwrap()
            .contains("A-version"));
    }

    #[test]
    fn pull_only_never_pushes() {
        let (_tmp, a, b) = setup();
        fs::write(a.join("local.md"), "local\n").unwrap();
        let out = backend()
            .sync(&a, &cfg(&a, SyncDirection::PullOnly))
            .unwrap();
        assert!(matches!(
            out,
            SyncOutcome::UpToDate | SyncOutcome::Updated { pushed: false, .. }
        ));
        backend()
            .sync(&b, &cfg(&b, SyncDirection::PullPush))
            .unwrap();
        assert!(!b.join("local.md").exists());
    }

    #[test]
    fn git_test_reports_repo_and_remote() {
        let (_tmp, a, _b) = setup();
        let report = git_test(Path::new("git"), &a, &cfg(&a, SyncDirection::PullPush));
        assert!(report.git.is_some());
        assert!(report.is_repo);
        assert_eq!(report.branch.as_deref(), Some("main"));
        assert!(report.has_remote);
        assert_eq!(report.remote_reachable, Some(true)); // the bare remote is on disk
        assert!(report.ok, "expected ok, got: {}", report.summary);
    }

    #[test]
    fn git_test_flags_a_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let report = git_test(
            Path::new("git"),
            tmp.path(),
            &cfg(tmp.path(), SyncDirection::PullPush),
        );
        assert!(!report.is_repo);
        assert!(!report.ok);
    }
}
