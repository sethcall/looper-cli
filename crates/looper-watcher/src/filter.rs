//! Path filtering for the watcher.
//!
//! [`WatchFilter`] serves two roles: it decides which directories to *prune* when
//! collecting the watch set (ignored directory names / path prefixes), and which file
//! events to *forward* (extension allow-list). It is generic — the watcher knows nothing
//! about OKF or markdown semantics. Gitignore honoring is configured separately on
//! [`crate::WatchConfig`] and applied during directory collection.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Decides which directories to prune and which file events to forward.
#[derive(Debug, Clone, Default)]
pub struct WatchFilter {
    ignored_dir_names: HashSet<OsString>,
    ignored_prefixes: Vec<PathBuf>,
    /// Allowed lowercase file extensions; `None` accepts any extension.
    extensions: Option<HashSet<String>>,
    /// Exact file names to also forward, regardless of extension (e.g. `.looperignore`).
    filenames: HashSet<OsString>,
}

impl WatchFilter {
    /// A filter that accepts only the given (case-insensitive) file extensions.
    pub fn with_extensions<I, S>(extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            extensions: Some(
                extensions
                    .into_iter()
                    .map(|e| e.into().to_ascii_lowercase())
                    .collect(),
            ),
            ..Self::default()
        }
    }

    /// A filter for markdown files that also skips the default don't-watch directories
    /// (`.git`, `node_modules`, `target`, `.looper`).
    #[must_use]
    pub fn markdown() -> Self {
        Self::with_extensions(["md"]).ignoring_default_dirs()
    }

    /// Add the default don't-watch directory names (`.git`, `node_modules`, `target`,
    /// `.looper`). Overridable: start from [`WatchFilter::with_extensions`] to omit them.
    #[must_use]
    pub fn ignoring_default_dirs(mut self) -> Self {
        for name in [".git", "node_modules", "target", ".looper"] {
            self.ignored_dir_names.insert(OsString::from(name));
        }
        self
    }

    /// Ignore any path that has a directory component with this exact name.
    #[must_use]
    pub fn ignoring_dir_name(mut self, name: impl Into<OsString>) -> Self {
        self.ignored_dir_names.insert(name.into());
        self
    }

    /// Ignore any path under `prefix` (used to exclude the KB output dir / `.looper`
    /// links and avoid an indexing feedback loop).
    #[must_use]
    pub fn ignoring_path(mut self, prefix: impl Into<PathBuf>) -> Self {
        self.ignored_prefixes.push(prefix.into());
        self
    }

    /// Also forward events for files with this exact name, regardless of extension or the allow-list
    /// (e.g. `.looperignore`, so a hand-edit can trigger a re-scan — item 47).
    #[must_use]
    pub fn also_filename(mut self, name: impl Into<OsString>) -> Self {
        self.filenames.insert(name.into());
        self
    }

    /// Whether `name` is an ignored directory name (used to prune the walk).
    #[must_use]
    pub fn is_ignored_dir_name(&self, name: &OsStr) -> bool {
        self.ignored_dir_names.contains(name)
    }

    /// Whether `path` lies under an ignored prefix (used to prune the walk).
    #[must_use]
    pub fn is_under_ignored_prefix(&self, path: &Path) -> bool {
        self.ignored_prefixes.iter().any(|p| path.starts_with(p))
    }

    /// Whether a file event for `path` should be forwarded.
    #[must_use]
    pub fn accepts(&self, path: &Path) -> bool {
        if self.is_under_ignored_prefix(path) {
            return false;
        }
        if path
            .components()
            .any(|c| matches!(c, Component::Normal(name) if self.ignored_dir_names.contains(name)))
        {
            return false;
        }
        if let Some(name) = path.file_name() {
            if self.filenames.contains(name) {
                return true; // exact-name match (e.g. .looperignore) bypasses the extension list
            }
        }
        match &self.extensions {
            None => true,
            Some(exts) => path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e.to_ascii_lowercase())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_filter_accepts_md_and_skips_noise() {
        let f = WatchFilter::markdown();
        assert!(f.accepts(Path::new("/ws/docs/readme.md")));
        assert!(f.accepts(Path::new("/ws/specs/a/b/deep.MD"))); // case-insensitive
        assert!(!f.accepts(Path::new("/ws/docs/readme.txt"))); // wrong extension
        assert!(!f.accepts(Path::new("/ws/node_modules/pkg/readme.md"))); // ignored dir
        assert!(!f.accepts(Path::new("/ws/.git/COMMIT_EDITMSG"))); // ignored dir
        assert!(!f.accepts(Path::new("/ws/no-extension"))); // no extension
    }

    #[test]
    fn also_filename_admits_exact_name_regardless_of_extension() {
        let f = WatchFilter::markdown().also_filename(".looperignore");
        assert!(f.accepts(Path::new("/ws/.looperignore"))); // exact name, no extension
        assert!(f.accepts(Path::new("/ws/sub/.looperignore"))); // nested
        assert!(f.accepts(Path::new("/ws/docs/x.md"))); // markdown still accepted
        assert!(!f.accepts(Path::new("/ws/other.txt"))); // unrelated still rejected
        assert!(!f.accepts(Path::new("/ws/.gitignore"))); // a different dotfile is not admitted
    }

    #[test]
    fn prune_predicates() {
        let f = WatchFilter::markdown().ignoring_path("/ws/.looper");
        assert!(f.is_ignored_dir_name(OsStr::new("node_modules")));
        assert!(!f.is_ignored_dir_name(OsStr::new("docs")));
        assert!(f.is_under_ignored_prefix(Path::new("/ws/.looper/kb")));
        assert!(!f.is_under_ignored_prefix(Path::new("/ws/docs")));
    }

    #[test]
    fn ignored_prefix_excludes_subtree() {
        let f = WatchFilter::markdown().ignoring_path("/ws/.looper");
        assert!(!f.accepts(Path::new("/ws/.looper/kb/x.md")));
        assert!(f.accepts(Path::new("/ws/docs/x.md")));
    }

    #[test]
    fn all_extensions_when_unset() {
        let f = WatchFilter::default();
        assert!(f.accepts(Path::new("/ws/anything.bin")));
    }
}
