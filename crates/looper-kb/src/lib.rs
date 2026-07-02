//! `looper-kb` — the knowledge-base abstraction (swappable backend seam).
//!
//! `looper-core` depends on the [`Kb`] **trait** only; concrete backends (e.g.
//! `looper-okf`) are injected at the `looper-app` composition root. This keeps the KB
//! swappable, as required by `mvp.md`. Methods take `&self` (backends use interior
//! mutability) so the engine can share a backend as `Arc<dyn Kb>`.
//!
//! Dependency rule: **leaf** crate. See `../../AGENTS.md`.
//! Implements plan item 12 (`../../specs/plan/12-kb-abstraction-and-okf-producer.md`).

mod error;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub use error::KbError;

/// A source markdown document to be indexed into the knowledge base.
#[derive(Debug, Clone)]
pub struct SourceDoc {
    /// Absolute path of the source file (the index key; also recorded for sync).
    pub source_path: PathBuf,
    /// Bundle-relative concept id — the verbatim path relative to the workspace folder,
    /// **including** the file extension (e.g. `docs/readme.md`, `docs/guide.adoc`). The
    /// extension is the id's format discriminator + the bundle file name (item 70).
    pub concept_id: String,
    /// The raw markdown content (frontmatter + body).
    pub content: String,
}

/// A reference to an indexed concept, plus the display metadata the UI needs to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptRef {
    /// The stable, producer-owned identity (e.g. `urn:looper:doc:7`).
    pub okf_id: String,
    /// The bundle-relative concept id.
    pub concept_id: String,
    /// Display title as known to the index (frontmatter `title:` → first `# H1` → filename).
    pub title: String,
    /// Labels/tags from the document's frontmatter (`tags:`); empty when none.
    pub labels: Vec<String>,
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The concept id that matched.
    pub concept_id: String,
    /// The concept's title.
    pub title: String,
    /// A short snippet around the match.
    pub snippet: String,
    /// The source file path (so the UI can open the matched document).
    pub path: String,
    /// Labels/tags from the document's frontmatter.
    pub labels: Vec<String>,
}

/// A lightweight catalog entry for one indexed document — the whole-KB listing a catalog consumer
/// renders its file tree, tag filters, and heat map over. Cheaper than a search hit:
/// no snippet/body, just identity + display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSummary {
    /// The **source** file path (the UI's open key + tree path) — shown prominently to the user.
    pub path: String,
    /// The **knowledge-base output** path: the emitted bundle file this doc is indexed as. Callers
    /// read this (augmented) version for content; backends that emit no per-doc artifact repeat
    /// `path` here.
    pub kb_path: String,
    /// Display title (frontmatter `title:` → first `# H1` → filename), when known.
    pub title: Option<String>,
    /// Labels/tags from the **bundle** (KB output) frontmatter; empty when none.
    pub labels: Vec<String>,
}

/// One tag and how many indexed documents carry it — for tag-filter UIs, sourced from the
/// backend's tag → documents index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    /// The tag (a frontmatter label).
    pub tag: String,
    /// Number of indexed documents carrying this tag.
    pub count: u32,
}

/// A knowledge-base backend. Looper is the *producer*; implementations index source
/// documents, support removal, and answer searches.
pub trait Kb: Send + Sync {
    /// Stable backend identifier, e.g. `"okf"` or `"mock"`.
    fn name(&self) -> &str;

    /// Index (create or update) a document, returning its concept reference. The
    /// `okf_id` is preserved across re-indexing of the same source.
    ///
    /// # Errors
    /// Returns [`KbError`] on I/O or backend failure.
    fn index(&self, doc: &SourceDoc) -> Result<ConceptRef, KbError>;

    /// Remove the document indexed from `source_path`. A no-op if it is not indexed.
    ///
    /// # Errors
    /// Returns [`KbError`] on I/O or backend failure.
    fn remove(&self, source_path: &Path) -> Result<(), KbError>;

    /// Search indexed concepts, returning at most `limit` hits.
    ///
    /// # Errors
    /// Returns [`KbError`] on backend failure.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, KbError>;

    /// The number of documents currently in the index — the KB's true size, independent of any
    /// single session's indexing activity.
    fn doc_count(&self) -> usize;

    /// The absolute source path of every indexed document — used to count docs per source folder
    /// (the Activity carousel, item 45). Defaults to empty for backends that don't track it.
    fn source_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// Drop **every** document from the index (and its emitted artifacts), leaving an empty KB —
    /// the "rebuild from scratch" primitive (item 47). Defaults to a no-op.
    ///
    /// # Errors
    /// Returns [`KbError`] on backend failure.
    fn clear(&self) -> Result<(), KbError> {
        Ok(())
    }

    /// Read the **indexed** markdown for a source document (the emitted bundle file) — producer
    /// metadata + enrichment, unlike the raw source — or `None` if it isn't indexed (item 51).
    /// Defaults to `None` for backends that don't emit a per-doc artifact.
    fn read_indexed(&self, _source_path: &Path) -> Option<String> {
        None
    }

    /// Overwrite the **indexed** artifact for a source document (the editor's split-write target:
    /// the full doc including producer-owned fenced regions). Returns `Ok(false)` when the doc
    /// isn't indexed or the backend has no per-doc artifact (the default).
    ///
    /// # Errors
    /// Returns [`KbError`] on I/O or backend failure.
    fn write_indexed(&self, _source_path: &Path, _content: &str) -> Result<bool, KbError> {
        Ok(false)
    }

    /// Every indexed document as a lightweight catalog entry (path + title + labels) — the
    /// whole-KB listing callers browse/visualize. Defaults to
    /// empty for backends that don't track it.
    fn list_documents(&self) -> Vec<DocSummary> {
        Vec::new()
    }

    /// Every tag in the KB with its document count, from the backend's persisted tag → documents
    /// index — powers tag-filter UIs. Order is backend-defined.
    /// Defaults to empty.
    fn tag_counts(&self) -> Vec<TagCount> {
        Vec::new()
    }

    /// Catalog entries for every indexed document carrying `tag` (the reverse tag lookup).
    /// Defaults to empty.
    fn documents_by_tag(&self, _tag: &str) -> Vec<DocSummary> {
        Vec::new()
    }

    /// Re-sync the catalog (title + labels) for `source_path` from its **bundle output** on disk —
    /// used after an external producer such as enrichment rewrites the bundle, so a catalog consumer reflects
    /// the augmented KB output. Returns the refreshed concept ref, or `None` if the doc
    /// isn't indexed / has no readable bundle. Defaults to `None`.
    fn refresh_indexed(&self, _source_path: &Path) -> Option<ConceptRef> {
        None
    }
}

/// Doc extensions stripped from the filename when falling back to a title (item 70:
/// `concept_id` now carries the source extension, which should not show in a title).
const DOC_EXTENSIONS: [&str; 5] = ["md", "markdown", "mdx", "adoc", "asciidoc"];

/// Derive a display title from markdown content: the first ATX `# ` heading, else the
/// last segment of `concept_id` with a known doc extension stripped.
#[must_use]
pub fn derive_title(content: &str, concept_id: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }
    let name = concept_id.rsplit('/').next().unwrap_or(concept_id);
    strip_doc_extension(name).to_string()
}

/// The `title:` value from a leading `---` YAML frontmatter block, if present. A minimal,
/// dependency-free parse so `MockKb` reflects the same "explicit frontmatter `title` wins"
/// contract a real KB (`OkfKb`) honors — important once a producer/ingest step synthesizes
/// frontmatter (item 70).
fn frontmatter_title(content: &str) -> Option<String> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---\n").or_else(|| rest.find("\n---\r\n"))?;
    for line in rest[..end].lines() {
        if let Some(value) = line.strip_prefix("title:") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Strip a recognized doc extension (`.md`, `.adoc`, …) from a filename, if present.
fn strip_doc_extension(name: &str) -> &str {
    if let Some(dot) = name.rfind('.') {
        if DOC_EXTENSIONS
            .iter()
            .any(|ext| name[dot + 1..].eq_ignore_ascii_case(ext))
        {
            return &name[..dot];
        }
    }
    name
}

/// An in-memory no-op backend that exercises the DI seam in tests.
#[derive(Debug, Default)]
pub struct MockKb {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    seq: u64,
    by_source: HashMap<PathBuf, MockEntry>,
    /// Indexed artifacts written via [`Kb::write_indexed`] (in-memory stand-in for bundle files).
    indexed: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone)]
struct MockEntry {
    okf_id: String,
    concept_id: String,
    content: String,
}

impl Kb for MockKb {
    fn name(&self) -> &str {
        "mock"
    }

    fn read_indexed(&self, source_path: &Path) -> Option<String> {
        lock(&self.inner).indexed.get(source_path).cloned()
    }

    fn write_indexed(&self, source_path: &Path, content: &str) -> Result<bool, KbError> {
        let mut state = lock(&self.inner);
        if !state.by_source.contains_key(source_path) {
            return Ok(false);
        }
        state
            .indexed
            .insert(source_path.to_path_buf(), content.to_owned());
        Ok(true)
    }

    fn index(&self, doc: &SourceDoc) -> Result<ConceptRef, KbError> {
        let mut state = lock(&self.inner);
        let okf_id = match state.by_source.get(&doc.source_path) {
            Some(existing) => existing.okf_id.clone(),
            None => {
                state.seq += 1;
                format!("urn:looper:doc:{}", state.seq)
            }
        };
        state.by_source.insert(
            doc.source_path.clone(),
            MockEntry {
                okf_id: okf_id.clone(),
                concept_id: doc.concept_id.clone(),
                content: doc.content.clone(),
            },
        );
        Ok(ConceptRef {
            okf_id,
            concept_id: doc.concept_id.clone(),
            title: frontmatter_title(&doc.content)
                .unwrap_or_else(|| derive_title(&doc.content, &doc.concept_id)),
            labels: Vec::new(),
        })
    }

    fn remove(&self, source_path: &Path) -> Result<(), KbError> {
        lock(&self.inner).by_source.remove(source_path);
        Ok(())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, KbError> {
        let needle = query.to_lowercase();
        let state = lock(&self.inner);
        let hits = state
            .by_source
            .iter()
            .filter(|(_, e)| e.content.to_lowercase().contains(&needle))
            .take(limit)
            .map(|(path, e)| SearchHit {
                concept_id: e.concept_id.clone(),
                title: derive_title(&e.content, &e.concept_id),
                snippet: e.content.chars().take(80).collect(),
                path: path.to_string_lossy().into_owned(),
                labels: Vec::new(),
            })
            .collect();
        Ok(hits)
    }

    fn doc_count(&self) -> usize {
        lock(&self.inner).by_source.len()
    }

    fn list_documents(&self) -> Vec<DocSummary> {
        let state = lock(&self.inner);
        state
            .by_source
            .iter()
            .map(|(path, e)| {
                let p = path.to_string_lossy().into_owned();
                DocSummary {
                    kb_path: p.clone(),
                    path: p,
                    title: Some(derive_title(&e.content, &e.concept_id)),
                    labels: Vec::new(),
                }
            })
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Opens a [`Kb`] backend for a workspace's KB directory. The composition root injects a
/// provider so `looper-core` never names a concrete backend — each workspace gets its own
/// KB rooted at its `kb_dir`. This is the swappable-KB seam from `mvp.md`.
pub trait KbProvider: Send + Sync {
    /// Stable backend identifier (e.g. `"okf"`, `"mock"`).
    fn name(&self) -> &str;

    /// Open (creating if needed) the KB rooted at `kb_dir`.
    ///
    /// # Errors
    /// Returns [`KbError`] if the backend cannot be initialized.
    fn open(&self, kb_dir: &Path) -> Result<Arc<dyn Kb>, KbError>;
}

/// A [`KbProvider`] that yields a fresh in-memory [`MockKb`] per workspace (the DI seam in
/// tests; no files are written).
#[derive(Debug, Default)]
pub struct MockKbProvider;

impl KbProvider for MockKbProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn open(&self, _kb_dir: &Path) -> Result<Arc<dyn Kb>, KbError> {
        Ok(Arc::new(MockKb::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, concept: &str, content: &str) -> SourceDoc {
        SourceDoc {
            source_path: PathBuf::from(path),
            concept_id: concept.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn derive_title_prefers_h1_then_filename() {
        assert_eq!(derive_title("# Hello\n\nbody", "x/y"), "Hello");
        assert_eq!(derive_title("no heading", "x/readme"), "readme");
        // The id carries the source extension now (item 70) — strip it for the fallback title.
        assert_eq!(derive_title("no heading", "x/readme.md"), "readme");
        assert_eq!(derive_title("no heading", "docs/guide.adoc"), "guide");
        // A non-doc dotted name is left intact.
        assert_eq!(derive_title("no heading", "data.v2"), "data.v2");
    }

    #[test]
    fn mock_index_preserves_okf_id_and_searches() {
        let kb = MockKb::default();
        let a = kb
            .index(&doc("/ws/a.md", "a", "# Alpha\nhello world"))
            .unwrap();
        let b = kb.index(&doc("/ws/b.md", "b", "# Beta\ngoodbye")).unwrap();
        assert_ne!(a.okf_id, b.okf_id);

        // Re-indexing the same source preserves the okf_id.
        let a2 = kb.index(&doc("/ws/a.md", "a", "# Alpha\nedited")).unwrap();
        assert_eq!(a.okf_id, a2.okf_id);

        let hits = kb.search("goodbye", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].concept_id, "b");
        assert_eq!(hits[0].title, "Beta");

        kb.remove(Path::new("/ws/b.md")).unwrap();
        assert!(kb.search("goodbye", 10).unwrap().is_empty());
    }

    #[test]
    fn mock_is_usable_as_trait_object() {
        let kb: std::sync::Arc<dyn Kb> = std::sync::Arc::new(MockKb::default());
        assert_eq!(kb.name(), "mock");
        kb.index(&doc("/x.md", "x", "hi")).unwrap();
    }

    #[test]
    fn mock_index_prefers_frontmatter_title_then_falls_back() {
        let kb = MockKb::default();
        // An explicit frontmatter `title:` wins (as a real KB / item-70 ingest output).
        let r = kb
            .index(&doc(
                "/ws/g.adoc",
                "docs/g.adoc",
                "---\ntitle: Guide\n---\n= Guide\nbody",
            ))
            .unwrap();
        assert_eq!(r.title, "Guide");
        // No frontmatter title → filename stem (extension stripped).
        let r2 = kb
            .index(&doc("/ws/h.adoc", "notes/h.adoc", "= H\nbody"))
            .unwrap();
        assert_eq!(r2.title, "h");
    }
}
