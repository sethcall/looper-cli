//! Resolve a workspace's Linking configuration into concrete generated links (plan items 49, 55).
//!
//! Every **normal** (non-spec) source folder is a *host* that links to every specs folder + the KB.
//! Each `(host, target)` link's name + `.gitignore` can be overridden ([`looper_ipc::LinkConfig`]);
//! unset pairs use defaults (KB → `kb`, specs → its local dir name, no `.gitignore`). A specs folder
//! is a target, never a host. This module bridges that config to the filesystem [`crate::link`]
//! engine.

use std::path::{Path, PathBuf};

use looper_ipc::{LinkConfig, LinkNaming, LinkTargetKind};

use crate::link::{self, DesiredLink};
use crate::Workspace;

/// The concrete links a host should hold, resolved from the workspace's per-`(host, target)` config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedLinks {
    /// The links to create/repair in the host repo.
    pub desired: Vec<DesiredLink>,
    /// The subset of desired link names whose `(host, target)` opts into `.gitignore` management.
    pub gitignore_names: Vec<String>,
}

/// Every linkable target (the KB + each specs folder) — what [`crate::link::reconcile`] needs to
/// recognize a stale Looper link by the path it points at.
#[must_use]
pub fn known_targets(workspace: &Workspace) -> Vec<PathBuf> {
    let mut targets = vec![workspace.kb_dir.clone()];
    targets.extend(workspace.linking.specs_folders.iter().map(PathBuf::from));
    targets
}

/// Resolve the links a `repo_root` host should hold (items 49, 55): the KB + every specs folder
/// (except itself), each named per its `(host, target)` override or the default. A specs folder is
/// a target, not a host, so it resolves to no links.
#[must_use]
pub fn resolve_links(workspace: &Workspace, repo_root: &Path) -> ResolvedLinks {
    let mut out = ResolvedLinks::default();
    if workspace
        .linking
        .specs_folders
        .iter()
        .any(|p| Path::new(p) == repo_root)
    {
        return out; // a specs folder is a link target, never a host
    }
    let host = repo_root.to_string_lossy();

    push_resolved(
        &mut out,
        workspace,
        &host,
        LinkTargetKind::Kb,
        None,
        &workspace.kb_dir,
    );
    for sf in &workspace.linking.specs_folders {
        let specs_path = PathBuf::from(sf);
        if specs_path.as_path() == repo_root {
            continue;
        }
        push_resolved(
            &mut out,
            workspace,
            &host,
            LinkTargetKind::Specs,
            Some(sf.as_str()),
            &specs_path,
        );
    }
    out
}

fn push_resolved(
    out: &mut ResolvedLinks,
    workspace: &Workspace,
    host: &str,
    kind: LinkTargetKind,
    specs_folder: Option<&str>,
    target_path: &Path,
) {
    let cfg =
        workspace.linking.links.iter().find(|l| {
            l.host == host && l.kind == kind && l.specs_folder.as_deref() == specs_folder
        });
    let name = match cfg {
        Some(c) => resolve_name(c, target_path),
        // Defaults: the KB link stays "kb" (item-29 back-compat); a specs link takes its dir name.
        None => match kind {
            LinkTargetKind::Kb => link::LINK_NAME.to_string(),
            LinkTargetKind::Specs => local_dir_name(target_path),
        },
    };
    if cfg.is_some_and(|c| c.gitignore) {
        out.gitignore_names.push(name.clone());
    }
    out.desired.push(DesiredLink {
        name,
        target: target_path.to_path_buf(),
    });
}

fn resolve_name(cfg: &LinkConfig, target_path: &Path) -> String {
    match cfg.naming {
        LinkNaming::LocalDirName => local_dir_name(target_path),
        LinkNaming::GitTrueName => {
            looper_git::repo_true_name(target_path).unwrap_or_else(|| local_dir_name(target_path))
        }
        LinkNaming::Custom => cfg
            .custom_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| local_dir_name(target_path)),
    }
}

fn local_dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| link::LINK_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceStore;
    use looper_ipc::LinkingConfig;

    fn workspace_with(linking: LinkingConfig, kb: PathBuf) -> Workspace {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(tmp.path().join("w.json")).unwrap();
        let id = store.create("W", vec![], kb).unwrap();
        store.set_linking_config(&id, linking).unwrap();
        store.get(&id).unwrap().clone()
    }

    #[test]
    fn defaults_link_kb_and_each_spec_with_default_names() {
        let kb = PathBuf::from("/ws/kb");
        let specs = PathBuf::from("/ws/specs");
        let w = workspace_with(
            LinkingConfig {
                specs_folders: vec!["/ws/specs".into()],
                links: vec![],
            },
            kb.clone(),
        );
        let r = resolve_links(&w, Path::new("/ws/code"));
        assert_eq!(
            r.desired,
            vec![
                DesiredLink {
                    name: "kb".into(),
                    target: kb
                },
                DesiredLink {
                    name: "specs".into(),
                    target: specs
                },
            ]
        );
        assert!(r.gitignore_names.is_empty());
    }

    #[test]
    fn per_host_overrides_name_and_gitignore_independently() {
        let kb = PathBuf::from("/ws/kb");
        let w = workspace_with(
            LinkingConfig {
                specs_folders: vec![],
                links: vec![LinkConfig {
                    host: "/ws/code".into(),
                    kind: LinkTargetKind::Kb,
                    specs_folder: None,
                    naming: LinkNaming::Custom,
                    custom_name: Some("brain".into()),
                    gitignore: true,
                }],
            },
            kb.clone(),
        );
        // The configured host renames the KB link + ignores it.
        let a = resolve_links(&w, Path::new("/ws/code"));
        assert_eq!(
            a.desired,
            vec![DesiredLink {
                name: "brain".into(),
                target: kb.clone()
            }]
        );
        assert_eq!(a.gitignore_names, vec!["brain".to_string()]);
        // A different host with no override falls back to the "kb" default, no gitignore.
        let b = resolve_links(&w, Path::new("/ws/other"));
        assert_eq!(
            b.desired,
            vec![DesiredLink {
                name: "kb".into(),
                target: kb
            }]
        );
        assert!(b.gitignore_names.is_empty());
    }

    #[test]
    fn a_specs_folder_is_never_a_host() {
        let w = workspace_with(
            LinkingConfig {
                specs_folders: vec!["/ws/specs".into()],
                links: vec![],
            },
            PathBuf::from("/ws/kb"),
        );
        assert!(resolve_links(&w, Path::new("/ws/specs")).desired.is_empty());
    }

    #[test]
    fn git_true_name_falls_back_to_local_dir_off_repo() {
        let w = workspace_with(
            LinkingConfig {
                specs_folders: vec![],
                links: vec![LinkConfig {
                    host: "/ws/code".into(),
                    kind: LinkTargetKind::Kb,
                    specs_folder: None,
                    naming: LinkNaming::GitTrueName,
                    custom_name: None,
                    gitignore: false,
                }],
            },
            PathBuf::from("/ws/kb-store"),
        );
        let r = resolve_links(&w, Path::new("/ws/code"));
        assert_eq!(r.desired.len(), 1);
        assert_eq!(r.desired[0].name, "kb-store");
    }
}
