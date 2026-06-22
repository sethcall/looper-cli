//! `looper-cli workspace create` — interactive workspace setup.
//!
//! Writes the **same** JSON workspace config the desktop app uses (via
//! `looper-workspace::WorkspaceStore`), so a workspace created here is interchangeable with one
//! created in the desktop app. With `--name` + `--folder`, runs non-interactively (scripting/CI).

use std::fmt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use inquire::{Confirm, MultiSelect, Text};
use looper_config::Dirs;
use looper_ipc::{LinkingConfig, WorkspaceFolderCandidate};
use looper_workspace::{discover_folder_candidates, WorkspaceStore};

use crate::print_line;

/// Run `workspace create`. Prompts for any field not supplied via flags; with `--name` and at least
/// one `--folder`, skips prompts entirely. Writes to `store` (default: the CLI config dir's
/// `workspaces.json`, mirroring where the desktop keeps it next to `config.json`).
pub fn create(
    store: Option<PathBuf>,
    name: Option<String>,
    folders: Vec<PathBuf>,
    specs_folders: Vec<PathBuf>,
    discover: Option<PathBuf>,
    kb_dir: Option<PathBuf>,
    yes: bool,
) -> Result<()> {
    let store_path = match store {
        Some(p) => p,
        None => Dirs::resolve_cli()?.config.join("workspaces.json"),
    };

    let non_interactive = name.is_some() && !folders.is_empty();

    let (name, folders, specs_folders, kb_dir) = if non_interactive {
        let name = name.expect("checked non-empty");
        let Some(kb_dir) = kb_dir else {
            bail!("--kb-dir is required when creating non-interactively (with --name + --folder)");
        };
        (name, folders, specs_folders, kb_dir)
    } else {
        prompt(name, folders, specs_folders, discover, kb_dir)?
    };

    // Resolve folders to absolute paths and verify each is a real directory.
    let mut resolved = Vec::with_capacity(folders.len());
    for f in &folders {
        let abs = f
            .canonicalize()
            .with_context(|| format!("not a folder: {}", f.display()))?;
        if !abs.is_dir() {
            bail!("not a folder: {}", abs.display());
        }
        resolved.push(abs);
    }
    if resolved.is_empty() {
        bail!("a workspace needs at least one folder");
    }
    let mut resolved_specs = Vec::with_capacity(specs_folders.len());
    for specs_folder in &specs_folders {
        let abs = specs_folder
            .canonicalize()
            .with_context(|| format!("not a folder: {}", specs_folder.display()))?;
        if !abs.is_dir() {
            bail!("not a folder: {}", abs.display());
        }
        if !resolved.iter().any(|folder| folder == &abs) {
            bail!(
                "--specs-folder must also be supplied as an input --folder: {}",
                abs.display()
            );
        }
        resolved_specs.push(abs);
    }

    eprintln!("\nWorkspace to create:");
    eprintln!("  name:    {name}");
    eprintln!("  store:   {}", store_path.display());
    eprintln!("  kb_dir:  {}", kb_dir.display());
    eprintln!("  folders:");
    for f in &resolved {
        eprintln!("    - {}", f.display());
    }
    if !resolved_specs.is_empty() {
        eprintln!("  specs repos:");
        for f in &resolved_specs {
            eprintln!("    - {}", f.display());
        }
    }
    if !yes
        && !non_interactive
        && !Confirm::new("Create this workspace?")
            .with_default(true)
            .prompt()?
    {
        eprintln!("Aborted.");
        return Ok(());
    }

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut workspaces = WorkspaceStore::open(&store_path).context("open workspace store")?;
    let linking = LinkingConfig {
        specs_folders: resolved_specs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        links: Vec::new(),
    };
    let id = workspaces
        .create_with_linking(&name, resolved.clone(), kb_dir.clone(), linking)
        .context("create workspace")?;

    println!("Created workspace '{name}' ({})", id.as_str());
    println!("  config: {}", store_path.display());
    println!("  index:  {}", kb_dir.display());
    println!(
        "\nBuild its index with:\n  looper-cli scan {}",
        resolved[0].display()
    );
    Ok(())
}

/// List saved workspaces + their config (human-readable, or full JSON with `--json`).
pub fn list(store: Option<PathBuf>, json: bool) -> Result<()> {
    let store_path = match store {
        Some(p) => p,
        None => Dirs::resolve_cli()?.config.join("workspaces.json"),
    };
    let workspaces = WorkspaceStore::open(&store_path)
        .with_context(|| format!("open workspace store {}", store_path.display()))?;
    let all = workspaces.list();

    if json {
        print_line(&serde_json::to_string_pretty(all)?);
        return Ok(());
    }
    if all.is_empty() {
        print_line(&format!("No workspaces in {}", store_path.display()));
        return Ok(());
    }
    print_line(&format!(
        "{} workspace(s) in {}:",
        all.len(),
        store_path.display()
    ));
    for w in all {
        print_line("");
        print_line(&format!("  {} ({})", w.name, w.id.as_str()));
        print_line(&format!("    kb_dir:     {}", w.kb_dir.display()));
        print_line(&format!(
            "    backend:    {:?}    enrichment: {}",
            w.backend,
            if w.enrichment.enabled { "on" } else { "off" }
        ));
        print_line("    folders:");
        for f in &w.folders {
            print_line(&format!("      - {}", f.display()));
        }
    }
    Ok(())
}

/// Interactive prompts for any field not supplied via flags.
fn prompt(
    name: Option<String>,
    folders: Vec<PathBuf>,
    specs_folders: Vec<PathBuf>,
    discover: Option<PathBuf>,
    kb_dir: Option<PathBuf>,
) -> Result<(String, Vec<PathBuf>, Vec<PathBuf>, PathBuf)> {
    let name = match name {
        Some(n) => n,
        None => Text::new("Workspace name")
            .with_help_message("This is just a label for the workspace. You can change it later.")
            .prompt()?,
    };

    let mut folders = folders;
    if folders.is_empty() {
        folders = prompt_folders(discover)?;
    }
    let specs_folders = if specs_folders.is_empty() {
        prompt_specs_folders(&folders)?
    } else {
        specs_folders
    };

    let kb_dir = match kb_dir {
        Some(k) => k,
        None => {
            let input = Text::new("Knowledge base folder (output)")
                .with_help_message(
                    "This is the output folder where Looper writes this workspace's knowledge base. Pick a stable folder for generated workspace data.",
                )
                .prompt()?;
            crate::expand_tilde(input.trim())
        }
    };

    Ok((name, folders, specs_folders, kb_dir))
}

fn prompt_folders(discover: Option<PathBuf>) -> Result<Vec<PathBuf>> {
    if let Some(parent) = discover {
        let selected = prompt_discovered_folders(parent)?;
        if !selected.is_empty() {
            return Ok(selected);
        }
    } else {
        let input = Text::new("Folder to scan for Git repos")
            .with_help_message(
                "Recommended: scan a parent folder to quickly multi-select many Git repos. Leave blank to add individual folders.",
            )
            .prompt()?;
        let parent = crate::expand_tilde(input.trim());
        if !parent.as_os_str().is_empty() {
            let selected = prompt_discovered_folders(parent)?;
            if !selected.is_empty() {
                return Ok(selected);
            }
        }
    }

    prompt_manual_folders()
}

fn prompt_discovered_folders(parent: PathBuf) -> Result<Vec<PathBuf>> {
    let candidates = discover_folder_candidates(&parent)
        .with_context(|| format!("discover folders under {}", parent.display()))?;
    if candidates.is_empty() {
        eprintln!("  no child folders found under {}", parent.display());
        return Ok(Vec::new());
    }

    let options: Vec<FolderCandidateOption> = candidates
        .into_iter()
        .map(|candidate| FolderCandidateOption { candidate })
        .collect();
    let selected = MultiSelect::new("Folders containing markdown (inputs)", options)
        .with_help_message(
            "Looper reads these input folders but does not touch anything in them unless you tell it to. Space toggles, Enter accepts, type to filter.",
        )
        .with_page_size(12)
        .prompt()?;

    Ok(selected
        .into_iter()
        .map(|option| PathBuf::from(option.candidate.path))
        .collect())
}

fn prompt_manual_folders() -> Result<Vec<PathBuf>> {
    let mut folders = Vec::new();
    loop {
        let input = Text::new("Folder containing markdown (path)")
            .with_help_message(
                "Input folder containing markdown. Looper does not touch anything in it unless you tell it to.",
            )
            .prompt()?;
        let path = crate::expand_tilde(input.trim());
        if path.as_os_str().is_empty() {
            continue;
        }
        if !path.is_dir() {
            eprintln!("  not a folder: {}", path.display());
            continue;
        }
        folders.push(path);
        if !Confirm::new("Add another folder?")
            .with_default(false)
            .prompt()?
        {
            break;
        }
    }
    Ok(folders)
}

fn prompt_specs_folders(folders: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if folders.is_empty() {
        return Ok(Vec::new());
    }

    let options: Vec<FolderPathOption> = folders
        .iter()
        .cloned()
        .map(|path| FolderPathOption { path })
        .collect();
    let selected = MultiSelect::new("Specs Repo folders (optional)", options)
        .with_help_message(
            "Select 'Specs Repo' when a git repo is a no-code, documentation-only repository. Looper has additional features for these types of folders.",
        )
        .with_page_size(12)
        .prompt()?;

    Ok(selected.into_iter().map(|option| option.path).collect())
}

#[derive(Clone)]
struct FolderCandidateOption {
    candidate: WorkspaceFolderCandidate,
}

impl fmt::Display for FolderCandidateOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match (self.candidate.is_git_repo, self.candidate.has_markdown) {
            (true, true) => "git+docs",
            (true, false) => "git",
            (false, true) => "docs",
            (false, false) => "dir",
        };
        write!(
            f,
            "{kind:<8}  {:<28} {}",
            self.candidate.name, self.candidate.path
        )
    }
}

#[derive(Clone)]
struct FolderPathOption {
    path: PathBuf,
}

impl fmt::Display for FolderPathOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        write!(f, "{:<28} {}", name, self.path.display())
    }
}
