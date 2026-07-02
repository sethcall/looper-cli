//! Pandoc fenced-div grammar for **producer-owned regions** in bundle docs.
//!
//! A producer (e.g. an enricher) marks the content it owns with a fenced div —
//! `::: {.enrichment key="value"}` … `:::` — so the region is mechanically
//! separable from author content: the indexer carries preserved regions forward
//! when a source edit regenerates the bundle, and the editor's split-write strips
//! them from what goes back to the source file.
//!
//! Grammar (keep in sync with the desktop editor/viewer's TS grammar in
//! `adoc-editor/src/browser/interaction/dialect/fenced-div.ts`): opener = a
//! column-0 line of 3+ colons plus a non-empty info segment (a `{…}` attribute
//! block or a bare word; trailing colons allowed); closer = a column-0 line of 3+
//! colons only; regions are depth-matched, and `:::` lines inside fenced code
//! blocks are literal text.

/// Fence classes whose regions are producer-owned and preserved across bundle
/// regeneration. `enrichment` is the enrichment seam's region class.
pub const PRESERVED_CLASSES: &[&str] = &["enrichment"];

/// Byte range of a fenced region within a body: the whole region (opener line
/// start → closer line end, or end of input when unclosed) plus the inner content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Byte offset of the opener line's first character.
    pub start: usize,
    /// Byte offset just past the opener line's newline (inner content start).
    pub inner_start: usize,
    /// Byte offset of the inner content's end (before the closer's newline).
    pub inner_end: usize,
    /// Byte offset just past the closer line (or end of input when unclosed).
    pub end: usize,
}

/// Opening fence: the info segment after the colon run, or `None` when the line is
/// not an opener (no colon run, empty info, or info that is itself all colons — a
/// closer).
fn opener_info(line: &str) -> Option<&str> {
    let colons = line.len() - line.trim_start_matches(':').len();
    if colons < 3 {
        return None;
    }
    let info = &line[colons..];
    let meat = info.trim();
    if meat.is_empty() || meat.chars().all(|c| c == ':') {
        return None;
    }
    Some(info)
}

/// Closing fence: a line of 3+ colons and nothing else.
fn is_closer(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == ':')
}

/// Whitespace-split tokens of a brace-form info segment, keeping quoted values
/// (which may contain spaces) intact. A bare-word opener has no brace tokens.
fn tokens_of_info(info: &str) -> Vec<String> {
    let meat = info
        .trim()
        .trim_end_matches(|c: char| c == ':' || c.is_whitespace());
    let Some(inner) = meat.strip_prefix('{') else {
        return Vec::new();
    };
    let inner = inner.strip_suffix('}').unwrap_or(inner);
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == q {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            current.push(ch);
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Whether an opener's info segment carries the given class (`{.name …}` brace
/// form, or the bare word itself).
fn info_has_class(info: &str, class: &str) -> bool {
    let tokens = tokens_of_info(info);
    if tokens.is_empty() {
        let meat = info
            .trim()
            .trim_end_matches(|c: char| c == ':' || c.is_whitespace());
        return !meat.starts_with('{') && meat.split_whitespace().next() == Some(class);
    }
    tokens
        .iter()
        .any(|t| t.strip_prefix('.').is_some_and(|name| name == class))
}

fn info_has_preserved_class(info: &str) -> bool {
    PRESERVED_CLASSES
        .iter()
        .any(|class| info_has_class(info, class))
}

/// Fenced-code tracking: a ``` / ~~~ fence (0–3 spaces of indent) shields
/// everything until its matching closer, so `:::` lines inside code stay literal.
#[derive(Clone, Copy)]
struct CodeFence {
    ch: char,
    len: usize,
}

fn code_fence_open(line: &str) -> Option<CodeFence> {
    let stripped = line
        .strip_prefix("   ")
        .or_else(|| line.strip_prefix("  "))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line);
    let first = stripped.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = stripped.chars().take_while(|&c| c == first).count();
    (len >= 3).then_some(CodeFence { ch: first, len })
}

fn code_fence_closes(line: &str, fence: CodeFence) -> bool {
    let stripped = line
        .strip_prefix("   ")
        .or_else(|| line.strip_prefix("  "))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line);
    let run = stripped.chars().take_while(|&c| c == fence.ch).count();
    run >= fence.len && stripped[run..].trim().is_empty()
}

/// All fenced regions whose opener's info matches `matches`, in document order.
/// Depth-matched (nested divs stay inside their region) and code-fence-aware; an
/// opener with no matching closer runs to end of input.
fn regions_matching(body: &str, matches: impl Fn(&str) -> bool) -> Vec<Region> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in body.split('\n') {
        lines.push((offset, line));
        offset += line.len() + 1;
    }

    let mut regions = Vec::new();
    let mut code: Option<CodeFence> = None;
    let mut i = 0;
    while i < lines.len() {
        let (line_start, line) = lines[i];
        if let Some(fence) = code {
            if code_fence_closes(line, fence) {
                code = None;
            }
            i += 1;
            continue;
        }
        if let Some(fence) = code_fence_open(line) {
            code = Some(fence);
            i += 1;
            continue;
        }
        let Some(info) = opener_info(line) else {
            i += 1;
            continue;
        };
        if !matches(info) {
            i += 1;
            continue;
        }
        // Scan to the depth-matched closer, code-fence-aware.
        let inner_start = (line_start + line.len() + 1).min(body.len());
        let mut depth = 1usize;
        let mut inner_code: Option<CodeFence> = None;
        let mut region = Region {
            start: line_start,
            inner_start,
            inner_end: body.len(),
            end: body.len(),
        };
        let mut j = i + 1;
        while j < lines.len() {
            let (close_start, candidate) = lines[j];
            if let Some(fence) = inner_code {
                if code_fence_closes(candidate, fence) {
                    inner_code = None;
                }
            } else if let Some(fence) = code_fence_open(candidate) {
                inner_code = Some(fence);
            } else if opener_info(candidate).is_some() {
                depth += 1;
            } else if is_closer(candidate) {
                depth -= 1;
                if depth == 0 {
                    region.inner_end = close_start.saturating_sub(1).max(region.inner_start);
                    region.end = close_start + candidate.len();
                    break;
                }
            }
            j += 1;
        }
        regions.push(region);
        i = j + 1;
    }
    regions
}

/// All fenced regions carrying `class`, in document order.
#[must_use]
pub fn regions_with_class(body: &str, class: &str) -> Vec<Region> {
    regions_matching(body, |info| info_has_class(info, class))
}

/// All producer-owned regions (any class in [`PRESERVED_CLASSES`]), in document order.
#[must_use]
pub fn preserved_regions(body: &str) -> Vec<Region> {
    regions_matching(body, info_has_preserved_class)
}

/// The unquoted value of a `key="value"` attribute on a region's opener line, if present.
#[must_use]
pub fn opener_attr(body: &str, region: &Region, key: &str) -> Option<String> {
    let opener_end = body[region.start..]
        .find('\n')
        .map_or(body.len(), |i| region.start + i);
    let info = opener_info(&body[region.start..opener_end])?;
    tokens_of_info(info).into_iter().find_map(|token| {
        let value = token.strip_prefix(key)?.strip_prefix('=')?;
        Some(value.trim_matches(|c| c == '"' || c == '\'').to_owned())
    })
}

/// Whether `body` contains any producer-owned region.
#[must_use]
pub fn has_preserved(body: &str) -> bool {
    !preserved_regions(body).is_empty()
}

/// `body` with every producer-owned region removed (the split-write's source side),
/// trimmed of the blank lines the removals leave behind at the edges.
#[must_use]
pub fn strip_preserved(body: &str) -> String {
    let mut out = body.to_owned();
    while let Some(region) = preserved_regions(&out).first().copied() {
        // The opener sits on its own line (the prefix ends at a line break), so the newlines
        // after the region are the splice's leftovers — consume them all to avoid doubling
        // the blank separation that surrounded the region.
        let mut end = region.end;
        while out[end..].starts_with('\n') {
            end += 1;
        }
        out = format!("{}{}", &out[..region.start], &out[end..]);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region_text(body: &str) -> Option<(&str, &str)> {
        preserved_regions(body)
            .last()
            .map(|r| (&body[r.start..r.end], &body[r.inner_start..r.inner_end]))
    }

    #[test]
    fn finds_a_basic_enrichment_region() {
        let body =
            "intro\n\n::: {.enrichment gemini-model=\"m\"}\n# AI Enrichment\n\n- a\n:::\n\ntail\n";
        let (region, inner) = region_text(body).unwrap();
        assert!(region.starts_with("::: {.enrichment"));
        assert!(region.ends_with(":::"));
        assert_eq!(inner.trim(), "# AI Enrichment\n\n- a");
    }

    #[test]
    fn non_preserved_fences_do_not_match() {
        assert!(preserved_regions("::: note\nbody\n:::\n").is_empty());
        assert!(preserved_regions("::: {.other}\nbody\n:::\n").is_empty());
        // …but a bare-word `enrichment` fence counts (the class is the contract).
        assert!(!preserved_regions("::: enrichment\nbody\n:::\n").is_empty());
    }

    #[test]
    fn code_blocks_cannot_spoof_a_region() {
        let body = "```\n::: {.enrichment}\n:::\n```\n";
        assert!(preserved_regions(body).is_empty());
    }

    #[test]
    fn nested_divs_stay_inside_the_region() {
        let body = "::: {.enrichment}\n::: {.inner}\ndeep\n:::\nafter inner\n:::\ntail\n";
        let (region, inner) = region_text(body).unwrap();
        assert!(inner.contains("after inner"));
        assert!(!region.contains("tail"));
    }

    #[test]
    fn code_inside_the_region_shields_its_colons() {
        let body = "::: {.enrichment}\n```\n:::\n```\nreal content\n:::\ntail\n";
        let (_, inner) = region_text(body).unwrap();
        assert!(inner.contains("real content"));
    }

    #[test]
    fn unclosed_region_runs_to_end_of_input() {
        let body = "before\n\n::: {.enrichment}\nrest";
        let (region, inner) = region_text(body).unwrap();
        assert!(region.ends_with("rest"));
        assert_eq!(inner, "rest");
    }

    #[test]
    fn multiple_regions_come_back_in_order() {
        let body = "::: {.enrichment}\nfirst\n:::\n\n::: {.enrichment}\nsecond\n:::\n";
        let regions = preserved_regions(body);
        assert_eq!(regions.len(), 2);
        assert!(body[regions[0].start..regions[0].end].contains("first"));
        assert!(body[regions[1].start..regions[1].end].contains("second"));
    }

    #[test]
    fn strip_preserved_removes_regions_and_keeps_author_content() {
        let body = "intro\n\n::: {.enrichment}\nadded\n:::\n\ntail\n";
        assert_eq!(strip_preserved(body), "intro\n\ntail\n");
        assert_eq!(strip_preserved("::: {.enrichment}\nonly\n:::\n"), "");
        assert_eq!(strip_preserved("no fences here\n"), "no fences here\n");
    }

    #[test]
    fn opener_attr_reads_a_regions_attribute() {
        let body = "::: {.enrichment from-revision=\"3\" enriched-at=\"t\"}\nbody\n:::\n";
        let region = preserved_regions(body)[0];
        assert_eq!(
            opener_attr(body, &region, "from-revision").as_deref(),
            Some("3")
        );
        assert_eq!(
            opener_attr(body, &region, "enriched-at").as_deref(),
            Some("t")
        );
        // Prefix names don't cross-match, absent keys are None.
        assert_eq!(opener_attr(body, &region, "revision"), None);
        assert_eq!(opener_attr(body, &region, "missing"), None);
    }

    #[test]
    fn quoted_attribute_values_keep_spaces() {
        let tokens = tokens_of_info(" {.enrichment title=\"a b c\"}");
        assert_eq!(tokens, vec![".enrichment", "title=\"a b c\""]);
    }
}
