//! Shared engine/workspace setup for the CLI commands.
//!
//! Resolves a workspace from the common [`SourceArgs`] — either a **saved** workspace (`--workspace`)
//! or an **ad-hoc** one over one or more folders (+ optional `--kb-dir`) — then opens it on the
//! engine and returns an event receiver subscribed before the open (so no lifecycle events are lost).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use inquire::{InquireError, Text};
use looper_config::Dirs;
use looper_core::Engine;
use looper_enrichment::MockEnricher;
use looper_ipc::EngineEvent;
use looper_okf::OkfProvider;
use looper_sync::MockSyncer;
use looper_workspace::{Workspace, WorkspaceStore};
use tokio::sync::broadcast::Receiver;

use crate::SourceArgs;

/// An open engine + the workspace it's driving.
pub struct Session {
    /// The running engine (OKF KB; no-op sync/enrichment).
    pub engine: Engine,
    /// The opened workspace's id.
    pub wid: String,
    /// Human label for the workspace (its name).
    pub label: String,
    /// Where the OKF index is written.
    pub kb_dir: PathBuf,
    /// The workspace's source folders (for display + relativizing paths).
    pub folders: Vec<PathBuf>,
}

/// Resolve the workspace from `src`, open it on a fresh engine, and return the session + a receiver
/// subscribed **before** the open.
pub fn open(src: &SourceArgs) -> Result<(Session, Receiver<EngineEvent>)> {
    let workspace = resolve_workspace(src)?;

    let engine = Engine::new(
        Arc::new(OkfProvider),
        Arc::new(MockSyncer::default()),
        Arc::new(MockEnricher::new()),
    );
    let wid = workspace.id.as_str().to_string();
    let label = workspace.name.clone();
    let kb_dir = workspace.kb_dir.clone();
    let folders = workspace.folders.clone();

    let events = engine.subscribe();
    engine.open_workspace(workspace).context("open workspace")?;

    Ok((
        Session {
            engine,
            wid,
            label,
            kb_dir,
            folders,
        },
        events,
    ))
}

/// Turn the CLI source args into a concrete [`Workspace`]:
/// - `--workspace NAME` → look it up (by name or id) in the store;
/// - one or more folders (+ optional `--kb-dir`) → an ad-hoc workspace (registered in a temp store;
///   only its OKF index, at `kb_dir`, persists).
fn resolve_workspace(src: &SourceArgs) -> Result<Workspace> {
    if src.workspace.is_some() && !src.folders.is_empty() {
        bail!("pass either source folders OR --workspace, not both");
    }

    if let Some(name) = &src.workspace {
        let store_path = store_path(src)?;
        let store = WorkspaceStore::open(&store_path)
            .with_context(|| format!("open workspace store {}", store_path.display()))?;
        return store
            .list()
            .iter()
            .find(|w| w.name == *name || w.id.as_str() == name)
            .cloned()
            .ok_or_else(|| anyhow!("no workspace named '{name}' in {}", store_path.display()));
    }

    if src.folders.is_empty() {
        bail!("specify one or more source folders, or --workspace <name>");
    }

    // Ad-hoc workspace over the given folders.
    let mut folders = Vec::with_capacity(src.folders.len());
    for f in &src.folders {
        folders.push(
            f.canonicalize()
                .with_context(|| format!("not a folder: {}", f.display()))?,
        );
    }
    let kb_dir = match &src.kb_dir {
        Some(k) => k.clone(),
        None => prompt_kb_dir()?,
    };

    let tmp = tempfile::tempdir().context("create temp workspace store")?;
    let mut store =
        WorkspaceStore::open(tmp.path().join("workspaces.json")).context("open workspace store")?;
    let id = store
        .create("looper-cli", folders, kb_dir)
        .context("create workspace")?;
    store
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("workspace creation failed"))
}

/// The workspace store file: `--store` if given, else the CLI config dir's `workspaces.json`.
pub fn store_path(src: &SourceArgs) -> Result<PathBuf> {
    match &src.store {
        Some(p) => Ok(p.clone()),
        None => Ok(Dirs::resolve_cli()?.config.join("workspaces.json")),
    }
}

/// Prompt for the index (KB) output dir when ad-hoc folders were given without `--kb`. There is no
/// default; the prompt nudges the user toward `workspace create` so they needn't answer again.
fn prompt_kb_dir() -> Result<PathBuf> {
    let input = Text::new("Where should the index (KB) be written?")
        .with_help_message(
            "no --kb given · tip: `looper-cli workspace create` saves this so you're not asked again",
        )
        .prompt()
        .map_err(|err| match err {
            InquireError::NotTTY | InquireError::IO(_) => anyhow!(
                "no --kb directory given and no terminal to prompt — \
                 pass --kb <dir> or use --workspace <name>"
            ),
            other => anyhow!("KB-directory prompt failed: {other}"),
        })?;
    let path = crate::expand_tilde(input.trim());
    if path.as_os_str().is_empty() {
        bail!("no KB directory given");
    }
    Ok(path)
}
