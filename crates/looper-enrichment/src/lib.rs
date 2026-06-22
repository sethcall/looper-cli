//! `looper-enrichment` — the enrichment seam (engine-facing, pure Rust).
//!
//! Defines the [`Enricher`] trait the engine drives, its error + result types, and a subprocess-free
//! [`MockEnricher`] for tests/the engine. The implementor resolves its **own** Gemini API key from
//! the given source/workspace, so the engine never handles the key. The concrete, LLM-bound enricher
//! (shells out to the vendored OKF Python tool) lives in `looper-okf-enricher` and is injected by the
//! desktop app. Mirrors the `looper-kb`/`looper-sync` trait seams. `core → enrichment`; never depends
//! on `core`/`app`.

use std::path::{Path, PathBuf};

pub use looper_ipc::{EnrichmentApiKeySource, EnrichmentConfig, EnrichmentDelta};

/// Errors from running enrichment.
#[derive(Debug, thiserror::Error)]
pub enum EnrichError {
    /// `GEMINI_API_KEY` was empty / not resolvable.
    #[error("no Gemini API key configured")]
    MissingApiKey,
    /// The enricher process could not be started (missing python/script/venv).
    #[error("could not start the enrichment tool: {0}")]
    Spawn(String),
    /// The enricher ran but exited non-zero.
    #[error("enrichment tool failed: {0}")]
    Tool(String),
    /// The enricher exceeded its time budget and was killed.
    #[error("enrichment timed out")]
    Timeout,
    /// An OS-keychain operation (reading/writing the specified key) failed.
    #[error("keychain error: {0}")]
    Keychain(String),
    /// Reading the bundle doc before/after failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The enrichment seam: enrich one bundle doc, or every doc in the bundle.
///
/// `bundle_dir` is the KB bundle root; `concept_id` is the bundle-relative doc id (no `.md`, e.g.
/// `vf/AGENTS`). The implementor resolves its own API key from `source`/`workspace_id` — the engine
/// passes the *source*, never a key — and rejects a missing key with [`EnrichError::MissingApiKey`].
pub trait Enricher: Send + Sync {
    /// Run enrichment for one doc, returning the delta (tags + content sections added).
    fn enrich(
        &self,
        bundle_dir: &Path,
        concept_id: &str,
        source: EnrichmentApiKeySource,
        workspace_id: &str,
    ) -> Result<EnrichmentDelta, EnrichError>;

    /// Enrich **every** doc in the bundle in one batch run, returning only the docs that actually
    /// changed (each keyed by its source path, for per-doc UI events). Drives the after-scan hook.
    fn enrich_all(
        &self,
        bundle_dir: &Path,
        source: EnrichmentApiKeySource,
        workspace_id: &str,
    ) -> Result<Vec<EnrichedDoc>, EnrichError>;
}

/// One document's outcome from a batch [`Enricher::enrich_all`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichedDoc {
    /// The doc's original source path (from its `source_path` frontmatter) — the key the UI uses
    /// for per-doc status; falls back to the bundle concept id when frontmatter lacks it.
    pub source_path: String,
    /// What enrichment added to this doc.
    pub delta: EnrichmentDelta,
}

// ---- MockEnricher (test/seam; no subprocess) ----------------------------------------

/// A subprocess-free [`Enricher`] for tests and the engine: returns scripted deltas and counts calls.
/// Ignores the API-key source/workspace entirely (so tests never touch the env or keychain).
#[derive(Debug, Default)]
pub struct MockEnricher {
    scripted: std::sync::Mutex<std::collections::VecDeque<EnrichmentDelta>>,
    calls: std::sync::Mutex<Vec<(PathBuf, String)>>,
    scripted_all: std::sync::Mutex<std::collections::VecDeque<Vec<EnrichedDoc>>>,
    all_calls: std::sync::Mutex<Vec<PathBuf>>,
}

impl MockEnricher {
    /// A mock that returns `EnrichmentDelta::default()` for every call.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a delta to return from the next `enrich` call (FIFO).
    pub fn script(&self, delta: EnrichmentDelta) {
        self.scripted.lock().expect("mock lock").push_back(delta);
    }

    /// Queue the docs returned by the next `enrich_all` call (FIFO).
    pub fn script_all(&self, docs: Vec<EnrichedDoc>) {
        self.scripted_all.lock().expect("mock lock").push_back(docs);
    }

    /// The `(bundle_dir, concept_id)` pairs `enrich` was called with, in order.
    pub fn calls(&self) -> Vec<(PathBuf, String)> {
        self.calls.lock().expect("mock lock").clone()
    }

    /// The bundle dirs `enrich_all` was called with, in order.
    pub fn all_calls(&self) -> Vec<PathBuf> {
        self.all_calls.lock().expect("mock lock").clone()
    }
}

impl Enricher for MockEnricher {
    fn enrich(
        &self,
        bundle_dir: &Path,
        concept_id: &str,
        _source: EnrichmentApiKeySource,
        _workspace_id: &str,
    ) -> Result<EnrichmentDelta, EnrichError> {
        self.calls
            .lock()
            .expect("mock lock")
            .push((bundle_dir.to_path_buf(), concept_id.to_owned()));
        Ok(self
            .scripted
            .lock()
            .expect("mock lock")
            .pop_front()
            .unwrap_or_default())
    }

    fn enrich_all(
        &self,
        bundle_dir: &Path,
        _source: EnrichmentApiKeySource,
        _workspace_id: &str,
    ) -> Result<Vec<EnrichedDoc>, EnrichError> {
        self.all_calls
            .lock()
            .expect("mock lock")
            .push(bundle_dir.to_path_buf());
        Ok(self
            .scripted_all
            .lock()
            .expect("mock lock")
            .pop_front()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_enricher_scripts_deltas_and_records_calls() {
        let mock = MockEnricher::new();
        mock.script(EnrichmentDelta {
            labels_added: vec!["x".into()],
            content_added: vec![],
        });
        let out = mock
            .enrich(Path::new("/kb"), "a/b", EnrichmentApiKeySource::Env, "ws")
            .unwrap();
        assert_eq!(out.labels_added, vec!["x"]);
        // Falls back to an empty delta once the script is drained.
        let empty = mock
            .enrich(Path::new("/kb"), "c/d", EnrichmentApiKeySource::Env, "ws")
            .unwrap();
        assert!(empty.labels_added.is_empty());
        assert_eq!(
            mock.calls(),
            vec![
                (PathBuf::from("/kb"), "a/b".to_owned()),
                (PathBuf::from("/kb"), "c/d".to_owned()),
            ]
        );
    }

    #[test]
    fn mock_enrich_all_scripts_docs_and_records_calls() {
        let mock = MockEnricher::new();
        mock.script_all(vec![EnrichedDoc {
            source_path: "/ws/a.md".into(),
            delta: EnrichmentDelta {
                labels_added: vec!["t".into()],
                content_added: vec![],
            },
        }]);
        let out = mock
            .enrich_all(Path::new("/kb"), EnrichmentApiKeySource::Env, "ws")
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_path, "/ws/a.md");
        // Drained → empty.
        assert!(mock
            .enrich_all(Path::new("/kb2"), EnrichmentApiKeySource::Env, "ws")
            .unwrap()
            .is_empty());
        assert_eq!(
            mock.all_calls(),
            vec![PathBuf::from("/kb"), PathBuf::from("/kb2")]
        );
    }
}
