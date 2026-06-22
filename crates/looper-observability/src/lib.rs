//! `looper-observability` — logging & diagnostics for Looper.
//!
//! [`init`] installs a `tracing` subscriber with a pretty console layer and a JSON
//! daily-rolling file layer (in [`looper_config::Config::log_dir`]), plus a panic hook
//! that records panics (location, message, backtrace) to the log. The level filter is
//! taken from `RUST_LOG` when set, otherwise `config.log.level`.
//!
//! Dependency rule: may depend on `looper-config`. See `../../AGENTS.md`.
//! Implements plan item 05 (`../../specs/plan/05-observability-logging.md`).
//!
//! Out of scope for Milestone 1 (future work): log retention/cleanup, runtime-reloadable
//! level, and field redaction.

mod error;

use std::panic::PanicHookInfo;
use std::path::Path;

use looper_config::{Config, Dirs};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub use error::ObsError;

/// Guards that must be held for the lifetime of the process. Dropping them stops file
/// logging and flushes buffered output, so the application keeps this alive until exit.
#[must_use = "dropping the guard stops file logging and flushes buffered output"]
#[derive(Debug)]
pub struct Guards {
    _file: WorkerGuard,
}

/// Initialize logging & diagnostics from configuration.
///
/// # Errors
/// Returns [`ObsError`] if the log directory cannot be created or a subscriber is
/// already installed for this process.
pub fn init(config: &Config, dirs: &Dirs) -> Result<Guards, ObsError> {
    let log_dir = config.log_dir(dirs);
    ensure_log_dir(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "looper.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let directive = filter_directive(std::env::var("RUST_LOG").ok(), &config.log.level);
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = fmt::layer()
        .with_target(true)
        .with_ansi(cfg!(debug_assertions));
    let file_layer = fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .try_init()?;

    install_panic_hook();

    Ok(Guards { _file: guard })
}

/// Create the log directory (and any parents) if it does not already exist.
fn ensure_log_dir(dir: &Path) -> Result<(), ObsError> {
    std::fs::create_dir_all(dir).map_err(|source| ObsError::CreateLogDir {
        path: dir.to_path_buf(),
        source,
    })
}

/// Resolve the tracing filter directive: `RUST_LOG` (when non-empty) wins, otherwise
/// the configured level.
fn filter_directive(rust_log: Option<String>, config_level: &str) -> String {
    rust_log
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| config_level.to_string())
}

/// Install a panic hook that records panics via `tracing`, then delegates to the
/// previously-installed hook (which still prints to stderr).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = panic_message(info);
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            target: "panic",
            location = %location,
            message = %message,
            backtrace = %backtrace,
            "panic"
        );
        previous(info);
    }));
}

/// Extract a human-readable message from a panic payload.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_prefers_rust_log_when_set() {
        assert_eq!(filter_directive(Some("debug".into()), "info"), "debug");
        assert_eq!(
            filter_directive(Some("looper=trace".into()), "info"),
            "looper=trace"
        );
    }

    #[test]
    fn directive_falls_back_to_config_level() {
        assert_eq!(filter_directive(None, "warn"), "warn");
        assert_eq!(filter_directive(Some("   ".into()), "warn"), "warn");
    }

    #[test]
    fn ensure_log_dir_creates_nested_dirs_idempotently() {
        let base = std::env::temp_dir().join(format!("looper-obs-dir-{}", std::process::id()));
        let nested = base.join("a/b/logs");
        ensure_log_dir(&nested).expect("create");
        assert!(nested.is_dir());
        ensure_log_dir(&nested).expect("idempotent");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// End-to-end: `init` installs the subscriber and the file layer actually writes a
    /// rolling log file. Only one test per process may call `init` (global subscriber).
    #[test]
    fn init_writes_a_rolling_log_file() {
        let base = std::env::temp_dir().join(format!("looper-obs-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dirs = Dirs {
            config: base.clone(),
            data: base.clone(),
            log: base.join("logs"),
            cache: base.clone(),
        };
        let config = Config::default(); // log.dir = None -> uses dirs.log

        let guard = init(&config, &dirs).expect("init");
        tracing::info!(test = true, "hello from the observability test");
        drop(guard); // joins the writer thread and flushes

        let wrote_log = std::fs::read_dir(base.join("logs"))
            .expect("read log dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("looper.log"));
        assert!(
            wrote_log,
            "expected a looper.log.* file in {:?}",
            base.join("logs")
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
