//! `looper-okf` — the OKF knowledge-base producer (a `looper_kb::Kb` backend).
//!
//! Emits OKF-conformant bundle markdown for each indexed source document: parses the
//! source frontmatter (preserving key order + unknown keys), ensures the required `type`
//! and a `title`, and writes producer extensions (`okf_id`, `okf_concept_id`,
//! `source_path`). A sidecar index (persisted via `looper-state`) maps source paths to
//! `{ okf_id, concept_id, content_hash }`; `okf_id` is preserved across re-indexing and
//! honored when present in the source frontmatter.
//!
//! Dependency rule: implements `looper-kb`; may depend on `looper-state`. Only
//! `looper-app` (the composition root) names this concrete backend. See `../../AGENTS.md`.
//! Implements plan item 12 (`../../specs/plan/12-kb-abstraction-and-okf-producer.md`).
//!
//! Out of scope for Milestone 1 (noted): the OKF git submodule + Python golden tests
//! (conformance is checked in Rust here), `description`/`timestamp` frontmatter, atomic
//! bundle writes, and move-stable `okf_id` via content-hash matching.

mod error;
mod frontmatter;
mod viz;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use looper_kb::{ConceptRef, DocSummary, Kb, KbError, KbProvider, SearchHit, SourceDoc, TagCount};
use looper_state::{Store, Versioned};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

pub use error::OkfError;
pub use frontmatter::Document;
pub use viz::{generate_visualization, render_visualization, VizDoc, VizStats};

/// The OKF producer: writes a bundle directory and maintains a sidecar index.
pub struct OkfKb {
    bundle_dir: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    store: Store<OkfIndex>,
    index: OkfIndex,
    /// In-memory search index: concept id -> searchable fields. Built from the bundle on
    /// open, kept in sync on index/remove. This is the "search index we maintain" — OKF
    /// itself keeps none (its `viz.html` does ad-hoc client-side substring matching).
    search: BTreeMap<String, SearchEntry>,
    /// Reverse tag index: tag -> the source paths carrying it. Built from the persisted
    /// `index.entries` (whose `labels` survive restarts) on open, and updated incrementally on
    /// index/remove/clear. Powers `tag_counts` + `documents_by_tag` (a catalog consumer)
    /// without rescanning the catalog.
    tags: BTreeMap<String, BTreeSet<PathBuf>>,
}

/// A concept's searchable fields. `haystack` is lowercased `title + concept_id + tags +
/// body` — a superset of `viz.html`'s title/id/tags substring search (it also matches body
/// text); `title` + `snippet` populate the returned hit.
struct SearchEntry {
    title: String,
    snippet: String,
    haystack: String,
    source_path: String,
    labels: Vec<String>,
}

/// The persisted sidecar index.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct OkfIndex {
    /// Monotonic sequence for minting new `okf_id`s.
    next_seq: u64,
    /// Source path -> entry.
    entries: BTreeMap<PathBuf, IndexEntry>,
}

impl Versioned for OkfIndex {
    const SCHEMA_VERSION: u32 = 1;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    okf_id: String,
    concept_id: String,
    content_hash: String,
    /// Display title, mirroring the **bundle** (KB output) — refreshed from it on open and on
    /// enrichment, and persisted so the catalog is available immediately. `#[serde(default)]`
    /// (→ `None`) keeps sidecars written before this field was added loadable.
    #[serde(default)]
    title: Option<String>,
    /// Frontmatter tags from the **bundle** (so enrichment-added tags are included), kept in sync
    /// alongside `title`. `#[serde(default)]` (→ empty) keeps older sidecars loadable.
    #[serde(default)]
    labels: Vec<String>,
}

impl OkfKb {
    /// Open the producer, creating `bundle_dir` and loading the sidecar index from
    /// `index_path`.
    ///
    /// # Errors
    /// Returns [`OkfError`] if the bundle dir cannot be created or the index cannot load.
    pub fn open(
        bundle_dir: impl Into<PathBuf>,
        index_path: impl Into<PathBuf>,
    ) -> Result<Self, OkfError> {
        let bundle_dir = bundle_dir.into();
        std::fs::create_dir_all(&bundle_dir).map_err(|source| OkfError::Io {
            path: bundle_dir.clone(),
            source,
        })?;
        let store = Store::new(index_path);
        let mut index = store.load()?;
        let search = build_search_index(&bundle_dir, &index);
        // The OKF bundle is the knowledge-base **output** and the source of truth for the catalog:
        // it carries enrichment-added tags (and any later augmentation) the original source
        // frontmatter lacks. Refresh each entry's cached title/labels from its freshly-parsed
        // bundle so `list_documents` / `tag_counts` reflect the current KB output. When a
        // bundle is missing/unreadable, keep the persisted values so the catalog still survives.
        let mut dirty = false;
        for entry in index.entries.values_mut() {
            if let Some(found) = search.get(&entry.concept_id) {
                if entry.title.as_deref() != Some(found.title.as_str())
                    || entry.labels != found.labels
                {
                    entry.title = Some(found.title.clone());
                    entry.labels = found.labels.clone();
                    dirty = true;
                }
            }
        }
        if dirty {
            store.save(&index)?;
        }
        let tags = build_tag_index(&index);
        Ok(Self {
            bundle_dir,
            inner: Mutex::new(Inner {
                store,
                index,
                search,
                tags,
            }),
        })
    }

    /// The bundle directory (where OKF concept docs are written; used by the viz renderer).
    #[must_use]
    pub fn bundle_dir(&self) -> &Path {
        &self.bundle_dir
    }

    fn concept_path(&self, concept_id: &str) -> PathBuf {
        self.bundle_dir.join(format!("{concept_id}.md"))
    }

    fn index_impl(&self, doc: &SourceDoc) -> Result<ConceptRef, OkfError> {
        let mut document = Document::parse(&doc.content);
        let content_hash = blake3::hash(doc.content.as_bytes()).to_hex().to_string();

        let mut guard = lock(&self.inner);
        let inner = &mut *guard;

        // okf_id: honor one already in the source frontmatter, else preserve the indexed
        // one for this source path, else mint a new one.
        let okf_id = if let Some(id) = frontmatter_str(&document.frontmatter, "okf_id") {
            id
        } else if let Some(entry) = inner.index.entries.get(&doc.source_path) {
            entry.okf_id.clone()
        } else {
            inner.index.next_seq += 1;
            format!("urn:looper:doc:{}", inner.index.next_seq)
        };

        ensure_okf_frontmatter(&mut document.frontmatter, &okf_id, doc, &document.body);

        write_bundle(&self.concept_path(&doc.concept_id), &document)?;

        let title = frontmatter_str(&document.frontmatter, "title")
            .unwrap_or_else(|| looper_kb::derive_title(&document.body, &doc.concept_id));
        let labels = frontmatter_tags(&document.frontmatter);

        // Reverse tag index: drop this path from its previous tags (labels may have changed on a
        // re-index), then add its current tags below.
        let prev_labels = inner
            .index
            .entries
            .get(&doc.source_path)
            .map(|e| e.labels.clone());
        if let Some(prev_labels) = prev_labels {
            for label in prev_labels {
                untag(&mut inner.tags, &label, &doc.source_path);
            }
        }

        inner.index.entries.insert(
            doc.source_path.clone(),
            IndexEntry {
                okf_id: okf_id.clone(),
                concept_id: doc.concept_id.clone(),
                content_hash,
                title: Some(title.clone()),
                labels: labels.clone(),
            },
        );
        inner.search.insert(
            doc.concept_id.clone(),
            search_entry(&doc.concept_id, &document.frontmatter, &document.body),
        );
        for label in &labels {
            inner
                .tags
                .entry(label.clone())
                .or_default()
                .insert(doc.source_path.clone());
        }
        inner.store.save(&inner.index)?;

        Ok(ConceptRef {
            okf_id,
            concept_id: doc.concept_id.clone(),
            title,
            labels,
        })
    }

    fn remove_impl(&self, source_path: &Path) -> Result<(), OkfError> {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        if let Some(entry) = inner.index.entries.remove(source_path) {
            let _ = std::fs::remove_file(self.concept_path(&entry.concept_id));
            inner.search.remove(&entry.concept_id);
            for label in &entry.labels {
                untag(&mut inner.tags, label, source_path);
            }
            inner.store.save(&inner.index)?;
        }
        Ok(())
    }

    fn clear_impl(&self) -> Result<(), OkfError> {
        let mut inner = lock(&self.inner);
        let concept_ids: Vec<String> = inner
            .index
            .entries
            .values()
            .map(|e| e.concept_id.clone())
            .collect();
        for concept_id in &concept_ids {
            let _ = std::fs::remove_file(self.concept_path(concept_id));
        }
        inner.index.entries.clear();
        inner.search.clear();
        inner.tags.clear();
        inner.store.save(&inner.index)?;
        Ok(())
    }

    fn list_documents_impl(&self) -> Vec<DocSummary> {
        let inner = lock(&self.inner);
        inner
            .index
            .entries
            .iter()
            .map(|(path, entry)| doc_summary(path, &self.concept_path(&entry.concept_id), entry))
            .collect()
    }

    fn tag_counts_impl(&self) -> Vec<TagCount> {
        let inner = lock(&self.inner);
        let mut counts: Vec<TagCount> = inner
            .tags
            .iter()
            .map(|(tag, paths)| TagCount {
                tag: tag.clone(),
                count: paths.len() as u32,
            })
            .collect();
        // Most-used first; ties broken by tag name for a stable, deterministic order.
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
        counts
    }

    fn documents_by_tag_impl(&self, tag: &str) -> Vec<DocSummary> {
        let inner = lock(&self.inner);
        match inner.tags.get(tag) {
            Some(paths) => paths
                .iter()
                .filter_map(|path| {
                    inner.index.entries.get(path).map(|entry| {
                        doc_summary(path, &self.concept_path(&entry.concept_id), entry)
                    })
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Re-sync the catalog for `source_path` from its bundle on disk (the KB output) — used after an
    /// external producer such as enrichment rewrites the bundle, so a catalog consumer's title/tags reflect
    /// the augmented output. Updates the cached entry, the search index, and the reverse
    /// tag index, then persists. Returns the refreshed concept ref, or `None` if the doc isn't
    /// indexed or its bundle can't be read.
    fn refresh_indexed_impl(&self, source_path: &Path) -> Option<ConceptRef> {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let (concept_id, okf_id, prev_labels) = inner
            .index
            .entries
            .get(source_path)
            .map(|e| (e.concept_id.clone(), e.okf_id.clone(), e.labels.clone()))?;
        let bundle = std::fs::read_to_string(self.concept_path(&concept_id)).ok()?;
        let document = Document::parse(&bundle);
        let title = frontmatter_str(&document.frontmatter, "title")
            .unwrap_or_else(|| looper_kb::derive_title(&document.body, &concept_id));
        let labels = frontmatter_tags(&document.frontmatter);

        for label in prev_labels {
            untag(&mut inner.tags, &label, source_path);
        }
        if let Some(entry) = inner.index.entries.get_mut(source_path) {
            entry.title = Some(title.clone());
            entry.labels = labels.clone();
        }
        for label in &labels {
            inner
                .tags
                .entry(label.clone())
                .or_default()
                .insert(source_path.to_path_buf());
        }
        inner.search.insert(
            concept_id.clone(),
            search_entry(&concept_id, &document.frontmatter, &document.body),
        );
        let _ = inner.store.save(&inner.index);

        Some(ConceptRef {
            okf_id,
            concept_id,
            title,
            labels,
        })
    }

    fn search_impl(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, OkfError> {
        let needle = query.to_lowercase();
        let inner = lock(&self.inner);
        // Query the maintained in-memory index (no per-keystroke disk reads); the BTreeMap
        // yields hits in stable concept-id order.
        let hits = inner
            .search
            .iter()
            .filter(|(_, entry)| entry.haystack.contains(&needle))
            .take(limit)
            .map(|(concept_id, entry)| SearchHit {
                concept_id: concept_id.clone(),
                title: entry.title.clone(),
                snippet: entry.snippet.clone(),
                path: entry.source_path.clone(),
                labels: entry.labels.clone(),
            })
            .collect();
        Ok(hits)
    }
}

impl Kb for OkfKb {
    fn name(&self) -> &str {
        "okf"
    }

    fn index(&self, doc: &SourceDoc) -> Result<ConceptRef, KbError> {
        self.index_impl(doc).map_err(into_kb_error)
    }

    fn remove(&self, source_path: &Path) -> Result<(), KbError> {
        self.remove_impl(source_path).map_err(into_kb_error)
    }

    fn doc_count(&self) -> usize {
        lock(&self.inner).index.entries.len()
    }

    fn source_paths(&self) -> Vec<String> {
        lock(&self.inner)
            .index
            .entries
            .keys()
            .map(|path| path.display().to_string())
            .collect()
    }

    fn clear(&self) -> Result<(), KbError> {
        self.clear_impl().map_err(into_kb_error)
    }

    fn read_indexed(&self, source_path: &Path) -> Option<String> {
        let concept_id = lock(&self.inner)
            .index
            .entries
            .get(source_path)
            .map(|entry| entry.concept_id.clone())?;
        std::fs::read_to_string(self.concept_path(&concept_id)).ok()
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, KbError> {
        self.search_impl(query, limit).map_err(into_kb_error)
    }

    fn list_documents(&self) -> Vec<DocSummary> {
        self.list_documents_impl()
    }

    fn tag_counts(&self) -> Vec<TagCount> {
        self.tag_counts_impl()
    }

    fn documents_by_tag(&self, tag: &str) -> Vec<DocSummary> {
        self.documents_by_tag_impl(tag)
    }

    fn refresh_indexed(&self, source_path: &Path) -> Option<ConceptRef> {
        self.refresh_indexed_impl(source_path)
    }
}

/// A [`KbProvider`] that opens an [`OkfKb`] per workspace, rooted at the workspace's
/// `kb_dir` (with an `okf-index.json` sidecar inside it). This is the concrete backend the
/// app injects into `looper-core` — the one place that names OKF.
#[derive(Debug, Default)]
pub struct OkfProvider;

impl KbProvider for OkfProvider {
    fn name(&self) -> &str {
        "okf"
    }

    fn open(&self, kb_dir: &Path) -> Result<Arc<dyn Kb>, KbError> {
        let kb = OkfKb::open(kb_dir, kb_dir.join("okf-index.json")).map_err(into_kb_error)?;
        Ok(Arc::new(kb))
    }
}

fn ensure_okf_frontmatter(fm: &mut Mapping, okf_id: &str, doc: &SourceDoc, body: &str) {
    // Required by OKF.
    if fm.get("type").is_none() {
        fm.insert(Value::from("type"), Value::from("Document"));
    }
    // Recommended; derive when absent.
    if fm.get("title").is_none() {
        let title = looper_kb::derive_title(body, &doc.concept_id);
        fm.insert(Value::from("title"), Value::from(title));
    }
    // Producer extensions (always refreshed; insert keeps an existing key's position).
    fm.insert(Value::from("okf_id"), Value::from(okf_id));
    fm.insert(
        Value::from("okf_concept_id"),
        Value::from(doc.concept_id.clone()),
    );
    fm.insert(
        Value::from("source_path"),
        Value::from(doc.source_path.to_string_lossy().into_owned()),
    );
}

fn write_bundle(path: &Path, document: &Document) -> Result<(), OkfError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| OkfError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let markdown = document.to_markdown()?;
    std::fs::write(path, markdown).map_err(|source| OkfError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn frontmatter_str(fm: &Mapping, key: &str) -> Option<String> {
    fm.get(key).and_then(Value::as_str).map(str::to_string)
}

fn frontmatter_tags(fm: &Mapping) -> Vec<String> {
    match fm.get("tags") {
        Some(Value::Sequence(seq)) => seq
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Build a concept's searchable entry from its frontmatter + body.
fn search_entry(concept_id: &str, fm: &Mapping, body: &str) -> SearchEntry {
    let title =
        frontmatter_str(fm, "title").unwrap_or_else(|| looper_kb::derive_title(body, concept_id));
    let labels = frontmatter_tags(fm);
    let tags = labels.join(" ");
    let haystack = format!("{title} {concept_id} {tags} {body}").to_lowercase();
    let snippet = body.chars().take(80).collect();
    SearchEntry {
        title,
        snippet,
        haystack,
        source_path: frontmatter_str(fm, "source_path").unwrap_or_default(),
        labels,
    }
}

/// Rebuild the in-memory search index by reading the bundle files named in the sidecar
/// index. This makes search work immediately after open — including across restarts,
/// without waiting for a re-scan to re-index unchanged files.
fn build_search_index(bundle_dir: &Path, index: &OkfIndex) -> BTreeMap<String, SearchEntry> {
    let mut search = BTreeMap::new();
    for entry in index.entries.values() {
        let path = bundle_dir.join(format!("{}.md", entry.concept_id));
        if let Ok(content) = std::fs::read_to_string(&path) {
            let doc = Document::parse(&content);
            search.insert(
                entry.concept_id.clone(),
                search_entry(&entry.concept_id, &doc.frontmatter, &doc.body),
            );
        }
    }
    search
}

/// Build a catalog entry from a sidecar index entry, its source path, and its bundle (KB output)
/// path. Callers surface the source path but keep the KB path so they can read the augmented
/// bundle for content.
fn doc_summary(source_path: &Path, kb_path: &Path, entry: &IndexEntry) -> DocSummary {
    DocSummary {
        path: source_path.display().to_string(),
        kb_path: kb_path.display().to_string(),
        title: entry.title.clone(),
        labels: entry.labels.clone(),
    }
}

/// Drop `path` from `tag`'s set in the reverse index, removing the tag entirely when it empties.
fn untag(tags: &mut BTreeMap<String, BTreeSet<PathBuf>>, tag: &str, path: &Path) {
    if let Some(set) = tags.get_mut(tag) {
        set.remove(path);
        if set.is_empty() {
            tags.remove(tag);
        }
    }
}

/// Build the reverse tag index (tag -> source paths) from the persisted sidecar entries. Their
/// `labels` survive restarts, so this needs no bundle parsing.
fn build_tag_index(index: &OkfIndex) -> BTreeMap<String, BTreeSet<PathBuf>> {
    let mut tags: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    for (path, entry) in &index.entries {
        for label in &entry.labels {
            tags.entry(label.clone()).or_default().insert(path.clone());
        }
    }
    tags
}

fn into_kb_error(err: OkfError) -> KbError {
    match err {
        OkfError::Io { path, source } => KbError::Io { path, source },
        other => KbError::Backend(other.to_string()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(source: &Path, concept: &str, content: &str) -> SourceDoc {
        SourceDoc {
            source_path: source.to_path_buf(),
            concept_id: concept.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn index_emits_conformant_bundle_and_preserves_okf_id() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let index = tmp.path().join("index.json");
        let src = tmp.path().join("docs/readme.md");

        let kb = OkfKb::open(&bundle, &index).unwrap();
        let r = kb
            .index(&doc(&src, "docs/readme", "# Readme\n\nhello\n"))
            .unwrap();
        assert_eq!(r.concept_id, "docs/readme");
        assert!(r.okf_id.starts_with("urn:looper:doc:"));

        // The bundle concept file exists and is OKF-conformant (parseable frontmatter,
        // required `type`, producer `okf_id`, derived `title`).
        let emitted = std::fs::read_to_string(bundle.join("docs/readme.md")).unwrap();
        let parsed = Document::parse(&emitted);
        assert_eq!(
            frontmatter_str(&parsed.frontmatter, "type").as_deref(),
            Some("Document")
        );
        assert_eq!(
            frontmatter_str(&parsed.frontmatter, "title").as_deref(),
            Some("Readme")
        );
        assert_eq!(
            frontmatter_str(&parsed.frontmatter, "okf_id"),
            Some(r.okf_id.clone())
        );
        // The source had no frontmatter, so the whole body (incl. the H1) is preserved;
        // `title` is derived from the H1 but the heading stays in the body.
        assert_eq!(parsed.body, "# Readme\n\nhello\n");

        // Re-index preserves okf_id across a content edit.
        let r2 = kb
            .index(&doc(&src, "docs/readme", "# Readme\n\nedited\n"))
            .unwrap();
        assert_eq!(r.okf_id, r2.okf_id);
    }

    #[test]
    fn honors_okf_id_in_source_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let kb = OkfKb::open(tmp.path().join("b"), tmp.path().join("i.json")).unwrap();
        let content = "---\nokf_id: urn:custom:42\ntype: Spec\n---\n# T\nbody\n";
        let r = kb
            .index(&doc(&tmp.path().join("a.md"), "a", content))
            .unwrap();
        assert_eq!(r.okf_id, "urn:custom:42");
        let emitted = std::fs::read_to_string(tmp.path().join("b/a.md")).unwrap();
        // Existing type is preserved (not overwritten with "Document").
        assert_eq!(
            frontmatter_str(&Document::parse(&emitted).frontmatter, "type").as_deref(),
            Some("Spec")
        );
    }

    #[test]
    fn read_indexed_returns_the_emitted_bundle_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let kb = OkfKb::open(tmp.path().join("b"), tmp.path().join("i.json")).unwrap();
        let src = tmp.path().join("a.md");
        kb.index(&doc(&src, "a", "# T\nbody\n")).unwrap();

        // The indexed view is the *emitted bundle* — producer frontmatter the source lacked, body kept.
        let indexed = kb.read_indexed(&src).expect("indexed doc is readable");
        assert!(
            indexed.contains("okf_id"),
            "indexed view carries producer metadata"
        );
        assert!(indexed.contains("body"), "indexed view keeps the body");

        assert!(kb.read_indexed(&tmp.path().join("missing.md")).is_none());
    }

    #[test]
    fn remove_search_and_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src_a = tmp.path().join("a.md");

        let r = {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(&tmp.path().join("b.md"), "b", "# B\nbanana\n"))
                .unwrap();
            kb.index(&doc(&src_a, "a", "# A\napple\n")).unwrap()
        };

        // Reopen: the persisted index keeps okf_ids stable.
        let kb = OkfKb::open(&bundle, &index).unwrap();
        let r2 = kb.index(&doc(&src_a, "a", "# A\napple again\n")).unwrap();
        assert_eq!(r.okf_id, r2.okf_id);

        let hits = kb.search("banana", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].concept_id, "b");

        kb.remove(&tmp.path().join("b.md")).unwrap();
        assert!(!bundle.join("b.md").exists());
        assert!(kb.search("banana", 10).unwrap().is_empty());
    }

    #[test]
    fn search_index_is_rebuilt_on_reopen_without_reindexing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(
                &tmp.path().join("x.md"),
                "notes/x",
                "# X\nplankton recipe\n",
            ))
            .unwrap();
        }
        // Reopen with no re-indexing: the in-memory search index is rebuilt from the bundle,
        // so content search works immediately (the restart case the engine relies on).
        let kb = OkfKb::open(&bundle, &index).unwrap();
        let by_body = kb.search("plankton", 10).unwrap();
        assert_eq!(by_body.len(), 1);
        assert_eq!(by_body[0].concept_id, "notes/x");
        // Title/id are searchable too.
        assert_eq!(kb.search("notes/x", 10).unwrap().len(), 1);
    }

    #[test]
    fn tag_counts_and_documents_by_tag_track_index_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let kb = OkfKb::open(tmp.path().join("b"), tmp.path().join("i.json")).unwrap();
        let src_a = tmp.path().join("a.md");
        let src_b = tmp.path().join("b.md");
        kb.index(&doc(&src_a, "a", "---\ntags: [api]\n---\n# A\n"))
            .unwrap();
        kb.index(&doc(&src_b, "b", "---\ntags: [api, infra]\n---\n# B\n"))
            .unwrap();

        // `api` on two docs, `infra` on one → most-used first.
        let counts = kb.tag_counts();
        assert_eq!(counts[0].tag, "api");
        assert_eq!(counts[0].count, 2);
        assert!(counts.iter().any(|c| c.tag == "infra" && c.count == 1));
        assert_eq!(kb.documents_by_tag("infra").len(), 1);
        assert_eq!(kb.documents_by_tag("api").len(), 2);

        // Re-index a.md without its tag → `api` drops to one doc.
        kb.index(&doc(&src_a, "a", "# A no tags\n")).unwrap();
        assert!(kb
            .tag_counts()
            .iter()
            .any(|c| c.tag == "api" && c.count == 1));

        // Removing b.md drops `api` and `infra` entirely.
        kb.remove(&src_b).unwrap();
        assert!(kb
            .tag_counts()
            .iter()
            .all(|c| c.tag != "api" && c.tag != "infra"));
    }

    #[test]
    fn catalog_survives_when_the_bundle_output_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src = tmp.path().join("a.md");
        {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(
                &src,
                "a",
                "---\ntags: [Product, Roadmap]\n---\n# A\nbody\n",
            ))
            .unwrap();
        }
        // Delete the emitted bundle output: with nothing to refresh from, the persisted sidecar
        // still answers catalog + tag queries (the catalog survives a missing output file).
        std::fs::remove_dir_all(&bundle).unwrap();
        let kb = OkfKb::open(&bundle, &index).unwrap();

        let docs = kb.list_documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].title.as_deref(), Some("A"));
        assert_eq!(
            docs[0].labels,
            vec!["Product".to_string(), "Roadmap".to_string()]
        );

        let counts = kb.tag_counts();
        assert_eq!(counts.len(), 2);
        assert!(counts.iter().any(|c| c.tag == "Product" && c.count == 1));
        assert_eq!(kb.documents_by_tag("Roadmap").len(), 1);
    }

    #[test]
    fn reopen_reflects_tags_added_to_the_bundle_output() {
        // Enrichment (or any producer) rewrites the bundle with extra tags after indexing. The
        // bundle is the KB output and authoritative: a reopen must surface those tags even though
        // the original source had none.
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src = tmp.path().join("a.md");
        {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(&src, "a", "# A\nbody\n")).unwrap();
            assert!(kb.tag_counts().is_empty());
        }
        std::fs::write(
            bundle.join("a.md"),
            "---\ntype: Document\ntitle: A\ntags:\n  - enriched\n---\n# A\nbody\n",
        )
        .unwrap();

        let kb = OkfKb::open(&bundle, &index).unwrap();
        assert!(kb.tag_counts().iter().any(|c| c.tag == "enriched"));
        assert_eq!(kb.list_documents()[0].labels, vec!["enriched".to_string()]);
    }

    #[test]
    fn refresh_indexed_picks_up_enrichment_tags_live() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src = tmp.path().join("a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();
        kb.index(&doc(&src, "a", "# A\nbody\n")).unwrap();
        assert!(kb.tag_counts().is_empty());

        // Simulate enrichment rewriting the bundle (the KB output) in place.
        std::fs::write(
            bundle.join("a.md"),
            "---\ntype: Document\ntitle: A\ntags:\n  - enriched\n  - looper\n---\n# A\nbody\n",
        )
        .unwrap();

        let cref = kb.refresh_indexed(&src).expect("doc is indexed");
        assert_eq!(
            cref.labels,
            vec!["enriched".to_string(), "looper".to_string()]
        );
        assert!(kb
            .tag_counts()
            .iter()
            .any(|c| c.tag == "enriched" && c.count == 1));
        assert_eq!(kb.documents_by_tag("looper").len(), 1);
        // The catalog's kb_path points at the bundle output file.
        assert!(kb.list_documents()[0].kb_path.ends_with("a.md"));

        // Not-indexed paths refresh to None.
        assert!(kb.refresh_indexed(&tmp.path().join("nope.md")).is_none());
    }

    #[test]
    fn old_sidecar_without_tags_is_backfilled_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src = tmp.path().join("a.md");
        {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(&src, "a", "---\ntags: [api]\n---\n# A\n"))
                .unwrap();
        }
        // Simulate a pre-item-67 sidecar: strip the persisted title/labels from the entry, as if
        // it had been written before those fields existed. The bundle on disk still has the tags.
        let raw = std::fs::read_to_string(&index).unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entries = json["data"]["entries"].as_object_mut().unwrap();
        for entry in entries.values_mut() {
            let obj = entry.as_object_mut().unwrap();
            obj.remove("title");
            obj.remove("labels");
        }
        std::fs::write(&index, serde_json::to_string(&json).unwrap()).unwrap();

        // Reopen: open() backfills title/labels from the bundle, so tag queries work again.
        let kb = OkfKb::open(&bundle, &index).unwrap();
        assert_eq!(kb.documents_by_tag("api").len(), 1);
        assert_eq!(kb.list_documents()[0].labels, vec!["api".to_string()]);
    }
}
