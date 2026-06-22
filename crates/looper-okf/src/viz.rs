//! Viz renderer — a Rust port of OKF's `generate_visualization` (viewer/generator.py).
//!
//! Walks a bundle directory, builds a cytoscape graph model (nodes + edges + bodies +
//! types + palette), and renders a self-contained `viz.html` that references **locally
//! vendored** cytoscape + marked + viz.js (see `frontend/public/viz/`) and carries its
//! own strict CSP — so the graph renders fully **offline** under `script-src 'self'`.
//!
//! The transform is faithful to OKF's deterministic Python (the six `tests/test_viewer.py`
//! cases are ported below). Differences from upstream are deliberate and noted:
//!  - the bundle data is a **non-executable JSON island** (`<script type="application/json">`),
//!    not an inline `window.BUNDLE = …` script — the latter violates `script-src 'self'`;
//!  - cytoscape / marked / viz.js / viz.css load from `/viz/...` (vendored), never a CDN;
//!  - `<` in the embedded JSON is escaped to `<`, so a markdown body containing
//!    `</script>` cannot break out of the island.
//!
//! Implements plan item 13 (`../../specs/plan/13-viz-renderer.md`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value as Json};
use serde_yaml_ng::{Mapping, Value};

use crate::{Document, OkfError};

/// Auto-generated bundle index; never a concept node.
const INDEX_NAME: &str = "index.md";
/// Fallback node color for an unrecognized concept `type`.
const DEFAULT_NODE_COLOR: &str = "#94a3b8";
/// Concept `type` → node color. Mirrors OKF's `_TYPE_PALETTE` (order-significant).
const TYPE_PALETTE: &[(&str, &str)] = &[
    ("BigQuery Dataset", "#8b5cf6"),
    ("BigQuery Table", "#3b82f6"),
    ("Reference", "#10b981"),
];

/// The HTML template, compiled in so the renderer is hermetic (token: `__OKF_DATA__`).
const TEMPLATE: &str = include_str!("../assets/viz/template.html");

/// Counts returned by [`generate_visualization`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VizStats {
    /// Number of concept nodes written.
    pub concepts: usize,
    /// Number of edges written.
    pub edges: usize,
    /// Size of the generated HTML, in bytes.
    pub bytes: usize,
}

/// A rendered visualization document: the self-contained HTML plus its counts.
#[derive(Debug, Clone)]
pub struct VizDoc {
    /// The HTML (references vendored `/viz/*` assets; carries its own strict CSP).
    pub html: String,
    /// Counts for the rendered graph.
    pub stats: VizStats,
}

/// Walk a bundle and render its `viz.html` **as a string** (nothing is written). Used by
/// the embedded webview view, which serves the HTML straight into an iframe.
///
/// `bundle_name` defaults to the bundle directory's (canonicalized) name.
///
/// # Errors
/// [`OkfError::BundleNotFound`] if `bundle_root` is not a directory, or [`OkfError::Io`] /
/// [`OkfError::Json`] if reading the bundle or serializing the graph fails.
pub fn render_visualization(
    bundle_root: &Path,
    bundle_name: Option<&str>,
) -> Result<VizDoc, OkfError> {
    if !bundle_root.is_dir() {
        return Err(OkfError::BundleNotFound {
            path: bundle_root.to_path_buf(),
        });
    }

    let concepts = walk_concepts(bundle_root)?;
    let graph = build_graph(&concepts);
    let edges = graph["edges"].as_array().map_or(0, Vec::len);

    let name = match bundle_name {
        Some(n) => n.to_string(),
        None => {
            let canon = bundle_root.canonicalize();
            let p = canon.as_deref().unwrap_or(bundle_root);
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    };

    // Escape `<` so a body containing `</script>` cannot terminate the JSON island.
    let payload =
        serde_json::to_string(&json!({ "name": name, "graph": graph }))?.replace('<', "\\u003c");
    let html = TEMPLATE.replace("__OKF_DATA__", &payload);

    Ok(VizDoc {
        stats: VizStats {
            concepts: concepts.len(),
            edges,
            bytes: html.len(),
        },
        html,
    })
}

/// Walk `bundle_root` and write a single self-contained `viz.html` to `out_path`.
///
/// # Errors
/// As [`render_visualization`], plus [`OkfError::Io`] if creating the parent dir or writing
/// the file fails.
pub fn generate_visualization(
    bundle_root: &Path,
    out_path: &Path,
    bundle_name: Option<&str>,
) -> Result<VizStats, OkfError> {
    let doc = render_visualization(bundle_root, bundle_name)?;

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| OkfError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(out_path, &doc.html).map_err(|source| OkfError::Io {
        path: out_path.to_path_buf(),
        source,
    })?;

    Ok(doc.stats)
}

/// A single concept (one bundle `*.md`, excluding `index.md`).
struct Concept {
    id: String,
    ty: String,
    title: String,
    description: String,
    resource: String,
    tags: Vec<String>,
    body: String,
    links_to: Vec<String>,
}

impl Concept {
    /// Render as a cytoscape node (`{ "data": { … } }`).
    fn to_node(&self) -> Json {
        let label = if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        };
        json!({
            "data": {
                "id": self.id,
                "label": label,
                "type": self.ty,
                "description": self.description,
                "resource": self.resource,
                "tags": self.tags,
                "color": palette_color(&self.ty),
                // Bigger bodies → bigger nodes, capped (matches OKF).
                "size": 30 + (self.body.chars().count() / 200).min(60),
            }
        })
    }
}

fn palette_color(ty: &str) -> &'static str {
    TYPE_PALETTE
        .iter()
        .find(|(k, _)| *k == ty)
        .map_or(DEFAULT_NODE_COLOR, |(_, v)| v)
}

/// `](/some/path.md)` or `](/some/path.md#anchor)` — captures the `/…md` target.
fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\]\((/[A-Za-z0-9_./-]+\.md)(?:#[A-Za-z0-9_-]*)?\)").unwrap())
}

/// Extract de-duplicated, order-preserving concept ids linked from `body`.
fn extract_links(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for cap in link_re().captures_iter(body) {
        // Strip the leading slash(es) and the `.md` suffix to get the concept id.
        let mut cid = cap[1].trim_start_matches('/').to_string();
        if let Some(stripped) = cid.strip_suffix(".md") {
            cid = stripped.to_string();
        }
        if !cid.is_empty() && seen.insert(cid.clone()) {
            out.push(cid);
        }
    }
    out
}

/// Recursively collect `*.md` paths under `dir`.
fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), OkfError> {
    let read = std::fs::read_dir(dir).map_err(|source| OkfError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in read {
        let entry = entry.map_err(|source| OkfError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| OkfError::Io {
            path: path.clone(),
            source,
        })?;
        if ft.is_dir() {
            collect_md(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn walk_concepts(bundle_root: &Path) -> Result<Vec<Concept>, OkfError> {
    let mut md_paths = Vec::new();
    collect_md(bundle_root, &mut md_paths)?;
    md_paths.sort();

    let mut concepts = Vec::new();
    for md_path in md_paths {
        if md_path.file_name().and_then(|n| n.to_str()) == Some(INDEX_NAME) {
            continue;
        }
        let rel = md_path.strip_prefix(bundle_root).unwrap_or(&md_path);
        let concept_id = rel
            .with_extension("")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        let content = std::fs::read_to_string(&md_path).map_err(|source| OkfError::Io {
            path: md_path.clone(),
            source,
        })?;
        let doc = Document::parse(&content);
        let fm = &doc.frontmatter;

        concepts.push(Concept {
            ty: fm_str(fm, "type").unwrap_or_else(|| "Unknown".to_string()),
            title: fm_str(fm, "title").unwrap_or_else(|| concept_id.clone()),
            description: fm_str(fm, "description").unwrap_or_default(),
            resource: fm_str(fm, "resource").unwrap_or_default(),
            tags: fm_tags(fm),
            links_to: extract_links(&doc.body),
            body: doc.body,
            id: concept_id,
        });
    }
    Ok(concepts)
}

fn build_graph(concepts: &[Concept]) -> Json {
    let ids: HashSet<&str> = concepts.iter().map(|c| c.id.as_str()).collect();
    let nodes: Vec<Json> = concepts.iter().map(Concept::to_node).collect();

    let mut edges: Vec<Json> = Vec::new();
    let mut seen_edges: HashSet<(&str, &str)> = HashSet::new();
    for c in concepts {
        for target in &c.links_to {
            let target = target.as_str();
            if target == c.id || !ids.contains(target) {
                continue;
            }
            if !seen_edges.insert((c.id.as_str(), target)) {
                continue;
            }
            edges.push(json!({
                "data": {
                    "id": format!("{}__{target}", c.id),
                    "source": c.id,
                    "target": target,
                }
            }));
        }
    }

    let bodies: BTreeMap<&str, &str> = concepts
        .iter()
        .map(|c| (c.id.as_str(), c.body.as_str()))
        .collect();
    let mut types: Vec<&str> = concepts.iter().map(|c| c.ty.as_str()).collect();
    types.sort_unstable();
    types.dedup();
    let palette: serde_json::Map<String, Json> = TYPE_PALETTE
        .iter()
        .map(|(k, v)| ((*k).to_string(), json!(v)))
        .collect();

    json!({
        "nodes": nodes,
        "edges": edges,
        "bodies": bodies,
        "types": types,
        "palette": palette,
    })
}

fn fm_str(fm: &Mapping, key: &str) -> Option<String> {
    fm.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Frontmatter `tags` as strings: a sequence maps element-wise; a present scalar becomes a
/// single-element list; absent/null yields an empty list (matches OKF's `tags or []`).
fn fm_tags(fm: &Mapping) -> Vec<String> {
    match fm.get("tags") {
        Some(Value::Sequence(seq)) => seq.iter().map(value_to_string).collect(),
        Some(v) if !v.is_null() => vec![value_to_string(v)],
        _ => Vec::new(),
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_yaml_ng::to_string(other)
            .map(|s| s.trim_end().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: PathBuf, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn make_bundle(root: &Path) {
        write(
            root.join("datasets/my_dataset.md"),
            "---\ntype: BigQuery Dataset\ntitle: My dataset\ndescription: A test dataset.\n\
             resource: https://example.com/dataset\ntags: [test]\n---\n\
             Parent dataset for [users](/tables/users.md).\n",
        );
        write(
            root.join("tables/users.md"),
            "---\ntype: BigQuery Table\ntitle: Users\ndescription: User profiles.\n\
             resource: https://example.com/users\ntags: [users]\n---\n\
             Joinable with [events](/tables/events.md) and see [DAU](/references/metrics/dau.md).\n",
        );
        write(
            root.join("tables/events.md"),
            "---\ntype: BigQuery Table\ntitle: Events\ndescription: User events.\n\
             resource: https://example.com/events\ntags: [events]\n---\n\
             See [users](/tables/users.md).\n",
        );
        write(
            root.join("references/metrics/dau.md"),
            "---\ntype: Reference\ntitle: Daily active users\ndescription: DAU metric.\n\
             resource: https://example.com/dau\ntags: [metric]\n---\n\
             COUNT(DISTINCT user_id) per day.\n",
        );
        // An auto-generated index that must NOT appear as a concept node.
        write(
            root.join("index.md"),
            "# My Bundle\n- tables/users\n- tables/events\n",
        );
    }

    /// Pull the JSON payload out of the generated `<script type="application/json">` island.
    fn payload(html: &str) -> Json {
        let open = "<script type=\"application/json\" id=\"okf-data\">";
        let start = html.find(open).expect("island present") + open.len();
        let end = start + html[start..].find("</script>").expect("island closed");
        serde_json::from_str(&html[start..end]).expect("island is valid JSON")
    }

    fn node_ids(graph: &Json) -> HashSet<String> {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["data"]["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn edge_pairs(graph: &Json) -> HashSet<(String, String)> {
        graph["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["data"]["source"].as_str().unwrap().to_string(),
                    e["data"]["target"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn writes_self_contained_offline_html() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        make_bundle(&bundle);
        let out = tmp.path().join("viz.html");

        let stats = generate_visualization(&bundle, &out, Some("My Bundle")).unwrap();
        assert_eq!(stats.concepts, 4);
        assert!(stats.bytes > 0);
        assert!(out.exists());

        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("<title>OKF Bundle Viewer</title>"));
        assert!(html.to_lowercase().contains("cytoscape"));
        assert!(html.to_lowercase().contains("marked"));
        assert!(html.contains("\"My Bundle\""));
        // Offline + CSP-clean: vendored local scripts, a strict CSP, and no CDN.
        assert!(html.contains("/viz/cytoscape.min.js"));
        assert!(html.contains("script-src 'self'"));
        assert!(!html.contains("cdn.jsdelivr.net"));
        assert!(!html.contains("window.BUNDLE"));
    }

    #[test]
    fn render_returns_html_without_writing_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        make_bundle(&bundle);
        let doc = render_visualization(&bundle, Some("My Bundle")).unwrap();
        assert_eq!(doc.stats.concepts, 4);
        assert!(doc.html.contains("<title>OKF Bundle Viewer</title>"));
        assert!(doc.html.contains("/viz/cytoscape.min.js"));
        assert!(doc.html.contains("\"My Bundle\""));
    }

    #[test]
    fn index_md_is_not_a_concept() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        make_bundle(&bundle);
        let out = tmp.path().join("viz.html");
        generate_visualization(&bundle, &out, None).unwrap();

        let graph = payload(&std::fs::read_to_string(&out).unwrap())["graph"].clone();
        let ids = node_ids(&graph);
        assert!(!ids.contains("index"));
        assert_eq!(
            ids,
            HashSet::from([
                "datasets/my_dataset".to_string(),
                "tables/users".to_string(),
                "tables/events".to_string(),
                "references/metrics/dau".to_string(),
            ])
        );
    }

    #[test]
    fn cross_links_become_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        make_bundle(&bundle);
        let out = tmp.path().join("viz.html");
        generate_visualization(&bundle, &out, None).unwrap();

        let graph = payload(&std::fs::read_to_string(&out).unwrap())["graph"].clone();
        let pairs = edge_pairs(&graph);
        let want = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert!(pairs.contains(&want("datasets/my_dataset", "tables/users")));
        assert!(pairs.contains(&want("tables/users", "tables/events")));
        assert!(pairs.contains(&want("tables/users", "references/metrics/dau")));
        assert!(pairs.contains(&want("tables/events", "tables/users")));
    }

    #[test]
    fn missing_link_targets_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        write(
            bundle.join("tables/lonely.md"),
            "---\ntype: BigQuery Table\ntitle: Lonely\ndescription: Has a dangling link.\n---\n\
             Links to [missing](/tables/missing.md).\n",
        );
        let out = tmp.path().join("viz.html");
        generate_visualization(&bundle, &out, None).unwrap();

        let graph = payload(&std::fs::read_to_string(&out).unwrap())["graph"].clone();
        assert!(graph["edges"].as_array().unwrap().is_empty());
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn node_colors_match_palette() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        make_bundle(&bundle);
        let out = tmp.path().join("viz.html");
        generate_visualization(&bundle, &out, None).unwrap();

        let graph = payload(&std::fs::read_to_string(&out).unwrap())["graph"].clone();
        let color = |id: &str| {
            graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["data"]["id"] == id)
                .unwrap()["data"]["color"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(color("datasets/my_dataset"), "#8b5cf6");
        assert_eq!(color("tables/users"), "#3b82f6");
        assert_eq!(color("references/metrics/dau"), "#10b981");
    }

    #[test]
    fn errors_when_bundle_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err =
            generate_visualization(&tmp.path().join("nope"), &tmp.path().join("viz.html"), None)
                .unwrap_err();
        assert!(matches!(err, OkfError::BundleNotFound { .. }));
    }

    #[test]
    fn script_breakout_in_body_is_neutralized() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        write(
            bundle.join("danger.md"),
            "---\ntype: Reference\ntitle: Danger\n---\nInline: </script><b>x</b>\n",
        );
        let out = tmp.path().join("viz.html");
        generate_visualization(&bundle, &out, None).unwrap();
        let html = std::fs::read_to_string(&out).unwrap();
        // The body's `</script>` is escaped inside the island — an unescaped one would
        // terminate the island early and corrupt the JSON (the round-trip below would
        // fail). It still decodes back to the real string for the viewer.
        assert!(html.contains("\\u003c/script>"));
        // And it still round-trips back to the real string for the viewer.
        let graph = payload(&html)["graph"].clone();
        assert!(graph["bodies"]["danger"]
            .as_str()
            .unwrap()
            .contains("</script>"));
    }
}
