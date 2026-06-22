//! Per-link `.gitignore` lines for Looper-generated links (plan items 49, 55).
//!
//! Each generated link a host opts to ignore gets a single **anchored** line in that repo's
//! `.gitignore` — e.g. `/kb` (anchored to the repo root, so it matches exactly the top-level
//! symlink, not nested files of the same name). [`apply`] appends the line when the link is ignored
//! and removes it when not, leaving the rest of the file alone; an otherwise-empty file Looper
//! created is cleaned up. The single-line model keeps the change legible (and previewable as a
//! one-line diff — item 55).

use std::path::Path;

use crate::WorkspaceError;

/// The anchored `.gitignore` line for a generated link `name` at the repo root (e.g. `/kb`).
#[must_use]
pub fn line_for(name: &str) -> String {
    format!("/{name}")
}

/// Whether `repo_root/.gitignore` already contains the anchored line for `name`.
#[must_use]
pub fn contains(repo_root: &Path, name: &str) -> bool {
    let line = line_for(name);
    std::fs::read_to_string(repo_root.join(".gitignore"))
        .map(|content| content.lines().any(|l| l.trim() == line))
        .unwrap_or(false)
}

/// Ensure `repo_root/.gitignore` contains (when `enabled`) or omits (when not) the anchored line
/// for `name`. The line is appended at the end; removal drops every matching line and cleans up a
/// now-empty file. User content is otherwise preserved.
///
/// # Errors
/// Returns [`WorkspaceError::Gitignore`] if the file can't be written or removed.
pub fn apply(repo_root: &Path, name: &str, enabled: bool) -> Result<(), WorkspaceError> {
    let path = repo_root.join(".gitignore");
    let line = line_for(name);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let has = existing.lines().any(|l| l.trim() == line);

    if enabled {
        if has {
            return Ok(()); // already present
        }
        let mut body = existing;
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&line);
        body.push('\n');
        return write(&path, &body);
    }

    if !has {
        return Ok(()); // already absent
    }
    let kept: Vec<&str> = existing.lines().filter(|l| l.trim() != line).collect();
    if kept.iter().all(|l| l.trim().is_empty()) {
        // Only our line (and blank lines) remained — remove the file rather than leave it empty.
        if path.exists() {
            std::fs::remove_file(&path).map_err(|source| WorkspaceError::Gitignore {
                path: path.clone(),
                source,
            })?;
        }
        return Ok(());
    }
    let mut body = kept.join("\n");
    body.push('\n');
    write(&path, &body)
}

fn write(path: &Path, content: &str) -> Result<(), WorkspaceError> {
    std::fs::write(path, content).map_err(|source| WorkspaceError::Gitignore {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn appends_and_removes_anchored_lines_preserving_user_content() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let gi = repo.join(".gitignore");
        std::fs::write(&gi, "target/\n*.log\n").unwrap();

        // Add `/kb` at the end; user lines stay.
        apply(repo, "kb", true).unwrap();
        assert_eq!(read(&gi), "target/\n*.log\n/kb\n");
        assert!(contains(repo, "kb"));

        // Idempotent.
        apply(repo, "kb", true).unwrap();
        assert_eq!(read(&gi), "target/\n*.log\n/kb\n");

        // A second link appends another line.
        apply(repo, "specs", true).unwrap();
        assert_eq!(read(&gi), "target/\n*.log\n/kb\n/specs\n");

        // Remove `/kb` — only that line goes; user content + the other link remain.
        apply(repo, "kb", false).unwrap();
        assert_eq!(read(&gi), "target/\n*.log\n/specs\n");
        assert!(!contains(repo, "kb"));

        // Removing an absent line is a no-op.
        apply(repo, "kb", false).unwrap();
        assert_eq!(read(&gi), "target/\n*.log\n/specs\n");
    }

    #[test]
    fn creates_then_removes_an_otherwise_empty_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let gi = repo.join(".gitignore");

        apply(repo, "kb", true).unwrap();
        assert_eq!(read(&gi), "/kb\n");

        apply(repo, "kb", false).unwrap();
        assert!(
            !gi.exists(),
            "a .gitignore Looper created + emptied is cleaned up"
        );
    }
}
