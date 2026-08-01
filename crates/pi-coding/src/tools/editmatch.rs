//! Edit matching: exact-then-fuzzy text replacement with pi's uniqueness,
//! overlap and no-change checks (port of pi's `coding/editmatch.go`).
//!
//! Also hosts the line-ending helpers (`normalize_to_lf`, `detect_line_ending`,
//! `restore_line_endings`) used by the edit tool.

use anyhow::{anyhow, Result};
use unicode_normalization::UnicodeNormalization;

/// A single `oldText` → `newText` replacement.
#[derive(Debug, Clone)]
pub(crate) struct EditEntry {
    pub old_text: String,
    pub new_text: String,
}

/// Reports whether `r` is in JS `String.prototype.trimEnd`'s trim set
/// (ECMAScript WhiteSpace ∪ LineTerminator): TAB VT FF SP NBSP ZWNBSP(U+FEFF),
/// any Zs, LF CR LS PS. Unlike Rust's `char::is_whitespace` it includes U+FEFF
/// and excludes U+0085 (NEL).
fn is_js_whitespace(r: char) -> bool {
    matches!(
        r,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' '
            | '\u{00A0}'
            | '\u{FEFF}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{1680}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{2000}'..='\u{200A}'
    )
}

/// Normalizes text for whitespace/Unicode-tolerant matching (port of pi's
/// `normalizeForFuzzyMatch`, edit-diff.ts:34). Applies Unicode NFKC, strips
/// trailing per-line whitespace (JS `trimEnd` set), and folds smart quotes,
/// dashes, and exotic spaces to ASCII.
fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let lines: Vec<String> = nfkc
        .split('\n')
        .map(|line| line.trim_end_matches(is_js_whitespace).to_string())
        .collect();
    let joined = lines.join("\n");
    joined
        .chars()
        .map(|r| match r {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{0020}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            _ => r,
        })
        .collect()
}

/// Result of locating `oldText` in content. When fuzzy, the index/length are in
/// the normalized content space.
struct FuzzyMatch {
    found: bool,
    index: i64,
    match_length: usize,
    used_fuzzy: bool,
}

/// Finds `old_text` in `content`, trying exact match then fuzzy (normalized)
/// match (port of pi's `fuzzyFindText`).
fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatch {
    if let Some(i) = content.find(old_text) {
        return FuzzyMatch {
            found: true,
            index: i as i64,
            match_length: old_text.len(),
            used_fuzzy: false,
        };
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if let Some(i) = fuzzy_content.find(&fuzzy_old) {
        return FuzzyMatch {
            found: true,
            index: i as i64,
            match_length: fuzzy_old.len(),
            used_fuzzy: true,
        };
    }
    FuzzyMatch {
        found: false,
        index: -1,
        match_length: 0,
        used_fuzzy: false,
    }
}

fn count_fuzzy_occurrences(content: &str, old_text: &str) -> usize {
    let c = normalize_for_fuzzy_match(content);
    let o = normalize_for_fuzzy_match(old_text);
    c.matches(&o.as_str()).count()
}

/// Strips a leading BOM, returning `(bom, text)`.
pub(crate) fn strip_bom(content: &str) -> (String, String) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), rest.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

/// Normalizes CRLF and bare CR to LF (port of pi's `normalizeToLF`).
pub(crate) fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Returns CRLF only if the first CRLF precedes the first bare LF (port of
/// pi's `detectLineEnding`).
pub(crate) fn detect_line_ending(s: &str) -> &'static str {
    let lf_idx = s.find('\n');
    let crlf_idx = s.find("\r\n");
    match (crlf_idx, lf_idx) {
        (None, None) | (_, None) | (None, _) => "\n",
        (Some(c), Some(l)) => {
            if c < l {
                "\r\n"
            } else {
                "\n"
            }
        }
    }
}

/// Restores the detected line ending across a (LF-normalized) string.
pub(crate) fn restore_line_endings(s: &str, ending: &str) -> String {
    if ending == "\r\n" {
        s.replace('\n', "\r\n")
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: i64,
    match_length: usize,
    new_text: String,
}

/// Splits content into lines that keep their trailing `"\n"` (port of pi's
/// `/[^\n]*\n|[^\n]+/g`). A trailing `"\n"` yields no empty final element, and
/// `""` yields no elements.
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match content[i..].find('\n') {
            None => {
                out.push(&content[i..]);
                break;
            }
            Some(j) => {
                out.push(&content[i..i + j + 1]);
                i += j + 1;
            }
        }
    }
    out
}

/// Returns the byte `[start, end)` span of each line (with ending).
fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let lines = split_lines_with_endings(content);
    let mut spans = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in lines {
        spans.push(LineSpan {
            start: offset,
            end: offset + line.len(),
        });
        offset += line.len();
    }
    spans
}

/// Widens a replacement to the `[start_line, end_line)` lines it touches.
/// `end_line` is exclusive.
fn get_replacement_line_range(lines: &[LineSpan], m: &MatchedEdit) -> Result<(usize, usize)> {
    let rep_start = m.match_index as usize;
    let rep_end = rep_start + m.match_length;
    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if rep_start >= line.start && rep_start < line.end {
            start_line = Some(i);
            break;
        }
    }
    let start_line = start_line.ok_or_else(|| anyhow!("Replacement range is outside the base content."))?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < rep_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err(anyhow!("Replacement range is outside the base content."));
    }
    Ok((start_line, end_line + 1))
}

/// Rewrites content with the given replacements, applied in reverse so earlier
/// offsets stay valid. `offset` shifts each replacement's match_index into
/// content-local coordinates.
fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: i64) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let mi = (r.match_index - offset) as usize;
        let end = mi + r.match_length;
        result = format!("{}{}{}", &result[..mi], r.new_text, &result[end..]);
    }
    result
}

/// Overlays line-level replacements matched against `base_content` (a
/// normalized view) onto `original_content`, copying untouched lines back
/// verbatim. Touched line-blocks are rewritten from `base_content`; the actual
/// replacement ranges drive preservation so duplicate normalized lines can't
/// align to the wrong occurrence.
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(anyhow!(
            "Cannot preserve unchanged lines because the base content has a different line count."
        ));
    }

    let mut sorted: Vec<MatchedEdit> = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);

    struct Group {
        start_line: usize,
        end_line: usize,
        replacements: Vec<MatchedEdit>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for r in &sorted {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, r)?;
        if let Some(last) = groups.last_mut() {
            if start_line < last.end_line {
                if end_line > last.end_line {
                    last.end_line = end_line;
                }
                last.replacements.push(r.clone());
                continue;
            }
        }
        groups.push(Group {
            start_line,
            end_line,
            replacements: vec![r.clone()],
        });
    }

    let mut out = String::new();
    let mut original_line_index = 0usize;
    for g in &groups {
        for line in &original_lines[original_line_index..g.start_line] {
            out.push_str(line);
        }
        let group_start_offset = base_lines[g.start_line].start;
        let group_end_offset = base_lines[g.end_line - 1].end;
        out.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &g.replacements,
            group_start_offset as i64,
        ));
        original_line_index = g.end_line;
    }
    for line in &original_lines[original_line_index..] {
        out.push_str(line);
    }
    Ok(out)
}

/// Applies edits to LF-normalized content using exact-then-fuzzy matching, with
/// pi's uniqueness/overlap/no-change checks. Returns the base content used for
/// matching and the resulting content.
pub(crate) fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[EditEntry],
    path: &str,
) -> Result<(String, String)> {
    let n = edits.len();
    let mut norm = Vec::with_capacity(n);
    for (i, e) in edits.iter().enumerate() {
        let old = normalize_to_lf(&e.old_text);
        let new = normalize_to_lf(&e.new_text);
        if old.is_empty() {
            return Err(empty_old_text_error(path, i, n));
        }
        norm.push(EditEntry {
            old_text: old,
            new_text: new,
        });
    }

    let mut any_fuzzy = false;
    for e in &norm {
        if fuzzy_find_text(normalized_content, &e.old_text).used_fuzzy {
            any_fuzzy = true;
            break;
        }
    }
    // Matching runs in fuzzy-normalized space when any edit needed fuzzy
    // matching, but unchanged lines are overlaid back from the original: the
    // returned base is always the LF-normalized original, not the fuzzy view.
    let replacement_base = if any_fuzzy {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched = Vec::with_capacity(n);
    for (i, e) in norm.iter().enumerate() {
        let m = fuzzy_find_text(&replacement_base, &e.old_text);
        if !m.found {
            return Err(not_found_error(path, i, n));
        }
        let occ = count_fuzzy_occurrences(&replacement_base, &e.old_text);
        if occ > 1 {
            return Err(duplicate_error(path, i, n, occ));
        }
        matched.push(MatchedEdit {
            edit_index: i,
            match_index: m.index,
            match_length: m.match_length,
            new_text: e.new_text.clone(),
        });
    }

    matched.sort_by_key(|m| m.match_index);
    for i in 1..matched.len() {
        let prev = &matched[i - 1];
        let cur = &matched[i];
        if prev.match_index + prev.match_length as i64 > cur.match_index {
            return Err(anyhow!(
                "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
                prev.edit_index,
                cur.edit_index,
                path
            ));
        }
    }

    let base = normalized_content.to_string();
    let result = if any_fuzzy {
        apply_replacements_preserving_unchanged_lines(normalized_content, &replacement_base, &matched)?
    } else {
        apply_replacements(&replacement_base, &matched, 0)
    };
    if base == result {
        return Err(no_change_error(path, n));
    }
    Ok((base, result))
}

fn empty_old_text_error(path: &str, i: usize, total: usize) -> anyhow::Error {
    if total == 1 {
        anyhow!("oldText must not be empty in {path}.")
    } else {
        anyhow!("edits[{i}].oldText must not be empty in {path}.")
    }
}

fn not_found_error(path: &str, i: usize, total: usize) -> anyhow::Error {
    if total == 1 {
        anyhow!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        anyhow!(
            "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(path: &str, i: usize, total: usize, occ: usize) -> anyhow::Error {
    if total == 1 {
        anyhow!(
            "Found {occ} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        anyhow!(
            "Found {occ} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn no_change_error(path: &str, total: usize) -> anyhow::Error {
    if total == 1 {
        anyhow!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        anyhow!("No changes made to {path}. The replacements produced identical content.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> EditEntry {
        EditEntry {
            old_text: old.to_string(),
            new_text: new.to_string(),
        }
    }

    #[test]
    fn strip_bom_works() {
        let (bom, text) = strip_bom("\u{FEFF}hello");
        assert_eq!(bom, "\u{FEFF}");
        assert_eq!(text, "hello");
        let (bom2, text2) = strip_bom("plain");
        assert_eq!(bom2, "");
        assert_eq!(text2, "plain");
    }

    #[test]
    fn normalize_to_lf_and_line_ending() {
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(detect_line_ending("a\r\nb"), "\r\n");
        assert_eq!(detect_line_ending("a\nb\r\nc"), "\n"); // bare LF first
        assert_eq!(detect_line_ending("a\nb"), "\n");
        assert_eq!(restore_line_endings("a\nb", "\r\n"), "a\r\nb");
    }

    #[test]
    fn apply_single_exact_edit() {
        let content = "fn foo() {\n    return 1;\n}\n";
        let (base, result) =
            apply_edits_to_normalized_content(content, &[edit("return 1;", "return 2;")], "f.rs")
                .unwrap();
        assert_eq!(base, content);
        assert_eq!(result, "fn foo() {\n    return 2;\n}\n");
    }

    #[test]
    fn apply_multiple_disjoint_edits() {
        let content = "a\nb\nc\nd\n";
        let (base, result) = apply_edits_to_normalized_content(
            content,
            &[edit("a", "A"), edit("c", "C")],
            "f.rs",
        )
        .unwrap();
        assert_eq!(base, content);
        assert_eq!(result, "A\nb\nC\nd\n");
    }

    #[test]
    fn duplicate_edit_rejected() {
        let content = "x\nx\n";
        let err = apply_edits_to_normalized_content(content, &[edit("x", "y")], "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 occurrences"), "got: {err}");
    }

    #[test]
    fn not_found_rejected() {
        let content = "hello world\n";
        let err = apply_edits_to_normalized_content(content, &[edit("missing", "x")], "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Could not find"));
    }

    #[test]
    fn empty_oldtext_rejected() {
        let content = "hello\n";
        let err = apply_edits_to_normalized_content(content, &[edit("", "x")], "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn overlapping_edits_rejected() {
        let content = "abcdef\n";
        // first edit covers "abcd", second covers "cdef" → overlap.
        let err = apply_edits_to_normalized_content(
            content,
            &[edit("abcd", "X"), edit("cdef", "Y")],
            "f.rs",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("overlap"));
    }

    #[test]
    fn no_change_rejected() {
        let content = "same\n";
        let err =
            apply_edits_to_normalized_content(content, &[edit("same", "same")], "f.rs")
                .unwrap_err()
                .to_string();
        assert!(err.contains("No changes"));
    }

    #[test]
    fn fuzzy_match_tolerates_smart_quotes() {
        // oldText uses straight apostrophe, content uses curly U+2019.
        let content = "don\u{2019}t panic\n";
        let (_base, result) =
            apply_edits_to_normalized_content(content, &[edit("don't", "do not")], "f.rs").unwrap();
        // The unchanged-line overlay preserves original lines except the touched block.
        assert!(result.contains("do not") || result.contains("do not panic"));
    }
}