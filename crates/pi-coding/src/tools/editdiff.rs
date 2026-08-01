//! Bounded edit-result diff rendering.
//!
//! Ports pi's display-oriented diff and unified-patch shapes while capping
//! serialized detail strings so an edit result cannot grow without bound.

use similar::{ChangeTag, TextDiff};

const DIFF_CONTEXT_LINES: usize = 4;
const MAX_DETAIL_BYTES: usize = 256 * 1024;
const TRUNCATION_NOTICE: &str = "\n... [diff details truncated]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditDiffDetails {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

pub(crate) fn generate_edit_details(path: &str, old_content: &str, new_content: &str) -> EditDiffDetails {
    let text_diff = TextDiff::from_lines(old_content, new_content);
    let first_changed_line = text_diff
        .iter_all_changes()
        .find(|change| change.tag() != ChangeTag::Equal)
        .map(|change| change.new_index().map_or_else(|| change.old_index().unwrap_or(0), |index| index) + 1);
    let diff = bounded_detail(generate_diff_string(&text_diff, old_content, new_content));
    let patch = bounded_detail(generate_unified_patch(&text_diff, path));
    EditDiffDetails {
        diff,
        patch,
        first_changed_line,
    }
}

fn generate_diff_string(text_diff: &TextDiff<'_, '_, str>, old_content: &str, new_content: &str) -> String {
    let max_line_num = old_content.split('\n').count().max(new_content.split('\n').count());
    let width = max_line_num.to_string().len();
    let groups = text_diff.grouped_ops(DIFF_CONTEXT_LINES);
    let mut output = Vec::new();

    for (group_index, group) in groups.iter().enumerate() {
        if group.first().is_some_and(|op| op.old_range().start > 0) {
            output.push(format!(" {:width$}...", ""));
        }
        for op in group {
            for change in text_diff.iter_changes(op) {
                let raw = change.as_str().unwrap_or_default();
                let value = raw.strip_suffix('\n').unwrap_or(raw);
                match change.tag() {
                    ChangeTag::Delete => output.push(format!(
                        "-{line:>width$} {value}",
                        line = change.old_index().unwrap_or(0) + 1,
                    )),
                    ChangeTag::Insert => output.push(format!(
                        "+{line:>width$} {value}",
                        line = change.new_index().unwrap_or(0) + 1,
                    )),
                    ChangeTag::Equal => output.push(format!(
                        " {line:>width$} {value}",
                        line = change.old_index().unwrap_or(0) + 1,
                    )),
                }
            }
        }
        if group_index + 1 == groups.len()
            && group.last().is_some_and(|op| op.old_range().end < text_diff.old_len())
        {
            output.push(format!(" {:width$}...", ""));
        }
    }
    output.join("\n")
}

fn generate_unified_patch(text_diff: &TextDiff<'_, '_, str>, path: &str) -> String {
    let mut formatter = text_diff.unified_diff();
    formatter
        .context_radius(DIFF_CONTEXT_LINES)
        .header(path, path)
        .to_string()
}

fn bounded_detail(mut value: String) -> String {
    if value.len() <= MAX_DETAIL_BYTES {
        return value;
    }
    let keep = MAX_DETAIL_BYTES.saturating_sub(TRUNCATION_NOTICE.len());
    let mut boundary = keep.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(TRUNCATION_NOTICE);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_replacement_matches_detail_shape() {
        let details = generate_edit_details("a.txt", "one\ntwo\nthree\n", "one\nTWO\nthree\n");
        assert_eq!(details.first_changed_line, Some(2));
        assert!(details.diff.contains("-2 two"), "{}", details.diff);
        assert!(details.diff.contains("+2 TWO"), "{}", details.diff);
        assert!(details.patch.starts_with("--- a.txt\n+++ a.txt\n@@"), "{}", details.patch);
        assert!(details.patch.contains("-two\n+TWO"), "{}", details.patch);
    }

    #[test]
    fn detail_strings_are_bounded() {
        let old = (0..40_000).map(|i| format!("old-{i}\n")).collect::<String>();
        let new = (0..40_000).map(|i| format!("new-{i}\n")).collect::<String>();
        let details = generate_edit_details("large.txt", &old, &new);
        assert!(details.diff.len() <= MAX_DETAIL_BYTES);
        assert!(details.patch.len() <= MAX_DETAIL_BYTES);
        assert!(details.diff.ends_with(TRUNCATION_NOTICE));
        assert!(details.patch.ends_with(TRUNCATION_NOTICE));
    }
}
