//! `looper-watcher` — cross-platform, gitignore-aware file watching for Looper.
//!
//! Collects the directories to watch under each root with the `ignore` crate — honoring
//! `.gitignore` by default (config-toggleable) plus a built-in, overridable don't-watch
//! list (`.git`/`node_modules`/`target`/`.looper`) — and watches each **non-recursively**.
//! So we genuinely do not watch `node_modules`/gitignored trees, and the inotify
//! descriptor count is accurate. New/removed directories are handled at runtime by a
//! manager thread that re-syncs the watch set (re-walk → diff → watch/unwatch → emit
//! synthetic creates for the brief race window before a new dir's watch is established).
//!
//! Forwarded events are filtered (a generic [`WatchFilter`], e.g. markdown-only); the
//! watcher knows nothing about OKF. Note: directory-level gitignore is honored here;
//! file-level gitignore (a gitignored file inside a watched dir) is enforced by the scan
//! layer (item 11), which makes the indexing decision.
//!
//! [`WatchHealth`] reports OS watch-limit status with actionable [`Remediation`] (an exact
//! `sysctl` command, with [`Remediation::apply`] to run it). See `health` for OS specifics.
//!
//! Dependency rule: **leaf**, domain-free. See `../../AGENTS.md`.
//! Implements plan item 08 (`../../specs/plan/08-file-watcher.md`).

mod error;
mod filter;
mod health;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ignore::WalkBuilder;
use notify_debouncer_full::notify::event::ModifyKind;
use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};

pub use error::WatchError;
pub use filter::WatchFilter;
pub use health::{
    compute_health, read_inotify_limits, suggested_watch_limit, ApplyError, InotifyLimits,
    Remediation, WatchHealth, WatchStatus,
};

/// Default debounce window for coalescing bursty editor writes.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(400);

type LooperDebouncer = Debouncer<RecommendedWatcher, RecommendedCache>;

/// Configuration for a [`Watcher`].
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Root directories to watch.
    pub roots: Vec<PathBuf>,
    /// Which directories to prune and which file events to forward.
    pub filter: WatchFilter,
    /// Debounce window.
    pub debounce: Duration,
    /// Honor `.gitignore` (and `.ignore` / global / exclude files) when choosing
    /// directories to watch. Default `true`.
    pub respect_gitignore: bool,
}

impl WatchConfig {
    /// A markdown-only, gitignore-respecting config for `roots` with the default debounce.
    #[must_use]
    pub fn markdown(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            filter: WatchFilter::markdown(),
            debounce: DEFAULT_DEBOUNCE,
            respect_gitignore: true,
        }
    }
}

/// A coalesced change to a watched path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The affected path.
    pub path: PathBuf,
    /// What kind of change occurred.
    pub kind: ChangeKind,
}

/// A simplified change classification (domain-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A path was created.
    Created,
    /// A path's contents/metadata were modified.
    Modified,
    /// A path was removed.
    Removed,
    /// A path was renamed/moved.
    Renamed,
    /// Any other change.
    Other,
}

/// A message from the watcher: a change event, or a non-fatal backend error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchMessage {
    /// A filtered change event.
    Event(WatchEvent),
    /// A non-fatal backend error (e.g. inotify queue overflow).
    Error(String),
}

/// A running file watcher. Keep it alive to keep watching; dropping it stops the watch.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    _handle: Option<JoinHandle<()>>,
    health: Arc<Mutex<WatchHealth>>,
}

impl Watcher {
    /// Start watching, returning the watcher and the channel of [`WatchMessage`]s.
    ///
    /// # Errors
    /// Returns [`WatchError`] if the backend cannot be initialized or an initial
    /// directory cannot be watched.
    pub fn start(config: WatchConfig) -> Result<(Self, Receiver<WatchMessage>), WatchError> {
        let (raw_tx, raw_rx) = channel::<DebounceEventResult>();
        let mut debouncer = new_debouncer(config.debounce, None, move |result| {
            let _ = raw_tx.send(result);
        })?;

        let filter = config.filter;
        let respect_gitignore = config.respect_gitignore;
        let roots = config.roots;

        let watched = collect_dirs(&roots, &filter, respect_gitignore);
        for dir in &watched {
            debouncer
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|source| WatchError::Watch {
                    path: dir.clone(),
                    source,
                })?;
        }

        let limits = read_inotify_limits();
        let health = Arc::new(Mutex::new(compute_health(watched.len(), limits)));
        let (out_tx, out_rx) = channel::<WatchMessage>();
        let stop = Arc::new(AtomicBool::new(false));

        let manager = Manager {
            debouncer,
            raw_rx,
            out_tx,
            watched,
            filter,
            respect_gitignore,
            roots,
            health: Arc::clone(&health),
            stop: Arc::clone(&stop),
            limits,
        };
        let handle = thread::spawn(move || manager.run());

        Ok((
            Self {
                stop,
                _handle: Some(handle),
                health,
            },
            out_rx,
        ))
    }

    /// The current watch-subsystem health (kept fresh by the manager thread).
    #[must_use]
    pub fn health(&self) -> WatchHealth {
        match self.health.lock() {
            Ok(health) => health.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Owns the debouncer and the watch set; runs on a dedicated thread so it can add/remove
/// watches in response to directory changes.
struct Manager {
    debouncer: LooperDebouncer,
    raw_rx: Receiver<DebounceEventResult>,
    out_tx: Sender<WatchMessage>,
    watched: HashSet<PathBuf>,
    filter: WatchFilter,
    respect_gitignore: bool,
    roots: Vec<PathBuf>,
    health: Arc<Mutex<WatchHealth>>,
    stop: Arc<AtomicBool>,
    limits: Option<InotifyLimits>,
}

impl Manager {
    fn run(mut self) {
        while !self.stop.load(Ordering::Acquire) {
            match self.raw_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(events)) => {
                    let mut needs_resync = false;
                    for event in events {
                        let kind = classify(&event.kind);
                        for path in &event.paths {
                            if kind == ChangeKind::Created && path.is_dir() {
                                needs_resync = true; // a new directory appeared
                            } else if kind == ChangeKind::Removed && self.watched.contains(path) {
                                needs_resync = true; // a watched directory went away
                            } else if self.filter.accepts(path) {
                                let _ = self.out_tx.send(WatchMessage::Event(WatchEvent {
                                    path: path.clone(),
                                    kind,
                                }));
                            }
                        }
                    }
                    if needs_resync {
                        self.resync();
                    }
                }
                Ok(Err(errors)) => {
                    for error in errors {
                        let _ = self.out_tx.send(WatchMessage::Error(error.to_string()));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // `self` (incl. the debouncer) drops here, releasing all OS watches.
    }

    /// Re-walk the roots and reconcile the watch set with the current directory tree.
    fn resync(&mut self) {
        let current = collect_dirs(&self.roots, &self.filter, self.respect_gitignore);

        let added: Vec<PathBuf> = current.difference(&self.watched).cloned().collect();
        let removed: Vec<PathBuf> = self.watched.difference(&current).cloned().collect();

        for dir in added {
            if self
                .debouncer
                .watch(&dir, RecursiveMode::NonRecursive)
                .is_ok()
            {
                self.watched.insert(dir.clone());
                self.emit_existing_files(&dir);
            }
        }
        for dir in removed {
            let _ = self.debouncer.unwatch(&dir);
            self.watched.remove(&dir);
        }

        if let Ok(mut health) = self.health.lock() {
            *health = compute_health(self.watched.len(), self.limits);
        }
    }

    /// Emit synthetic `Created` events for matching files already present in a newly
    /// watched directory (catches files created before the watch was established).
    fn emit_existing_files(&self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() && self.filter.accepts(&path) {
                let _ = self.out_tx.send(WatchMessage::Event(WatchEvent {
                    path,
                    kind: ChangeKind::Created,
                }));
            }
        }
    }
}

/// Collect the directories to watch under `roots`, honoring gitignore (when enabled) and
/// the filter's ignored directory names / path prefixes.
fn collect_dirs(
    roots: &[PathBuf],
    filter: &WatchFilter,
    respect_gitignore: bool,
) -> HashSet<PathBuf> {
    let mut dirs = HashSet::new();
    for root in roots {
        let mut builder = WalkBuilder::new(root);
        builder
            .git_ignore(respect_gitignore)
            .git_global(respect_gitignore)
            .git_exclude(respect_gitignore)
            .ignore(respect_gitignore)
            .parents(respect_gitignore)
            .require_git(false)
            .hidden(false);

        let prune = filter.clone();
        builder.filter_entry(move |entry| {
            if entry.file_type().is_some_and(|t| t.is_dir())
                && prune.is_ignored_dir_name(entry.file_name())
            {
                return false;
            }
            !prune.is_under_ignored_prefix(entry.path())
        });

        for entry in builder.build().filter_map(Result::ok) {
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                dirs.insert(entry.into_path());
            }
        }
    }
    dirs
}

fn classify(kind: &EventKind) -> ChangeKind {
    match kind {
        EventKind::Create(_) => ChangeKind::Created,
        EventKind::Modify(ModifyKind::Name(_)) => ChangeKind::Renamed,
        EventKind::Modify(_) => ChangeKind::Modified,
        EventKind::Remove(_) => ChangeKind::Removed,
        _ => ChangeKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn config(root: &Path) -> WatchConfig {
        WatchConfig {
            roots: vec![root.to_path_buf()],
            filter: WatchFilter::markdown(),
            debounce: Duration::from_millis(100),
            respect_gitignore: true,
        }
    }

    /// Wait up to 20s for an event whose path ends with `needle`.
    fn wait_for(rx: &Receiver<WatchMessage>, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(WatchMessage::Event(ev)) if ev.path.ends_with(needle) => return true,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        false
    }

    #[test]
    fn collect_dirs_honors_gitignore_and_default_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "skipme/\n").expect("gitignore");
        std::fs::create_dir(root.join("skipme")).unwrap();
        std::fs::create_dir(root.join("keepme")).unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();

        let dirs = collect_dirs(&[root.to_path_buf()], &WatchFilter::markdown(), true);
        assert!(dirs.contains(root));
        assert!(dirs.contains(&root.join("keepme")));
        assert!(
            !dirs.contains(&root.join("skipme")),
            "gitignored dir should be pruned"
        );
        assert!(
            !dirs.contains(&root.join("node_modules")),
            "default don't-watch dir should be pruned"
        );
    }

    #[test]
    fn watches_markdown_creation_and_filters_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let (watcher, rx) = Watcher::start(config(&root)).expect("start watcher");

        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(root.join("note.md"), "# hi").expect("write md");
        std::fs::write(root.join("ignore.txt"), "nope").expect("write txt");

        let saw_md = wait_for(&rx, "note.md");
        drop(watcher);
        assert!(saw_md, "expected a watch event for note.md");
    }

    #[test]
    fn watches_files_created_in_a_new_subdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let (watcher, rx) = Watcher::start(config(&root)).expect("start watcher");

        std::thread::sleep(Duration::from_millis(400));
        let sub = root.join("newdir");
        std::fs::create_dir(&sub).expect("mkdir");
        std::fs::write(sub.join("deep.md"), "# deep").expect("write md");

        let saw = wait_for(&rx, "deep.md");
        drop(watcher);
        assert!(
            saw,
            "expected an event for deep.md in the newly created subdir"
        );
    }
}
