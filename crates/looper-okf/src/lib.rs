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
pub mod fence;
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
    const SCHEMA_VERSION: u32 = 2;

    /// v1 → v2 (item 70 — native-extension bundles): `concept_id` was stored without the
    /// source file's extension; backfill it from each entry's source-path key. The old bundle
    /// file `{old_id}.md` now equals `{new_id}`, so **nothing on disk moves**. Idempotent
    /// (skips already-suffixed ids) and lenient (entries whose key has no extension are kept).
    fn migrate(
        from_version: u32,
        mut value: serde_json::Value,
    ) -> Result<serde_json::Value, looper_state::StateError> {
        if from_version != 1 {
            return Err(looper_state::StateError::UnsupportedVersion {
                found: from_version,
                current: Self::SCHEMA_VERSION,
            });
        }
        if let Some(entries) = value.get_mut("entries").and_then(|e| e.as_object_mut()) {
            for (source_path, entry) in entries.iter_mut() {
                let Some(ext) = Path::new(source_path).extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                let Some(obj) = entry.as_object_mut() else {
                    continue;
                };
                if let Some(concept_id) = obj.get("concept_id").and_then(|c| c.as_str()) {
                    let suffix = format!(".{ext}");
                    if !concept_id.ends_with(&suffix) {
                        let migrated = format!("{concept_id}{suffix}");
                        obj.insert("concept_id".into(), serde_json::Value::String(migrated));
                    }
                }
            }
        }
        Ok(value)
    }
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
    /// Source-content revision: bumped exactly when the indexed content hash changes (no-op
    /// re-indexes keep it). `#[serde(default)]` (→ 0 = pre-revision sidecar) keeps older sidecars
    /// loadable; the next index of such an entry backfills it to 1.
    #[serde(default)]
    revision: u64,
    /// ISO-8601 timestamp of the last content change (when `revision` last bumped).
    #[serde(default)]
    updated_at: Option<String>,
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
        // The concept id is the bundle-relative path verbatim (extension included), so the
        // bundle file is named exactly the id — no extension is appended (item 70).
        self.bundle_dir.join(concept_id)
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

        // Content revision + change timestamp (staleness metadata): bump on a content-hash
        // change, keep on a no-op re-index (rebuilds), backfill pre-revision sidecars to 1.
        let (revision, updated_at) = match inner.index.entries.get(&doc.source_path) {
            Some(entry) if entry.content_hash == content_hash && entry.revision > 0 => (
                entry.revision,
                entry.updated_at.clone().unwrap_or_else(utc_now_iso),
            ),
            Some(entry) => (entry.revision + 1, utc_now_iso()),
            None => (1, utc_now_iso()),
        };

        ensure_okf_frontmatter(
            &mut document.frontmatter,
            &okf_id,
            doc,
            &document.body,
            revision,
            &updated_at,
        );

        // Two authors, one output: regeneration replaces the bundle from source, so any
        // producer-owned fenced regions (and the frontmatter they added) in the PREVIOUS bundle
        // are carried forward mechanically — enrichment survives source edits with no re-derivation.
        let concept_path = self.concept_path(&doc.concept_id);
        carry_over_preserved(&mut document, &concept_path);

        write_bundle(&concept_path, &document)?;

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
                revision,
                updated_at: Some(updated_at),
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

    fn write_indexed(&self, source_path: &Path, content: &str) -> Result<bool, KbError> {
        let Some(concept_id) = lock(&self.inner)
            .index
            .entries
            .get(source_path)
            .map(|entry| entry.concept_id.clone())
        else {
            return Ok(false);
        };
        let path = self.concept_path(&concept_id);
        std::fs::write(&path, content)
            .map_err(|source| into_kb_error(OkfError::Io { path, source }))?;
        // Keep the catalog (title/labels/search) in step with the rewritten bundle.
        let _ = self.refresh_indexed_impl(source_path);
        Ok(true)
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

fn ensure_okf_frontmatter(
    fm: &mut Mapping,
    okf_id: &str,
    doc: &SourceDoc,
    body: &str,
    revision: u64,
    updated_at: &str,
) {
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
    // Staleness metadata (index-only; never written to source files): the source-content
    // revision and when it last changed. Enrichment pins its fence to `okf_revision` via a
    // `from-revision` attribute, so revision mismatch = stale enrichment.
    fm.insert(Value::from("okf_revision"), Value::from(revision));
    fm.insert(Value::from("okf_updated_at"), Value::from(updated_at));
}

/// Now as an ISO-8601 UTC string, dependency-free (Howard Hinnant's `civil_from_days`).
fn utc_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_unix_utc(i64::try_from(secs).unwrap_or(0))
}

fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1); // [1, 31]
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1); // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

/// Carry producer-owned content forward from the previous bundle at `previous_bundle` into a
/// regenerated `document`: preserved fenced regions (see [`fence::PRESERVED_CLASSES`]) are appended
/// to the body in their original order, and frontmatter keys the regenerated doc lacks are restored.
/// If the regenerated source body itself contains a preserved region, the source is authoritative
/// and nothing is carried over (no duplicates; an author-deleted region stays deleted). Carried
/// regions are appended at the end — mid-doc placement is not re-anchored (v1).
fn carry_over_preserved(document: &mut Document, previous_bundle: &Path) {
    let Ok(previous) = std::fs::read_to_string(previous_bundle) else {
        return;
    };
    let prev = Document::parse(&previous);
    let regions = fence::preserved_regions(&prev.body);
    if regions.is_empty() || fence::has_preserved(&document.body) {
        return;
    }
    let mut body = document.body.trim_end().to_owned();
    for region in &regions {
        body.push_str("\n\n");
        body.push_str(prev.body[region.start..region.end].trim_end());
    }
    body.push('\n');
    document.body = body;
    // Restore keys only the previous bundle has (enrichment-added `description`, `gemini_*`, …).
    // Keys the source (or the producer refresh) defines keep the regenerated value; a key at whole-
    // key granularity that the source deleted but enrichment had added comes back with the region.
    for (key, value) in &prev.frontmatter {
        if document.frontmatter.get(key).is_none() {
            document.frontmatter.insert(key.clone(), value.clone());
        }
    }
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
        let path = bundle_dir.join(&entry.concept_id);
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

    /// Simulate an enricher: append a preserved fence + enrichment frontmatter to the bundle,
    /// exactly as `looper-okf-enricher` writes it (the desktop repo's concrete producer).
    fn enrich_bundle(kb: &OkfKb, source: &Path) {
        let bundle = kb.read_indexed(source).unwrap();
        let mut document = Document::parse(&bundle);
        document
            .frontmatter
            .insert(Value::from("gemini_enriched_at"), Value::from("t"));
        document
            .frontmatter
            .insert(Value::from("description"), Value::from("AI summary."));
        document.body = format!(
            "{}\n\n::: {{.enrichment gemini-model=\"m\" enriched-at=\"t\" from-revision=\"1\"}}\n# AI Enrichment\n\n- a point\n\n:::\n",
            document.body.trim_end()
        );
        assert!(kb
            .write_indexed(source, &document.to_markdown().unwrap())
            .unwrap());
    }

    #[test]
    fn revision_bumps_on_content_change_and_holds_on_noop_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src_path = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        kb.index(&doc(&src_path, "docs/a.md", "# A\n\nv1\n"))
            .unwrap();
        let first = kb.read_indexed(&src_path).unwrap();
        assert!(first.contains("okf_revision: 1"));
        assert!(first.contains("okf_updated_at: "));

        // No-op re-index (a rebuild): revision and timestamp are unchanged.
        kb.index(&doc(&src_path, "docs/a.md", "# A\n\nv1\n"))
            .unwrap();
        assert_eq!(kb.read_indexed(&src_path).unwrap(), first);

        // Content change: revision bumps.
        kb.index(&doc(&src_path, "docs/a.md", "# A\n\nv2\n"))
            .unwrap();
        assert!(kb
            .read_indexed(&src_path)
            .unwrap()
            .contains("okf_revision: 2"));
    }

    #[test]
    fn reindex_carries_enrichment_fence_and_frontmatter_forward() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        kb.index(&doc(&src, "docs/a.md", "# A\n\noriginal body\n"))
            .unwrap();
        enrich_bundle(&kb, &src);

        // The source changes (any editor) → re-index regenerates the bundle…
        kb.index(&doc(&src, "docs/a.md", "# A\n\nEDITED body\n"))
            .unwrap();
        let out = kb.read_indexed(&src).unwrap();

        // …and the enrichment fence + enrichment-only frontmatter survive, no re-derivation.
        assert!(out.contains("EDITED body"));
        assert!(out.contains("::: {.enrichment gemini-model=\"m\""));
        assert!(out.contains("- a point"));
        assert!(out.contains("gemini_enriched_at: t"));
        assert!(out.contains("description: AI summary."));
        assert_eq!(out.matches("::: {.enrichment").count(), 1);
        // The staleness signal: the source edit bumped the doc revision while the carried fence
        // still pins the revision it was derived from — detectably stale, no API call spent.
        assert!(out.contains("okf_revision: 2"));
        assert!(out.contains("from-revision=\"1\""));
    }

    #[test]
    fn source_authored_preserved_fence_wins_over_carry_over() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        kb.index(&doc(&src, "docs/a.md", "# A\n\nbody\n")).unwrap();
        enrich_bundle(&kb, &src);

        let authored = "# A\n\n::: {.enrichment}\nauthor took ownership\n:::\n";
        kb.index(&doc(&src, "docs/a.md", authored)).unwrap();
        let out = kb.read_indexed(&src).unwrap();
        assert!(out.contains("author took ownership"));
        assert_eq!(out.matches("::: {.enrichment").count(), 1);
        assert!(!out.contains("- a point"));
    }

    #[test]
    fn source_frontmatter_wins_over_restored_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        kb.index(&doc(&src, "docs/a.md", "# A\n\nbody\n")).unwrap();
        enrich_bundle(&kb, &src);

        // The author adds their own description — it must beat the enrichment-added one.
        kb.index(&doc(
            &src,
            "docs/a.md",
            "---\ndescription: Author's own.\n---\n# A\n\nbody v2\n",
        ))
        .unwrap();
        let out = kb.read_indexed(&src).unwrap();
        assert!(out.contains("description: Author's own."));
        assert!(!out.contains("AI summary."));
        assert!(out.contains("gemini_enriched_at: t"));
    }

    #[test]
    fn unenriched_docs_reindex_exactly_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        kb.index(&doc(&src, "docs/a.md", "# A\n\nv1\n")).unwrap();
        kb.index(&doc(&src, "docs/a.md", "# A\n\nv2\n")).unwrap();
        let out = kb.read_indexed(&src).unwrap();
        assert!(out.contains("v2") && !out.contains("v1"));
        assert!(!out.contains(":::"));
    }

    #[test]
    fn write_indexed_requires_an_indexed_doc_and_refreshes_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let (bundle, index) = (tmp.path().join("b"), tmp.path().join("i.json"));
        let src = tmp.path().join("docs/a.md");
        let kb = OkfKb::open(&bundle, &index).unwrap();

        assert!(!kb.write_indexed(&src, "x").unwrap());
        kb.index(&doc(&src, "docs/a.md", "# A\n\nbody\n")).unwrap();
        assert!(kb
            .write_indexed(&src, "---\ntitle: A\ntags:\n- fresh\n---\n# A\n\nbody\n")
            .unwrap());
        let labels: Vec<String> = kb.tag_counts().into_iter().map(|t| t.tag).collect();
        assert!(labels.contains(&"fresh".to_string()));
    }

    #[test]
    fn index_emits_conformant_bundle_and_preserves_okf_id() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let index = tmp.path().join("index.json");
        let src = tmp.path().join("docs/readme.md");

        let kb = OkfKb::open(&bundle, &index).unwrap();
        let r = kb
            .index(&doc(&src, "docs/readme.md", "# Readme\n\nhello\n"))
            .unwrap();
        assert_eq!(r.concept_id, "docs/readme.md");
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
            .index(&doc(&src, "docs/readme.md", "# Readme\n\nedited\n"))
            .unwrap();
        assert_eq!(r.okf_id, r2.okf_id);
    }

    #[test]
    fn honors_okf_id_in_source_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let kb = OkfKb::open(tmp.path().join("b"), tmp.path().join("i.json")).unwrap();
        let content = "---\nokf_id: urn:custom:42\ntype: Spec\n---\n# T\nbody\n";
        let r = kb
            .index(&doc(&tmp.path().join("a.md"), "a.md", content))
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
        kb.index(&doc(&src, "a.md", "# T\nbody\n")).unwrap();

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
            kb.index(&doc(&tmp.path().join("b.md"), "b.md", "# B\nbanana\n"))
                .unwrap();
            kb.index(&doc(&src_a, "a.md", "# A\napple\n")).unwrap()
        };

        // Reopen: the persisted index keeps okf_ids stable.
        let kb = OkfKb::open(&bundle, &index).unwrap();
        let r2 = kb
            .index(&doc(&src_a, "a.md", "# A\napple again\n"))
            .unwrap();
        assert_eq!(r.okf_id, r2.okf_id);

        let hits = kb.search("banana", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].concept_id, "b.md");

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
                "notes/x.md",
                "# X\nplankton recipe\n",
            ))
            .unwrap();
        }
        // Reopen with no re-indexing: the in-memory search index is rebuilt from the bundle,
        // so content search works immediately (the restart case the engine relies on).
        let kb = OkfKb::open(&bundle, &index).unwrap();
        let by_body = kb.search("plankton", 10).unwrap();
        assert_eq!(by_body.len(), 1);
        assert_eq!(by_body[0].concept_id, "notes/x.md");
        // Title/id are searchable too.
        assert_eq!(kb.search("notes/x", 10).unwrap().len(), 1);
    }

    #[test]
    fn tag_counts_and_documents_by_tag_track_index_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let kb = OkfKb::open(tmp.path().join("b"), tmp.path().join("i.json")).unwrap();
        let src_a = tmp.path().join("a.md");
        let src_b = tmp.path().join("b.md");
        kb.index(&doc(&src_a, "a.md", "---\ntags: [api]\n---\n# A\n"))
            .unwrap();
        kb.index(&doc(&src_b, "b.md", "---\ntags: [api, infra]\n---\n# B\n"))
            .unwrap();

        // `api` on two docs, `infra` on one → most-used first.
        let counts = kb.tag_counts();
        assert_eq!(counts[0].tag, "api");
        assert_eq!(counts[0].count, 2);
        assert!(counts.iter().any(|c| c.tag == "infra" && c.count == 1));
        assert_eq!(kb.documents_by_tag("infra").len(), 1);
        assert_eq!(kb.documents_by_tag("api").len(), 2);

        // Re-index a.md without its tag → `api` drops to one doc.
        kb.index(&doc(&src_a, "a.md", "# A no tags\n")).unwrap();
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
                "a.md",
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
            kb.index(&doc(&src, "a.md", "# A\nbody\n")).unwrap();
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
        kb.index(&doc(&src, "a.md", "# A\nbody\n")).unwrap();
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
            kb.index(&doc(&src, "a.md", "---\ntags: [api]\n---\n# A\n"))
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

    #[test]
    fn sidecar_v1_concept_ids_are_backfilled_with_extension_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b");
        let index = tmp.path().join("i.json");
        let src = tmp.path().join("notes/x.md");
        {
            let kb = OkfKb::open(&bundle, &index).unwrap();
            kb.index(&doc(&src, "notes/x.md", "# X\nplankton\n"))
                .unwrap();
        }
        // Rewrite the sidecar as a pre-item-70 v1: schema_version 1 + an extension-less
        // concept_id. The bundle file on disk stays `b/notes/x.md` — exactly what the migrated
        // id (`notes/x.md`) maps to, so nothing has to move.
        let raw = std::fs::read_to_string(&index).unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        json["schema_version"] = serde_json::json!(1);
        for entry in json["data"]["entries"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            entry["concept_id"] = serde_json::json!("notes/x");
        }
        std::fs::write(&index, serde_json::to_string(&json).unwrap()).unwrap();

        // Reopen: the v1→v2 migration backfills the extension; search resolves via the bundle.
        let kb = OkfKb::open(&bundle, &index).unwrap();
        let hits = kb.search("plankton", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].concept_id, "notes/x.md");
    }
}
