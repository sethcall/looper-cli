//! `looper-cli demo` — generate live activity over the bundled `demo/` corpus so the Activity TUI
//! (run separately over the same folders) has something to show.
//!
//! Each tick rewrites one doc with a tiny **rotating trailing HTML comment** — a real (but
//! invisible-when-rendered) content change, since the engine de-dups no-op writes by content hash.
//! It round-robins all the demo markdown at ~3 changes/second until Ctrl-C, then **restores every
//! file to its original bytes** so the committed docs stay pristine. Run from the looper-cli root.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// The demo "repos" (relative to the looper-cli root).
const REPOS: [&str; 3] = ["demo/frontend", "demo/backend", "demo/sre"];

/// Roughly 3 changes per second.
const TICK: Duration = Duration::from_millis(333);

pub fn run() -> Result<()> {
    let paths = collect_md_files();
    if paths.is_empty() {
        bail!(
            "no demo docs found under {} — run `looper-cli demo` from the looper-cli repo root",
            REPOS.join(", ")
        );
    }

    // Snapshot every file's original bytes so we can restore them on exit.
    let mut docs: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        docs.push((path, bytes));
    }

    eprintln!(
        "Demo: editing {} docs across {} repos at ~3/s (Ctrl-C to stop + restore).",
        docs.len(),
        REPOS.len()
    );
    eprintln!("Watch it live in another terminal:");
    eprintln!("  cargo run -p looper-cli -- tui demo/frontend demo/backend demo/sre --kb demo/kb");
    eprintln!();

    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&stop);
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }

    let mut count = 0usize;
    while !stop.load(Ordering::SeqCst) {
        let (path, original) = &docs[count % docs.len()];
        // original bytes + a rotating marker → a real content change the engine will re-index.
        let mut bytes = original.clone();
        bytes.extend_from_slice(format!("\n<!-- looper demo tick {count} -->\n").as_bytes());
        std::fs::write(path, &bytes).with_context(|| format!("write {}", path.display()))?;
        eprintln!("  edited {}", path.display());
        count += 1;
        std::thread::sleep(TICK);
    }

    // Restore originals so the committed docs are unchanged after a clean stop.
    eprintln!("\nRestoring {} docs…", docs.len());
    for (path, original) in &docs {
        let _ = std::fs::write(path, original);
    }
    eprintln!("Demo stopped after {count} edits; docs restored.");
    Ok(())
}

/// Strip any leftover `<!-- looper demo tick N -->` markers from the demo docs — e.g. after a hard
/// kill that skipped the on-exit restore. Idempotent.
pub fn reset() -> Result<()> {
    let files = collect_md_files();
    if files.is_empty() {
        bail!(
            "no demo docs found under {} — run `looper-cli demo --reset` from the looper-cli root",
            REPOS.join(", ")
        );
    }
    let mut fixed = 0usize;
    for path in files {
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cleaned = strip_markers(&content);
        if cleaned != content {
            std::fs::write(&path, &cleaned).with_context(|| format!("write {}", path.display()))?;
            eprintln!("  reset {}", path.display());
            fixed += 1;
        }
    }
    eprintln!("Reset {fixed} file(s); demo markers removed.");
    Ok(())
}

/// Drop demo-tick marker lines, then normalize to a single trailing newline.
fn strip_markers(content: &str) -> String {
    let body = content
        .lines()
        .filter(|line| !is_marker(line.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}\n", body.trim_end())
}

fn is_marker(line: &str) -> bool {
    line.starts_with("<!-- looper demo tick") && line.ends_with("-->")
}

fn collect_md_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for repo in REPOS {
        collect_into(Path::new(repo), &mut out);
    }
    out.sort();
    out
}

fn collect_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_into(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}
