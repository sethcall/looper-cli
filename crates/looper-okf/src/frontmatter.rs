//! OKF concept-document frontmatter: parse and emit, preserving key order and unknown
//! keys (so producer extensions like `okf_id` round-trip and a future Python OKF
//! consumer still reads our output). The ordered map is `serde_yaml_ng::Mapping`
//! (insertion-ordered), matching pyyaml `safe_dump(sort_keys=False)`.

use serde_yaml_ng::Mapping;

use crate::OkfError;

/// A parsed markdown document: an (ordered) YAML frontmatter mapping + the body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    /// The frontmatter mapping (empty if the source had none).
    pub frontmatter: Mapping,
    /// Everything after the frontmatter.
    pub body: String,
}

impl Document {
    /// Parse `content` into frontmatter + body. A leading `---` line opens the
    /// frontmatter, a `---` line closes it. Malformed YAML yields an empty mapping
    /// (the body is preserved).
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let after_open = content
            .strip_prefix("---\n")
            .or_else(|| content.strip_prefix("---\r\n"));
        if let Some(rest) = after_open {
            if let Some((yaml, body)) = split_frontmatter(rest) {
                let frontmatter = serde_yaml_ng::from_str(yaml).unwrap_or_default();
                return Self {
                    frontmatter,
                    body: body.to_string(),
                };
            }
        }
        Self {
            frontmatter: Mapping::new(),
            body: content.to_string(),
        }
    }

    /// Render back to markdown (`---` frontmatter + body). With empty frontmatter,
    /// returns the body unchanged.
    ///
    /// # Errors
    /// Returns [`OkfError::Yaml`] if the frontmatter cannot be serialized.
    pub fn to_markdown(&self) -> Result<String, OkfError> {
        if self.frontmatter.is_empty() {
            return Ok(self.body.clone());
        }
        let yaml = serde_yaml_ng::to_string(&self.frontmatter)?;
        Ok(format!("---\n{yaml}---\n{}", self.body))
    }
}

fn split_frontmatter(rest: &str) -> Option<(&str, &str)> {
    for delim in ["\n---\n", "\n---\r\n"] {
        if let Some(idx) = rest.find(delim) {
            return Some((&rest[..idx], &rest[idx + delim.len()..]));
        }
    }
    rest.strip_suffix("\n---").map(|yaml| (yaml, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_emit_preserves_key_order_and_body() {
        let src = "---\nzeta: 1\nalpha: 2\n---\n# Title\n\nbody text\n";
        let doc = Document::parse(src);
        assert_eq!(doc.body, "# Title\n\nbody text\n");
        // Key order is preserved (zeta before alpha, not sorted).
        let out = doc.to_markdown().unwrap();
        let z = out.find("zeta").unwrap();
        let a = out.find("alpha").unwrap();
        assert!(z < a, "insertion order must be preserved, got:\n{out}");
        assert!(out.starts_with("---\n"));
        assert!(out.ends_with("body text\n"));
    }

    #[test]
    fn no_frontmatter_is_passthrough() {
        let doc = Document::parse("just a body\n");
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.to_markdown().unwrap(), "just a body\n");
    }
}
