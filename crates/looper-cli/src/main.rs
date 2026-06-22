//! `looper-cli` — the terminal entry point to the Looper engine.
//!
//! Drives the same `looper-core` engine the desktop app uses, but headless: it injects the concrete
//! OKF knowledge base and no-op sync/enrichment seams (the engine is LLM-free here). A command opens
//! a workspace — either a **saved** one (`--workspace`) or an **ad-hoc** one over folders — then
//! reports the scan/index lifecycle. The engine runs on OS threads and emits events on a broadcast
//! bus.

mod demo;
mod session;
mod tui;
mod workspace;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use looper_ipc::EngineEvent;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::session::open;

#[derive(Parser)]
#[command(
    name = "looper-cli",
    version,
    about = "Looper engine — scan, watch, and index AI-generated docs from the terminal."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where a command gets its workspace: a saved one (`--workspace`) or ad-hoc folders (+ `--kb-dir`).
#[derive(Args)]
pub struct SourceArgs {
    /// Source folder(s) to index — one or more. Omit when using `--workspace`.
    #[arg(value_name = "FOLDER")]
    pub folders: Vec<PathBuf>,
    /// Use a saved workspace (by name or id) instead of ad-hoc folders.
    #[arg(long, value_name = "NAME")]
    pub workspace: Option<String>,
    /// OKF index output dir for ad-hoc folders. Prompted if omitted; not needed with --workspace.
    #[arg(long = "kb-dir", visible_alias = "kb", value_name = "DIR")]
    pub kb_dir: Option<PathBuf>,
    /// Workspace store file (default: the CLI config dir's `workspaces.json`).
    #[arg(long, value_name = "FILE")]
    pub store: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Scan once and build/update the OKF index, reporting what was indexed.
    Scan {
        #[command(flatten)]
        source: SourceArgs,
        /// Emit engine events + a summary as JSON lines (machine-readable).
        #[arg(long)]
        json: bool,
    },
    /// Scan, then keep watching and re-indexing live until interrupted (Ctrl-C drains + exits).
    Watch {
        #[command(flatten)]
        source: SourceArgs,
        /// Emit engine events as JSON lines (machine-readable; good for services).
        #[arg(long)]
        json: bool,
    },
    /// Live Activity view in the terminal (a TUI): folder activity + index status.
    Tui {
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Manage workspaces.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCmd,
    },
    /// Generate live activity over the bundled `demo/` corpus (run a `tui` over it separately).
    Demo {
        #[command(subcommand)]
        command: Option<DemoCmd>,
    },
}

#[derive(Subcommand)]
enum DemoCmd {
    /// Strip leftover `<!-- looper demo tick -->` markers from the demo docs (after a hard kill).
    Reset,
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Create a workspace interactively (or via flags); writes the same config the desktop app uses.
    Create {
        /// Workspace store file to write (default: the CLI config dir's `workspaces.json`).
        #[arg(long, value_name = "FILE")]
        store: Option<PathBuf>,
        /// Workspace name (skips the name prompt).
        #[arg(long)]
        name: Option<String>,
        /// Input folder containing markdown; repeatable (skips the folder prompt).
        #[arg(long = "folder", value_name = "DIR")]
        folders: Vec<PathBuf>,
        /// Docs-only Specs Repo folder; repeatable and must also be supplied as an input folder.
        #[arg(long = "specs-folder", value_name = "DIR")]
        specs_folders: Vec<PathBuf>,
        /// Parent folder whose direct child folders can be selected interactively.
        #[arg(long, value_name = "DIR")]
        discover: Option<PathBuf>,
        /// Knowledge base output directory (skips the kb-dir prompt).
        #[arg(long, value_name = "DIR")]
        kb_dir: Option<PathBuf>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List saved workspaces and their config.
    List {
        /// Workspace store file (default: the CLI config dir's `workspaces.json`).
        #[arg(long, value_name = "FILE")]
        store: Option<PathBuf>,
        /// Emit the full workspace config as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan { source, json } => scan(&source, json),
        Command::Watch { source, json } => watch(&source, json),
        Command::Tui { source } => tui::run(&source),
        Command::Workspace { command } => match command {
            WorkspaceCmd::Create {
                store,
                name,
                folders,
                specs_folders,
                discover,
                kb_dir,
                yes,
            } => workspace::create(store, name, folders, specs_folders, discover, kb_dir, yes),
            WorkspaceCmd::List { store, json } => workspace::list(store, json),
        },
        Command::Demo { command } => match command {
            Some(DemoCmd::Reset) => demo::reset(),
            None => demo::run(),
        },
    }
}

/// Open the workspace, run the startup scan, report the index, exit.
fn scan(source: &SourceArgs, json: bool) -> Result<()> {
    let (session, mut events) = open(source)?;
    if !json {
        eprintln!(
            "Scanning {} → index at {}",
            session.label,
            session.kb_dir.display()
        );
    }
    let (mut indexed, mut removed) = (0u32, 0u32);
    loop {
        let event = match events.blocking_recv() {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        emit_event(&event, &session.wid, json);
        if let EngineEvent::ScanCompleted {
            workspace_id,
            indexed: i,
            removed: r,
        } = &event
        {
            if *workspace_id == session.wid {
                (indexed, removed) = (*i, *r);
                break;
            }
        }
    }
    let count = session.engine.doc_count(&session.wid);
    if json {
        print_line(
            &serde_json::json!({
                "event": "scan_complete",
                "indexed": indexed, "removed": removed,
                "doc_count": count, "kb_dir": session.kb_dir,
            })
            .to_string(),
        );
    } else {
        print_line(&format!(
            "{count} document(s) indexed at {}",
            session.kb_dir.display()
        ));
    }
    session.engine.shutdown();
    Ok(())
}

/// Like [`scan`], but stays open and re-indexes live until Ctrl-C, which **drains + joins** workers
/// before exiting.
fn watch(source: &SourceArgs, json: bool) -> Result<()> {
    let (session, mut events) = open(source)?;
    if !json {
        eprintln!(
            "Watching {} → index at {}  (Ctrl-C to stop)",
            session.label,
            session.kb_dir.display()
        );
    }

    // Ctrl-C / SIGTERM → request a clean shutdown (checked between non-blocking drains).
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&stop);
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }

    let mut scanned = false;
    while !stop.load(Ordering::SeqCst) {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    emit_event(&event, &session.wid, json);
                    if !scanned
                        && matches!(&event, EngineEvent::ScanCompleted { workspace_id, .. } if *workspace_id == session.wid)
                    {
                        scanned = true;
                        if !json {
                            eprintln!("— initial scan done; watching for changes —");
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    if json {
        print_line(&serde_json::json!({ "event": "draining" }).to_string());
    } else {
        eprintln!("\nDraining + stopping…");
    }
    session.engine.shutdown(); // stops + joins workers (in-flight index writes finish)
    if json {
        print_line(&serde_json::json!({ "event": "stopped" }).to_string());
    } else {
        eprintln!("Stopped.");
    }
    Ok(())
}

/// Emit one engine event in the chosen format. JSON mode emits every event (the CLI opens exactly
/// one workspace) as a JSON line; human mode emits a one-liner for events belonging to `wid`.
fn emit_event(event: &EngineEvent, wid: &str, json: bool) {
    if json {
        if let Ok(line) = serde_json::to_string(event) {
            print_line(&line);
        }
    } else if let Some(line) = describe_event(event, wid) {
        eprintln!("  {line}");
    }
}

/// Write a line to stdout, exiting cleanly if the consumer closed the pipe (so `… --json | head`
/// terminates like a normal Unix tool instead of panicking — we can't reset `SIGPIPE` because
/// `unsafe_code` is forbidden).
pub(crate) fn print_line(line: &str) {
    use std::io::Write;
    if let Err(err) = writeln!(std::io::stdout(), "{line}") {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
    }
}

/// Expand a leading `~/` to the user's home directory; otherwise return the path as-is.
pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// A one-line human description of an engine event that belongs to `wid` (else `None`).
fn describe_event(event: &EngineEvent, wid: &str) -> Option<String> {
    match event {
        EngineEvent::ScanStarted {
            workspace_id,
            total,
        } if workspace_id == wid => Some(format!("scan: {total} change(s) to apply…")),
        EngineEvent::DocIndexed {
            workspace_id,
            title,
            path,
            ..
        } if workspace_id == wid => Some(format!("indexed: {title}  ({path})")),
        EngineEvent::DocRemoved { workspace_id, path } if workspace_id == wid => {
            Some(format!("removed: {path}"))
        }
        EngineEvent::FileChanged {
            workspace_id,
            path,
            kind,
        } if workspace_id == wid => Some(format!("changed [{kind:?}]: {path}")),
        EngineEvent::Warning {
            workspace_id,
            message,
        } if workspace_id == wid => Some(format!("warning: {message}")),
        EngineEvent::ScanCompleted {
            workspace_id,
            indexed,
            removed,
        } if workspace_id == wid => Some(format!(
            "scan complete: {indexed} indexed, {removed} removed"
        )),
        _ => None,
    }
}
