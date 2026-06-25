//! `looper-ingest` — the ingestion seam (plan item 70).
//!
//! Turns a watched source file into the **markdown-envelope** the KB indexes, *before*
//! `kb.index`. Markdown is passed through unchanged; AsciiDoc is **metadata-lifted** — the
//! document header (the `= ` doctitle + `:name: value` attributes) becomes a synthesized
//! `---` YAML frontmatter, while the **body is kept byte-verbatim** (no transpilation, no
//! AsciiDoc parser). The bundle's file extension (carried by the concept id) is the format
//! marker; there is no `source_format` field.
//!
//! Dependency rule: a leaf domain crate — depends only on external crates. `looper-core`
//! calls [`prepare`] at its index choke-point. See `../../AGENTS.md`.

use serde_yaml_ng::{Mapping, Value};

/// An ingestion failure.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// The synthesized frontmatter could not be serialized to YAML.
    #[error("failed to serialize synthesized frontmatter: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

/// Prepares a watched source file into a markdown-envelope (`---` YAML frontmatter + body)
/// for the KB. Implementors keep the body verbatim and only synthesize/strip frontmatter.
pub trait Ingestor {
    /// File extensions (lowercase, no dot) this ingestor handles.
    fn extensions(&self) -> &'static [&'static str];

    /// Produce the markdown-envelope content to hand to `kb.index`. `concept_id` is the
    /// bundle-relative id (extension included, item 70) — used for a title fallback.
    ///
    /// # Errors
    /// [`IngestError`] if the synthesized frontmatter cannot be serialized.
    fn prepare(&self, raw: &str, concept_id: &str) -> Result<String, IngestError>;
}

/// Markdown (and unknown formats): identity. The KB already expects markdown-with-frontmatter.
pub struct MarkdownPassthrough;

impl Ingestor for MarkdownPassthrough {
    fn extensions(&self) -> &'static [&'static str] {
        &["md", "markdown", "mdx"]
    }

    fn prepare(&self, raw: &str, _concept_id: &str) -> Result<String, IngestError> {
        Ok(raw.to_string())
    }
}

/// AsciiDoc: lift the document header into YAML frontmatter, keep the body **verbatim**.
pub struct AdocIngestor;

impl Ingestor for AdocIngestor {
    fn extensions(&self) -> &'static [&'static str] {
        &["adoc", "asciidoc"]
    }

    fn prepare(&self, raw: &str, concept_id: &str) -> Result<String, IngestError> {
        // An author-written leading `---` YAML block (Jekyll/Hugo-style adoc) is already a
        // markdown-envelope — leave it for the KB's frontmatter parser, body untouched.
        if raw.starts_with("---\n") || raw.starts_with("---\r\n") {
            return Ok(raw.to_string());
        }

        let header = parse_adoc_header(raw);
        let mut frontmatter = Mapping::new();

        // Title: the `= ` doctitle, else the concept id's filename stem (item 70 keeps the
        // extension on the id, so strip it for a clean title) — the 1:1 analog of markdown's
        // first `# ` → filename.
        let title = header
            .doctitle
            .unwrap_or_else(|| filename_stem(concept_id).to_string());
        frontmatter.insert(Value::from("title"), Value::from(title));

        for (name, value) in header.attributes {
            if name.eq_ignore_ascii_case("keywords") || name.eq_ignore_ascii_case("tags") {
                let tags: Vec<Value> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(Value::from)
                    .collect();
                if !tags.is_empty() {
                    frontmatter.insert(Value::from("tags"), Value::Sequence(tags));
                }
            } else {
                frontmatter.insert(Value::from(name), Value::from(value));
            }
        }

        let yaml = serde_yaml_ng::to_string(&frontmatter)?;
        // The body is the *entire* raw AsciiDoc, unchanged — the doctitle/attribute lines stay
        // in it (read, not removed), exactly as markdown leaves its `# ` heading in the body.
        Ok(format!("---\n{yaml}---\n{raw}"))
    }
}

/// Prepare `raw` into the markdown-envelope the KB indexes, dispatching on `extension`
/// (lowercase or not, no dot). AsciiDoc is metadata-lifted; markdown and any other extension
/// pass through unchanged. `concept_id` carries the source extension (item 70).
///
/// # Errors
/// [`IngestError`] if the synthesized frontmatter cannot be serialized.
pub fn prepare(extension: &str, raw: &str, concept_id: &str) -> Result<String, IngestError> {
    let ext = extension.to_ascii_lowercase();
    if AdocIngestor.extensions().contains(&ext.as_str()) {
        AdocIngestor.prepare(raw, concept_id)
    } else {
        MarkdownPassthrough.prepare(raw, concept_id)
    }
}

/// The parsed AsciiDoc document header.
struct AdocHeader {
    doctitle: Option<String>,
    attributes: Vec<(String, String)>,
}

/// Parse the AsciiDoc document header: the level-0 `= ` doctitle plus `:name: value` attribute
/// entries, from the first non-blank/non-comment line until the first blank line (the header
/// terminator). Author/revision lines are skipped. No body is consumed.
fn parse_adoc_header(raw: &str) -> AdocHeader {
    let mut doctitle = None;
    let mut attributes = Vec::new();
    let mut started = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if !started {
            // Skip leading blank lines and `//` line comments before the header.
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            started = true;
        } else if trimmed.is_empty() {
            // The document header ends at the first blank line.
            break;
        }

        if doctitle.is_none() {
            if let Some(title) = doctitle_text(trimmed) {
                doctitle = Some(title.to_string());
                continue;
            }
        }
        if let Some((name, value)) = parse_attribute(trimmed) {
            attributes.push((name, value));
        }
    }

    AdocHeader {
        doctitle,
        attributes,
    }
}

/// The text of a level-0 doctitle (`= Title`), or `None`. `== ` (and deeper) are sections.
fn doctitle_text(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("= ")?;
    let title = rest.trim();
    (!title.is_empty()).then_some(title)
}

/// Parse an attribute entry: `:name: value` or `:name:` (→ empty). Returns `None` for unset
/// forms (`:name!:`, `:!name:`) and non-attribute lines.
fn parse_attribute(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(':')?;
    let end = rest.find(':')?;
    let name = &rest[..end];
    if name.is_empty()
        || name.starts_with('!')
        || name.ends_with('!')
        || name.contains(char::is_whitespace)
    {
        return None;
    }
    let value = rest[end + 1..].trim().to_string();
    Some((name.to_string(), value))
}

/// The filename stem of a concept id: its last `/` segment with a trailing extension stripped.
fn filename_stem(concept_id: &str) -> &str {
    let name = concept_id.rsplit('/').next().unwrap_or(concept_id);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_after_frontmatter(envelope: &str) -> &str {
        // Everything after the closing `---\n` of a leading frontmatter block.
        let rest = envelope.strip_prefix("---\n").expect("has frontmatter");
        let idx = rest.find("\n---\n").expect("closing delimiter");
        &rest[idx + "\n---\n".len()..]
    }

    #[test]
    fn markdown_passes_through_unchanged() {
        let src = "---\ntitle: X\n---\n# X\n\nbody\n";
        assert_eq!(prepare("md", src, "notes/x.md").unwrap(), src);
        // An unknown extension also passes through (never transpiled).
        assert_eq!(prepare("txt", "plain", "notes/x.txt").unwrap(), "plain");
    }

    #[test]
    fn adoc_lifts_doctitle_and_keeps_body_verbatim() {
        let raw = "= Beta\n\nworld\n";
        let out = prepare("adoc", raw, "notes/b.adoc").unwrap();
        assert!(out.starts_with("---\ntitle: Beta\n"));
        // The body is byte-identical to the source (the `= Beta` doctitle stays in it).
        assert_eq!(body_after_frontmatter(&out), raw);
    }

    #[test]
    fn adoc_lifts_keywords_and_attributes() {
        let raw = "= Guide\n:keywords: login, session\n:author: Jane\n\nbody\n";
        let out = prepare("asciidoc", raw, "docs/guide.adoc").unwrap();
        let doc = serde_yaml_ng::from_str::<Mapping>(
            out.strip_prefix("---\n")
                .unwrap()
                .split("\n---\n")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(doc.get("title").unwrap().as_str(), Some("Guide"));
        assert_eq!(doc.get("author").unwrap().as_str(), Some("Jane"));
        let tags: Vec<&str> = doc
            .get("tags")
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(tags, vec!["login", "session"]);
        assert_eq!(body_after_frontmatter(&out), raw);
    }

    #[test]
    fn adoc_without_doctitle_falls_back_to_filename_stem() {
        let raw = "no title here\n\njust body\n";
        let out = prepare("adoc", raw, "notes/sub/readme.adoc").unwrap();
        assert!(out.starts_with("---\ntitle: readme\n"), "got: {out}");
        assert_eq!(body_after_frontmatter(&out), raw);
    }

    #[test]
    fn adoc_with_existing_yaml_frontmatter_passes_through() {
        let raw = "---\ntitle: Pre\n---\n= Body\n";
        assert_eq!(prepare("adoc", raw, "x.adoc").unwrap(), raw);
    }

    #[test]
    fn section_headings_are_not_mistaken_for_the_doctitle() {
        // `== ` is a section, not a level-0 doctitle → filename-stem title, body verbatim.
        let raw = "== Section\n\nbody\n";
        let out = prepare("adoc", raw, "a/notes.adoc").unwrap();
        assert!(out.starts_with("---\ntitle: notes\n"), "got: {out}");
        assert_eq!(body_after_frontmatter(&out), raw);
    }
}
