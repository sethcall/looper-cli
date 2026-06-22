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
    /// Bundle-relative concept id without `.md` (e.g. `docs/readme`). The caller derives
    /// it from the source path relative to its workspace folder.
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
}

/// Derive a display title from markdown content: the first ATX `# ` heading, else the
/// last segment of `concept_id`.
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
    concept_id
        .rsplit('/')
        .next()
        .unwrap_or(concept_id)
        .to_string()
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
            title: derive_title(&doc.content, &doc.concept_id),
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
}
