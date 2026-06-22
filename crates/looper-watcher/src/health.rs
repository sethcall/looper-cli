//! OS watch-limit detection, health reporting, and remediation.
//!
//! The headline limit is **Linux inotify** `fs.inotify.max_user_watches`: a watch
//! consumes one descriptor per directory. Because we prune ignored directories (see
//! `crate::collect_dirs`), the count of watched directories is an honest, accurate
//! estimate of inotify descriptor usage. We read the limit and report a [`WatchHealth`]
//! with actionable [`Remediation`]. macOS (FSEvents) and Windows (ReadDirectoryChangesW)
//! watch recursively without a per-directory limit, so there the limit is not applicable.

/// Linux inotify per-user limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InotifyLimits {
    /// Max watches (directories) per user (`fs.inotify.max_user_watches`).
    pub max_user_watches: usize,
    /// Max inotify instances per user (`fs.inotify.max_user_instances`).
    pub max_user_instances: usize,
}

/// Coarse health classification of the watch subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStatus {
    /// Comfortable headroom, or a platform without a per-watch limit.
    Healthy,
    /// Approaching the inotify watch limit (>= 80%).
    Approaching,
    /// At or beyond the inotify watch limit — some watches may fail.
    Exceeded,
    /// A watcher backend error occurred (e.g. notification-queue overflow).
    Degraded,
}

/// Actionable remediation for a non-healthy watch status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remediation {
    /// One-line summary of what to do.
    pub summary: String,
    /// Shell commands that fix it (the UI can present / offer to run these).
    pub commands: Vec<String>,
    /// Whether the commands need root/sudo (so the app should ask rather than silently run).
    pub requires_root: bool,
}

/// Error from [`Remediation::apply`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The command could not be spawned.
    #[error("failed to run `{command}`: {source}")]
    Spawn {
        /// The command that failed to spawn.
        command: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The command ran but exited unsuccessfully (commonly: not elevated).
    #[error("`{command}` exited with status {code:?}")]
    Failed {
        /// The command that failed.
        command: String,
        /// The exit code, if any.
        code: Option<i32>,
    },
}

impl Remediation {
    /// Best-effort: run the remediation commands.
    ///
    /// These usually require root, so on a typical desktop this returns [`ApplyError`]
    /// (no askpass / not elevated). A GUI should prompt for elevation (e.g. via
    /// `pkexec`) and surface that error rather than failing silently.
    ///
    /// # Errors
    /// Returns [`ApplyError`] if a command cannot be spawned or exits unsuccessfully.
    pub fn apply(&self) -> Result<(), ApplyError> {
        for command in &self.commands {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .status()
                .map_err(|source| ApplyError::Spawn {
                    command: command.clone(),
                    source,
                })?;
            if !status.success() {
                return Err(ApplyError::Failed {
                    command: command.clone(),
                    code: status.code(),
                });
            }
        }
        Ok(())
    }
}

/// A snapshot of watch-subsystem health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchHealth {
    /// Coarse status.
    pub status: WatchStatus,
    /// Directories actually being watched (≈ inotify descriptors used on Linux).
    pub watched_dirs: usize,
    /// Linux inotify limits when available; `None` on macOS/Windows.
    pub limits: Option<InotifyLimits>,
    /// Remediation when not [`WatchStatus::Healthy`].
    pub remediation: Option<Remediation>,
    /// Human-readable, platform-specific note.
    pub note: String,
}

/// Parse a `/proc/sys/...` integer value. Only meaningful on Linux.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_limit(contents: &str) -> Option<usize> {
    contents.trim().parse().ok()
}

/// Read the Linux inotify limits, or `None` on non-Linux / when unreadable.
#[cfg(target_os = "linux")]
#[must_use]
pub fn read_inotify_limits() -> Option<InotifyLimits> {
    let watches =
        parse_limit(&std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").ok()?)?;
    let instances =
        parse_limit(&std::fs::read_to_string("/proc/sys/fs/inotify/max_user_instances").ok()?)?;
    Some(InotifyLimits {
        max_user_watches: watches,
        max_user_instances: instances,
    })
}

/// Read the Linux inotify limits, or `None` on non-Linux / when unreadable.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn read_inotify_limits() -> Option<InotifyLimits> {
    None
}

/// Suggest a new `max_user_watches` value with headroom (>= 2x need, >= 512Ki, rounded
/// up to a power of two).
#[must_use]
pub fn suggested_watch_limit(needed: usize) -> usize {
    needed
        .saturating_mul(2)
        .max(524_288)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX)
}

fn inotify_remediation(needed: usize) -> Remediation {
    let target = suggested_watch_limit(needed);
    Remediation {
        summary: format!("Raise the inotify watch limit to at least {target}."),
        commands: vec![
            format!("sudo sysctl -w fs.inotify.max_user_watches={target}"),
            format!(
                "echo 'fs.inotify.max_user_watches={target}' | sudo tee /etc/sysctl.d/99-looper-inotify.conf"
            ),
        ],
        requires_root: true,
    }
}

fn linux_note() -> String {
    "Linux inotify uses one watch descriptor per directory. Ignored directories \
     (gitignored or in the don't-watch list, e.g. node_modules) are pruned and not counted."
        .to_string()
}

fn platform_note() -> String {
    #[cfg(target_os = "macos")]
    let note = "macOS FSEvents watches recursively without a per-directory descriptor limit.";
    #[cfg(target_os = "windows")]
    let note = "Windows ReadDirectoryChangesW watches recursively; the main risk is \
                notification-buffer overflow under heavy bursts.";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let note = "This platform has no known per-watch limit.";
    note.to_string()
}

/// Compute health from the watched-directory count and (optional) inotify limits.
#[must_use]
pub fn compute_health(watched_dirs: usize, limits: Option<InotifyLimits>) -> WatchHealth {
    let Some(limits) = limits else {
        return WatchHealth {
            status: WatchStatus::Healthy,
            watched_dirs,
            limits: None,
            remediation: None,
            note: platform_note(),
        };
    };

    let limit = limits.max_user_watches;
    // >= 80% without floating point: watched/limit >= 4/5.
    let status = if watched_dirs >= limit {
        WatchStatus::Exceeded
    } else if watched_dirs.saturating_mul(5) >= limit.saturating_mul(4) {
        WatchStatus::Approaching
    } else {
        WatchStatus::Healthy
    };

    let remediation = match status {
        WatchStatus::Healthy => None,
        _ => Some(inotify_remediation(watched_dirs.max(limit))),
    };

    WatchHealth {
        status,
        watched_dirs,
        limits: Some(limits),
        remediation,
        note: linux_note(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_limit_handles_trailing_newline() {
        assert_eq!(parse_limit("8192\n"), Some(8192));
        assert_eq!(parse_limit("  524288 "), Some(524_288));
        assert_eq!(parse_limit("not-a-number"), None);
    }

    #[test]
    fn no_limits_is_healthy() {
        let h = compute_health(100_000, None);
        assert_eq!(h.status, WatchStatus::Healthy);
        assert!(h.remediation.is_none());
    }

    #[test]
    fn health_thresholds_and_remediation() {
        let limits = Some(InotifyLimits {
            max_user_watches: 1000,
            max_user_instances: 128,
        });
        assert_eq!(compute_health(500, limits).status, WatchStatus::Healthy);
        assert_eq!(compute_health(800, limits).status, WatchStatus::Approaching);
        assert_eq!(compute_health(1000, limits).status, WatchStatus::Exceeded);

        let rem = compute_health(1000, limits).remediation.unwrap();
        assert!(rem.requires_root);
        assert!(rem.commands.iter().any(|c| c.contains("max_user_watches")));
    }

    #[test]
    fn suggested_limit_has_headroom() {
        assert_eq!(suggested_watch_limit(1000), 524_288);
        assert_eq!(suggested_watch_limit(400_000), 1_048_576);
    }

    #[test]
    fn apply_runs_commands() {
        let ok = Remediation {
            summary: "ok".to_string(),
            commands: vec!["true".to_string()],
            requires_root: false,
        };
        assert!(ok.apply().is_ok());

        let bad = Remediation {
            summary: "bad".to_string(),
            commands: vec!["false".to_string()],
            requires_root: false,
        };
        assert!(matches!(bad.apply(), Err(ApplyError::Failed { .. })));
    }
}
