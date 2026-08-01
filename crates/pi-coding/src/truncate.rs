//! Output truncation (port of pi's `coding/truncate.go`).
//!
//! Faithful port of pi's head/tail truncation used by the read, bash, ls, find
//! and grep tools. `TruncationResult` serializes with the same camelCase JSON
//! field names as pi's `TruncationResult` (truncate.ts) so it can be embedded
//! verbatim in tool `details` payloads.

use serde::{Deserialize, Serialize};

/// Default maximum number of lines kept by head/tail truncation.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default maximum number of bytes kept by head/tail truncation (50 KiB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Maximum line length (in UTF-16 code units) for grep output lines.
pub const GREP_MAX_LINE_LENGTH: usize = 500;
/// Default entry limit for the `ls` tool.
pub const LS_DEFAULT_LIMIT: usize = 500;
/// Default result limit for the `find` tool.
pub const FIND_DEFAULT_LIMIT: usize = 1000;
/// Default match limit for the `grep` tool.
pub const GREP_DEFAULT_LIMIT: usize = 100;
/// Default / hard-max result limit for the `glob` tool (OMP child-agent parity).
pub const GLOB_DEFAULT_LIMIT: usize = 200;
/// Hard maximum for the `glob` tool `limit` parameter (clamped, never exceeded).
pub const GLOB_MAX_LIMIT: usize = 200;

/// Describes the outcome of a truncation operation. The JSON field names match
/// pi's `TruncationResult` shape (truncate.ts) so it can be embedded in tool
/// `details` payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    /// The (possibly truncated) output text.
    #[serde(default)]
    pub content: String,
    /// Whether truncation occurred.
    #[serde(default)]
    pub truncated: bool,
    /// What triggered truncation: `"lines"`, `"bytes"`, or `""` (pi: null).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub truncated_by: String,
    /// Total lines in the original content.
    #[serde(default)]
    pub total_lines: usize,
    /// Total bytes in the original content.
    #[serde(default)]
    pub total_bytes: usize,
    /// Lines in the output.
    #[serde(default)]
    pub output_lines: usize,
    /// Bytes in the output.
    #[serde(default)]
    pub output_bytes: usize,
    /// Whether the last retained line is a partial byte-truncated line.
    #[serde(default)]
    pub last_line_partial: bool,
    /// Whether the very first line alone exceeds the byte limit (head only).
    #[serde(default)]
    pub first_line_exceeds_limit: bool,
    /// The effective max-lines bound.
    #[serde(default)]
    pub max_lines: usize,
    /// The effective max-bytes bound.
    #[serde(default)]
    pub max_bytes: usize,
}

/// Renders a byte count as a human-readable size (port of pi's `formatSize`).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Returns the number of UTF-16 code units in `s`, matching JS `String.length`
/// (astral characters count as 2). Used where pi reports `.length`.
pub fn utf16_len(s: &str) -> usize {
    let mut n = 0usize;
    for r in s.chars() {
        if (r as u32) > 0xFFFF {
            n += 2;
        } else {
            n += 1;
        }
    }
    n
}

/// Splits `content` on `'\n'`, dropping a trailing empty element when the
/// content ends with `'\n'` (port of pi's `splitLinesForCounting`). An empty
/// input yields an empty slice.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Keeps the first N lines/bytes (for file reads). Port of pi's `truncateHead`.
///
/// `max_lines`/`max_bytes` of `0` fall back to the defaults. `max_lines` of
/// [`usize::MAX`] disables the line limit (byte cap only), matching pi's
/// `truncateHead({ maxLines: Number.MAX_SAFE_INTEGER })` for ls/find/grep.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let max_lines = if max_lines == 0 { DEFAULT_MAX_LINES } else { max_lines };
    let max_bytes = if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes };
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            max_lines,
            max_bytes,
            ..Default::default()
        };
    }
    if !lines.is_empty() && lines[0].len() > max_bytes {
        return TruncationResult {
            truncated: true,
            truncated_by: "bytes".to_string(),
            total_lines,
            total_bytes,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
            ..Default::default()
        };
    }

    let mut out: Vec<&str> = Vec::new();
    let mut out_bytes = 0usize;
    let mut truncated_by = "lines";
    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let mut line_bytes = line.len();
        if i > 0 {
            line_bytes += 1; // the '\n' separator
        }
        if out_bytes + line_bytes > max_bytes {
            truncated_by = "bytes";
            break;
        }
        out.push(line);
        out_bytes += line_bytes;
    }
    if out.len() >= max_lines && out_bytes <= max_bytes {
        truncated_by = "lines";
    }
    let output_content = out.join("\n");
    let output_bytes = output_content.len();
    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: truncated_by.to_string(),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        output_bytes,
        max_lines,
        max_bytes,
        ..Default::default()
    }
}

/// Keeps the last N lines/bytes (for command output). Port of pi's
/// `truncateTail`.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let max_lines = if max_lines == 0 { DEFAULT_MAX_LINES } else { max_lines };
    let max_bytes = if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes };
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            max_lines,
            max_bytes,
            ..Default::default()
        };
    }
    let mut out: Vec<String> = Vec::new();
    let mut out_bytes = 0usize;
    let mut truncated_by = "lines";
    let mut last_line_partial = false;
    for i in (0..lines.len()).rev() {
        if out.len() >= max_lines {
            break;
        }
        let mut line_bytes = lines[i].len();
        if !out.is_empty() {
            line_bytes += 1; // the '\n' separator
        }
        if out_bytes + line_bytes > max_bytes {
            truncated_by = "bytes";
            if out.is_empty() {
                let truncated = truncate_string_to_bytes_from_end(lines[i], max_bytes);
                out.insert(0, truncated);
                out_bytes = out[0].len();
                last_line_partial = true;
            }
            break;
        }
        out.insert(0, lines[i].to_string());
        out_bytes += line_bytes;
    }
    if out.len() >= max_lines && out_bytes <= max_bytes {
        truncated_by = "lines";
    }
    let output_content = out.join("\n");
    let output_bytes = output_content.len();
    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: truncated_by.to_string(),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        output_bytes,
        last_line_partial,
        max_lines,
        max_bytes,
        ..Default::default()
    }
}

/// Returns the last `max_bytes` bytes of `s`, advancing past any leading UTF-8
/// continuation bytes so the result is valid UTF-8 (port of pi's helper).
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut start = s.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    s[start..].to_string()
}

/// Truncates a single line to `max_chars` UTF-16 code units, appending a
/// marker. Port of pi's `truncateLine` — a slice that would split a surrogate
/// pair yields a lone high surrogate in JS, which serializes as U+FFFD.
///
/// `max_chars` of `0` falls back to [`GREP_MAX_LINE_LENGTH`].
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    let max_chars = if max_chars == 0 { GREP_MAX_LINE_LENGTH } else { max_chars };
    if utf16_len(line) <= max_chars {
        return (line.to_string(), false);
    }
    let mut buf = String::new();
    let mut n = 0usize;
    for r in line.chars() {
        let units = if (r as u32) > 0xFFFF { 2 } else { 1 };
        if n + units > max_chars {
            if units == 2 && n + 1 == max_chars {
                // JS slice cuts mid-pair, leaving a lone high surrogate.
                buf.push('\u{FFFD}');
            }
            break;
        }
        buf.push(r);
        n += units;
        if n == max_chars {
            break;
        }
    }
    buf.push_str("... [truncated]");
    (buf, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_boundaries() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn utf16_len_astral_counts_two() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("\u{1F600}"), 2); // astral
        assert_eq!(utf16_len("a\u{1F600}b"), 4);
    }

    #[test]
    fn truncate_head_under_limits_passthrough() {
        let tr = truncate_head("a\nb\nc", 0, 0);
        assert!(!tr.truncated);
        assert_eq!(tr.content, "a\nb\nc");
        assert_eq!(tr.total_lines, 3);
        assert_eq!(tr.output_lines, 3);
    }

    #[test]
    fn truncate_head_by_lines() {
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let tr = truncate_head(&content, 3, 0);
        assert!(tr.truncated);
        assert_eq!(tr.truncated_by, "lines");
        assert_eq!(tr.output_lines, 3);
        assert_eq!(tr.content, "line0\nline1\nline2");
        assert_eq!(tr.total_lines, 10);
    }

    #[test]
    fn truncate_head_by_bytes() {
        let content = (0..5).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let tr = truncate_head(&content, 0, 20);
        assert!(tr.truncated);
        assert_eq!(tr.truncated_by, "bytes");
        assert_eq!(tr.output_lines, 3);
        assert_eq!(tr.content, "line0\nline1\nline2");
    }

    #[test]
    fn truncate_head_first_line_exceeds_limit() {
        let long = "x".repeat(100);
        let tr = truncate_head(&long, 0, 10);
        assert!(tr.truncated);
        assert!(tr.first_line_exceeds_limit);
        assert_eq!(tr.truncated_by, "bytes");
        assert_eq!(tr.content, "");
    }

    #[test]
    fn truncate_tail_keeps_last_lines() {
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let tr = truncate_tail(&content, 3, 0);
        assert!(tr.truncated);
        assert_eq!(tr.truncated_by, "lines");
        assert_eq!(tr.content, "line7\nline8\nline9");
        assert_eq!(tr.output_lines, 3);
    }

    #[test]
    fn truncate_tail_by_bytes_partial_line() {
        // A long last line that alone exceeds the byte budget yields a partial
        // (byte-truncated from the end) last line.
        let content = format!("ab\n{}", "x".repeat(40));
        let tr = truncate_tail(&content, 0, 10);
        assert!(tr.truncated);
        assert_eq!(tr.truncated_by, "bytes");
        assert!(tr.last_line_partial);
        // The partial line is the last `max_bytes` of the long line.
        assert_eq!(tr.content.len(), 10);
        assert!(tr.content.chars().all(|c| c == 'x'));
    }

    #[test]
    fn truncate_line_short_passthrough() {
        let (s, was) = truncate_line("hello", 10);
        assert!(!was);
        assert_eq!(s, "hello");
    }

    #[test]
    fn truncate_line_appends_marker() {
        let long = "a".repeat(600);
        let (s, was) = truncate_line(&long, 10);
        assert!(was);
        assert!(s.ends_with("... [truncated]"));
        assert!(s.starts_with("aaaaaaaaaa"));
    }

    #[test]
    fn truncate_line_surrogate_split_yields_replacement() {
        let s = "\u{1F600}\u{1F600}"; // two astral chars = 4 UTF-16 units
        let (out, was) = truncate_line(s, 3);
        assert!(was);
        assert!(out.starts_with('\u{1F600}'));
        assert!(out.contains('\u{FFFD}'));
        assert!(out.ends_with("... [truncated]"));
    }
}