use std::ops::Range;

use super::analysis::{
    AnalysisMode, ListMarker, MarkdownBlock, analyze_with_limit, streaming_tail_start,
};
use super::inline::{InlineStyle, InlineStyleRange, shifted_styles, wrap_inline};
use super::mermaid::{
    MermaidDiagnostic, MermaidDiagramKind, MermaidLimits, render_mermaid_unicode,
};
use super::table::layout_table;
use super::text::{
    display_width, fit_text, safe_prefix, sanitize_inline, wrap_text, wrap_verbatim,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineRole {
    #[default]
    Text,
    Heading(u8),
    ListMarker,
    Quote,
    Code,
    CodeFence,
    TableBorder,
    TableHeader,
    TableBody,
    MermaidBorder,
    MermaidNode,
    MermaidEdge,
    Diagnostic,
    ThematicBreak,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NeutralLine {
    pub text: String,
    pub role: LineRole,
    pub inline_styles: Vec<InlineStyleRange>,
    /// Normalized fenced-code language for [`LineRole::Code`] body lines.
    ///
    /// Terminal-neutral metadata so presentation layers can apply a language-aware
    /// lexer instead of one language-agnostic highlighter. Fence chrome
    /// ([`LineRole::CodeFence`]) and non-code roles leave this `None`.
    pub language: Option<String>,
}

impl NeutralLine {
    fn new(text: impl Into<String>, role: LineRole) -> Self {
        Self {
            text: text.into(),
            role,
            inline_styles: Vec::new(),
            language: None,
        }
    }

    fn code(text: impl Into<String>, language: Option<String>) -> Self {
        Self {
            text: text.into(),
            role: LineRole::Code,
            inline_styles: Vec::new(),
            language,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkdownRenderOptions {
    pub width: usize,
    pub max_input_bytes: usize,
    pub mermaid: MermaidLimits,
}

impl Default for MarkdownRenderOptions {
    fn default() -> Self {
        Self {
            width: 80,
            max_input_bytes: super::analysis::DEFAULT_MAX_MARKDOWN_BYTES,
            mermaid: MermaidLimits::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkdownRenderOutput {
    pub lines: Vec<NeutralLine>,
    pub diagnostics: Vec<RenderDiagnostic>,
    pub truncated: bool,
}

impl MarkdownRenderOutput {
    pub fn plain_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.text.clone()).collect()
    }

    pub fn plain_text(&self) -> String {
        let separator_bytes = self.lines.len().saturating_sub(1);
        let capacity = self
            .lines
            .iter()
            .map(|line| line.text.len())
            .sum::<usize>()
            .saturating_add(separator_bytes);
        let mut output = String::with_capacity(capacity);
        for (index, line) in self.lines.iter().enumerate() {
            if index != 0 {
                output.push('\n');
            }
            output.push_str(&line.text);
        }
        output
    }
}

#[derive(Clone, Debug)]
pub struct StreamingMarkdownRenderer {
    options: MarkdownRenderOptions,
    source: String,
    frozen_source_bytes: usize,
    frozen: MarkdownRenderOutput,
    tail: MarkdownRenderOutput,
    parsed_bytes: usize,
    input_truncated: bool,
}

impl StreamingMarkdownRenderer {
    #[must_use]
    pub fn new(options: MarkdownRenderOptions) -> Self {
        Self {
            options,
            source: String::new(),
            frozen_source_bytes: 0,
            frozen: MarkdownRenderOutput::default(),
            tail: MarkdownRenderOutput::default(),
            parsed_bytes: 0,
            input_truncated: false,
        }
    }

    pub fn push_str(&mut self, chunk: &str) {
        let remaining = self.options.max_input_bytes.saturating_sub(self.source.len());
        let (chunk, truncated) = safe_prefix(chunk, remaining);
        self.source.push_str(chunk);
        self.input_truncated |= truncated;

        let boundary = streaming_tail_start(&self.source);
        if boundary > self.frozen_source_bytes {
            let line_offset = self.source[..self.frozen_source_bytes].matches('\n').count();
            let newly_frozen = &self.source[self.frozen_source_bytes..boundary];
            let mut rendered = render_with_mode(
                newly_frozen,
                &self.options,
                AnalysisMode::Streaming,
            );
            offset_source_lines(&mut rendered.diagnostics, line_offset);
            self.frozen.lines.append(&mut rendered.lines);
            self.frozen.diagnostics.append(&mut rendered.diagnostics);
            self.frozen_source_bytes = boundary;
        }

        let tail_source = &self.source[self.frozen_source_bytes..];
        self.parsed_bytes = self.parsed_bytes.saturating_add(tail_source.len());
        self.tail = render_with_mode(tail_source, &self.options, AnalysisMode::Streaming);
        let line_offset = self.source[..self.frozen_source_bytes].matches('\n').count();
        offset_source_lines(&mut self.tail.diagnostics, line_offset);
    }

    #[must_use]
    pub fn output(&self) -> MarkdownRenderOutput {
        let mut output = self.frozen.clone();
        output.lines.extend(self.tail.lines.iter().cloned());
        output.diagnostics.extend(self.tail.diagnostics.iter().cloned());
        if self.input_truncated {
            output.diagnostics.push(RenderDiagnostic::InputTruncated {
                limit: self.options.max_input_bytes,
            });
            let width = self.options.width.clamp(1, 1_000);
            let message = format!(
                "[markdown truncated at {} bytes]",
                self.options.max_input_bytes
            );
            output.lines.push(NeutralLine::new(
                fit_text(&message, width),
                LineRole::Diagnostic,
            ));
            output.truncated = true;
        }
        output
    }

    #[must_use]
    pub fn frozen_source_bytes(&self) -> usize {
        self.frozen_source_bytes
    }

    /// Total bytes reparsed across streaming tail updates.
    ///
    /// Exposed for performance-contract tests and diagnostics; it does not
    /// include the already frozen prefix.
    #[must_use]
    pub const fn parsed_bytes(&self) -> usize {
        self.parsed_bytes
    }
}

fn offset_source_lines(diagnostics: &mut [RenderDiagnostic], line_offset: usize) {
    for diagnostic in diagnostics {
        match diagnostic {
            RenderDiagnostic::Mermaid { source_line, .. }
            | RenderDiagnostic::UnclosedFence { source_line }
            | RenderDiagnostic::TableTooNarrow { source_line } => {
                *source_line = source_line.saturating_add(line_offset);
            }
            RenderDiagnostic::InputTruncated { .. } => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderDiagnostic {
    InputTruncated { limit: usize },
    Mermaid {
        source_line: usize,
        diagnostic: MermaidDiagnostic,
    },
    UnclosedFence { source_line: usize },
    TableTooNarrow { source_line: usize },
}

pub fn render_markdown(
    source: &str,
    options: &MarkdownRenderOptions,
) -> MarkdownRenderOutput {
    render_with_mode(source, options, AnalysisMode::Complete)
}

pub fn render_markdown_streaming(
    source: &str,
    options: &MarkdownRenderOptions,
) -> MarkdownRenderOutput {
    render_with_mode(source, options, AnalysisMode::Streaming)
}

fn render_with_mode(
    source: &str,
    options: &MarkdownRenderOptions,
    mode: AnalysisMode,
) -> MarkdownRenderOutput {
    let width = options.width.clamp(1, 1_000);
    let document = analyze_with_limit(source, mode, options.max_input_bytes);
    let mut output = MarkdownRenderOutput {
        truncated: document.truncated,
        ..MarkdownRenderOutput::default()
    };
    if document.truncated {
        output
            .diagnostics
            .push(RenderDiagnostic::InputTruncated {
                limit: options.max_input_bytes,
            });
    }

    for block in document.blocks {
        match block {
            MarkdownBlock::Blank => output.lines.push(NeutralLine::default()),
            MarkdownBlock::Heading { level, text, .. } => {
                append_wrapped(&mut output.lines, &text, width, LineRole::Heading(level), "", None);
            }
            MarkdownBlock::List { items, .. } => {
                for item in items {
                    let marker = match item.marker {
                        ListMarker::Bullet(_) => "•".to_owned(),
                        ListMarker::Ordered { number, delimiter } => format!("{number}{delimiter}"),
                    };
                    // Task-list checkboxes (`- [ ]` / `- [x]`) render as
                    // checkbox glyphs instead of literal bracket text: `☐`
                    // (U+2610, unchecked) and `☑` (U+2611, checked). The glyph
                    // is chrome like the bullet/number, so it shares the
                    // ListMarker inline style; the item body stays base text.
                    let task = match item.checked {
                        Some(true) => "☑ ",
                        Some(false) => "☐ ",
                        None => "",
                    };
                    let indent = "  ".repeat(item.depth);
                    let prefix = format!("{indent}{marker} {task}");
                    // The list line is base text; only the marker prefix (after
                    // the leading indent) carries InlineStyle::ListMarker, so
                    // the item body renders in the base color — OMP colors the
                    // bullet/number only. The indent stays unstyled.
                    append_wrapped(
                        &mut output.lines,
                        &item.text,
                        width,
                        LineRole::Text,
                        &prefix,
                        Some((indent.len(), InlineStyle::ListMarker)),
                    );
                }
            }
            MarkdownBlock::FencedCode {
                info,
                source,
                closed,
                line,
                ..
            } => {
                if is_mermaid_info(&info) && closed {
                    append_mermaid(&mut output, &source, line, width, options.mermaid);
                } else {
                    if !closed {
                        output
                            .diagnostics
                            .push(RenderDiagnostic::UnclosedFence { source_line: line });
                    }
                    append_code(&mut output.lines, &info, &source, width, closed);
                }
            }
            MarkdownBlock::Table(table) => {
                if let Some(layout) = layout_table(&table, width) {
                    let header_rows = table_row_height(&table.headers, &layout.column_widths);
                    let line_count = layout_line_count(&table, &layout.column_widths);
                    for (index, (line, inline_styles)) in layout
                        .lines
                        .into_iter()
                        .zip(layout.inline_styles)
                        .enumerate()
                    {
                        let role = if index == 0 || index == header_rows + 1 || index + 1 == line_count {
                            LineRole::TableBorder
                        } else if index <= header_rows {
                            LineRole::TableHeader
                        } else {
                            LineRole::TableBody
                        };
                        output.lines.push(NeutralLine {
                            text: line,
                            role,
                            inline_styles,
                            language: None,
                        });
                    }
                } else {
                    output
                        .diagnostics
                        .push(RenderDiagnostic::TableTooNarrow {
                            source_line: table.line,
                        });
                    append_verbatim_source_lines(
                        &mut output.lines,
                        table.source_lines.iter().map(String::as_str),
                        LineRole::Text,
                    );
                }
            }
            MarkdownBlock::BlockQuote { lines, .. } => {
                for line in lines {
                    append_wrapped(&mut output.lines, &line, width, LineRole::Quote, "│ ", None);
                }
            }
            MarkdownBlock::ThematicBreak { .. } => {
                output.lines.push(NeutralLine::new(
                    "─".repeat(width),
                    LineRole::ThematicBreak,
                ));
            }
            MarkdownBlock::Paragraph { lines, .. } => {
                append_source_lines(
                    &mut output.lines,
                    lines.iter().map(String::as_str),
                    width,
                    LineRole::Text,
                );
            }
        }
    }

    if document.truncated {
        let message = format!("[markdown truncated at {} bytes]", options.max_input_bytes);
        output.lines.push(NeutralLine::new(
            fit_text(&message, width),
            LineRole::Diagnostic,
        ));
    }
    output
}

fn append_mermaid(
    output: &mut MarkdownRenderOutput,
    source: &str,
    source_line: usize,
    width: usize,
    limits: MermaidLimits,
) {
    match render_mermaid_unicode(source, width.saturating_sub(2), limits) {
        Ok(art) => {
            let kind = match art.kind {
                MermaidDiagramKind::Flowchart => "flowchart",
                MermaidDiagramKind::ClassDiagram => "classDiagram",
                MermaidDiagramKind::Sequence => "sequenceDiagram",
            };
            // Over-budget flowcharts/class diagrams arrive as multiple
            // chunks: render each as its own bordered frame, numbered so the
            // panels read in order. Single-chunk renders keep the plain title.
            let chunk_count = art.chunks.len();
            for (index, chunk_lines) in art.chunks.iter().enumerate() {
                let title = if chunk_count > 1 {
                    format!("┌─ mermaid · {kind} [part {}/{}]", index + 1, chunk_count)
                } else {
                    format!("┌─ mermaid · {kind}")
                };
                output.lines.push(NeutralLine::new(
                    fit_text(&title, width),
                    LineRole::MermaidBorder,
                ));
                let mut in_edges = false;
                for line in chunk_lines {
                    if line == "edges" {
                        in_edges = true;
                    }
                    output.lines.push(NeutralLine::new(
                        fit_text(&format!("│ {line}"), width),
                        if in_edges { LineRole::MermaidEdge } else { LineRole::MermaidNode },
                    ));
                }
                output.lines.push(NeutralLine::new(
                    fit_text("└─", width),
                    LineRole::MermaidBorder,
                ));
            }
        }
        Err(diagnostic) => {
            let message = format!("! mermaid: {}", diagnostic.message);
            output.lines.push(NeutralLine::new(
                fit_text("┌─ mermaid · source fallback", width),
                LineRole::MermaidBorder,
            ));
            for line in source.lines() {
                for wrapped in wrap_verbatim(line, width.saturating_sub(2).max(1)) {
                    output.lines.push(NeutralLine::new(
                        format!("│ {wrapped}"),
                        LineRole::Code,
                    ));
                }
            }
            if source.is_empty() {
                output.lines.push(NeutralLine::new("│", LineRole::Code));
            }
            for line in wrap_text(&message, width.saturating_sub(2).max(1)) {
                output.lines.push(NeutralLine::new(
                    fit_text(&format!("│ {line}"), width),
                    LineRole::Diagnostic,
                ));
            }
            output.lines.push(NeutralLine::new(
                fit_text("└─", width),
                LineRole::MermaidBorder,
            ));
            output.diagnostics.push(RenderDiagnostic::Mermaid {
                source_line,
                diagnostic,
            });
        }
    }
}

/// Text embedded in the temporary bottom border of a fence that is still
/// open at render time (streaming tail or a source that never closes it).
/// The marker makes the frame read as incomplete instead of pretending the
/// block ended, and disappears once the closing fence line arrives.
const UNCLOSED_FENCE_MARKER: &str = "… (unclosed fence)";

fn append_code(lines: &mut Vec<NeutralLine>, info: &str, source: &str, width: usize, closed: bool) {
    let language = normalize_fence_language(info);
    let title = match language.as_deref() {
        Some(lang) => format!("code · {lang}"),
        None => "code".to_owned(),
    };
    // Tool-card-style frame. The top border embeds the language label
    // (`╭── code · lang ──╮`); when the label cannot fit the border, fall
    // back to a plain top border plus a titled row, mirroring tool cards.
    let title_width = display_width(&title);
    if title_width <= width.saturating_sub(8) {
        let fill = width.saturating_sub(8) - title_width;
        lines.push(NeutralLine::new(
            format!("╭── {title} ──{}╮", "─".repeat(fill)),
            LineRole::CodeFence,
        ));
    } else {
        lines.push(NeutralLine::new(
            format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
            LineRole::CodeFence,
        ));
        let inner = width.saturating_sub(4).max(1);
        let fitted = fit_text(&title, inner);
        let pad = inner.saturating_sub(display_width(&fitted));
        lines.push(NeutralLine::new(
            fit_text(&format!("│ {fitted}{} │", " ".repeat(pad)), width),
            LineRole::CodeFence,
        ));
    }

    // Frame body: per-line sides (`│ … │`) with the code padded to the inner
    // width. Wrapping stays layout-verbatim (tabs expand, indentation
    // survives) so Python/Makefile samples render true.
    let content_width = width.saturating_sub(4).max(1);
    let mut emitted_body = false;
    for line in source.lines() {
        for wrapped in wrap_verbatim(line, content_width) {
            // `wrap_verbatim` keeps a single grapheme wider than the content
            // band intact on its own row (lossless overflow policy). Clamp
            // such rows to `content_width` with an ellipsis so every body row
            // is exactly frame_width — the D40 border contract — instead of
            // pushing the right `│` past the `╰…╯` corner.
            let fitted = if display_width(&wrapped) > content_width {
                fit_text(&wrapped, content_width)
            } else {
                wrapped
            };
            let pad = content_width - display_width(&fitted);
            lines.push(NeutralLine::code(
                format!("│ {fitted}{} │", " ".repeat(pad)),
                language.clone(),
            ));
            emitted_body = true;
        }
    }
    if !emitted_body {
        // Empty / blank-only fences still expose one explicit padded body row.
        lines.push(NeutralLine::code(
            format!("│ {} │", " ".repeat(content_width)),
            language.clone(),
        ));
    }

    if closed {
        lines.push(NeutralLine::new(
            format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
            LineRole::CodeFence,
        ));
    } else {
        // Still-open fence: a temporary bottom that embeds the unclosed
        // marker (`╰── … (unclosed fence) ──╯`), so the frame never renders
        // borderless or fakes a closed block. When the marker cannot fit the
        // border line, fall back to a plain bottom border plus an inner
        // marker row, mirroring the top border's overflow behavior.
        let marker_width = display_width(UNCLOSED_FENCE_MARKER);
        if marker_width <= width.saturating_sub(8) {
            let fill = width.saturating_sub(8) - marker_width;
            lines.push(NeutralLine::new(
                format!("╰── {UNCLOSED_FENCE_MARKER} ──{}╯", "─".repeat(fill)),
                LineRole::CodeFence,
            ));
        } else {
            let inner = width.saturating_sub(4).max(1);
            let fitted = fit_text(UNCLOSED_FENCE_MARKER, inner);
            let pad = inner.saturating_sub(display_width(&fitted));
            lines.push(NeutralLine::new(
                fit_text(&format!("│ {fitted}{} │", " ".repeat(pad)), width),
                LineRole::CodeFence,
            ));
            lines.push(NeutralLine::new(
                format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
                LineRole::CodeFence,
            ));
        }
    }
}

fn append_source_lines<'a>(
    output: &mut Vec<NeutralLine>,
    lines: impl IntoIterator<Item = &'a str>,
    width: usize,
    role: LineRole,
) {
    for line in lines {
        append_wrapped(output, line, width, role, "", None);
    }
}

fn append_verbatim_wrapped_lines<'a>(
    output: &mut Vec<NeutralLine>,
    lines: impl IntoIterator<Item = &'a str>,
    width: usize,
    role: LineRole,
) {
    for line in lines {
        output.extend(wrap_verbatim(line, width).into_iter().map(|text| NeutralLine::new(text, role)));
    }
}

fn table_row_height(cells: &[String], widths: &[usize]) -> usize {
    widths
        .iter()
        .enumerate()
        .map(|(column, width)| wrap_inline(cells.get(column).map(String::as_str).unwrap_or_default(), *width).len())
        .max()
        .unwrap_or(1)
}

fn layout_line_count(table: &super::table::TableBlock, widths: &[usize]) -> usize {
    3 + table_row_height(&table.headers, widths)
        + table.rows.iter().map(|row| table_row_height(row, widths)).sum::<usize>()
}

fn append_verbatim_source_lines<'a>(
    output: &mut Vec<NeutralLine>,
    lines: impl IntoIterator<Item = &'a str>,
    role: LineRole,
) {
    output.extend(lines.into_iter().map(|line| NeutralLine::new(sanitize_inline(line), role)));
}

/// Append `text` wrapped at `width`, prefixed by `prefix` on the first line
/// and `prefix_width` spaces on continuation lines.
///
/// `marker` optionally carries `(marker_start, style)`: the prefix slice
/// `marker_start..prefix.len()` on the FIRST row gets that inline style (used
/// to color just the list bullet/number while the item body — and the leading
/// indent — stay in the base text style). Continuation rows never receive the
/// marker style, so wrapped list text renders in base.
fn append_wrapped(
    output: &mut Vec<NeutralLine>,
    text: &str,
    width: usize,
    role: LineRole,
    prefix: &str,
    marker: Option<(usize, InlineStyle)>,
) {
    let prefix_width = display_width(prefix);
    if prefix_width >= width {
        // The marker alone overflows the row; render the visible prefix as a
        // marker-only row so it still carries the marker style.
        let marker_text = fit_text(prefix, width);
        let mut inline_styles = Vec::new();
        if let Some((marker_start, marker_style)) = marker {
            let start = marker_start.min(marker_text.len());
            if start < marker_text.len() {
                inline_styles.push(InlineStyleRange {
                    range: start..marker_text.len(),
                    style: marker_style,
                });
            }
        }
        output.push(NeutralLine {
            text: marker_text,
            role,
            inline_styles,
            language: None,
        });
        return;
    }
    let content_width = width - prefix_width;
    for (index, line) in wrap_inline(text, content_width).into_iter().enumerate() {
        let continuation = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };
        let content_offset = continuation.len();
        let mut rendered = String::with_capacity(content_offset + line.text.len());
        rendered.push_str(&continuation);
        rendered.push_str(&line.text);
        let mut inline_styles = shifted_styles(&line.styles, content_offset);
        if index == 0
            && let Some((marker_start, marker_style)) = marker
        {
            inline_styles.push(InlineStyleRange {
                range: marker_start..continuation.len(),
                style: marker_style,
            });
        }
        output.push(NeutralLine {
            text: rendered,
            role,
            inline_styles,
            language: None,
        });
    }
}

fn normalize_fence_language(info: &str) -> Option<String> {
    let language = info
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['{', '}', '.'])
        .trim();
    if language.is_empty() {
        None
    } else {
        Some(language.to_ascii_lowercase())
    }
}

fn is_mermaid_info(info: &str) -> bool {
    matches!(
        normalize_fence_language(info).as_deref(),
        Some("mermaid" | "mermaid-js")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn render(source: &str, width: usize) -> MarkdownRenderOutput {
        render_markdown(
            source,
            &MarkdownRenderOptions {
                width,
                ..MarkdownRenderOptions::default()
            },
        )
    }

    fn code_body_lines(output: &MarkdownRenderOutput) -> Vec<&NeutralLine> {
        output
            .lines
            .iter()
            .filter(|line| line.role == LineRole::Code)
            .collect()
    }

    #[test]
    fn fenced_code_propagates_normalized_languages() {
        for (source, expected) in [
            ("```sh\necho hi\n```", Some("sh")),
            ("```BASH\nls -la\n```", Some("bash")),
            ("```Rust\nlet x = 1;\n```", Some("rust")),
            ("```json\n{\"a\":1}\n```", Some("json")),
            ("```JSON {highlight}\n{}\n```", Some("json")),
            ("```.Ts\nconst n = 1\n```", Some("ts")),
            ("```\nplain\n```", None),
        ] {
            let output = render(source, 40);
            let body = code_body_lines(&output);
            assert!(!body.is_empty(), "missing body for {source}");
            for line in body {
                assert_eq!(
                    line.language.as_deref(),
                    expected,
                    "language mismatch for {source}: {:?}",
                    line.text
                );
                assert!(
                    line.text.starts_with("│ "),
                    "body must carry the frame side: {:?}",
                    line.text
                );
                assert!(
                    line.text.ends_with('│'),
                    "body must close the frame side: {:?}",
                    line.text
                );
            }
            assert!(
                output
                    .lines
                    .iter()
                    .filter(|line| line.role == LineRole::CodeFence)
                    .all(|line| line.language.is_none()),
                "fence chrome must stay language-neutral"
            );
            if let Some(lang) = expected {
                assert!(
                    output.lines[0].text.starts_with(&format!("╭── code · {lang}")),
                    "top border must carry the language: {:?}",
                    output.lines[0].text
                );
            } else {
                assert!(
                    output.lines[0].text.starts_with("╭── code"),
                    "language-less top border: {:?}",
                    output.lines[0].text
                );
            }
            let bottom = format!("╰{}╯", "─".repeat(38));
            assert_eq!(
                output.lines.last().map(|line| line.text.as_str()),
                Some(bottom.as_str())
            );
        }
    }

    #[test]
    fn code_fences_preserve_leading_indentation_with_padding() {
        // Contract: fenced code is layout-verbatim inside the frame; tabs expand
        // to four spaces and indentation must survive for Python/Makefile samples.
        let output = render("```\n    indented\n\ttabbed\n  two\n```", 30);
        let expected = format!(
            "╭── code ──{}╮\n│     indented{} │\n│     tabbed{} │\n│   two{} │\n╰{}╯",
            "─".repeat(18),
            " ".repeat(14),
            " ".repeat(16),
            " ".repeat(21),
            "─".repeat(28),
        );
        assert_eq!(output.plain_text(), expected);
        assert!(output.diagnostics.is_empty());
        for line in code_body_lines(&output) {
            assert_eq!(line.language, None);
        }
    }

    #[test]
    fn code_body_wraps_within_width_after_padding() {
        let output = render("```rust\nabcdefghijklmnopqrstuvwxyz\n```", 10);
        assert_eq!(output.lines[0].role, LineRole::CodeFence);
        // The 45-column language label cannot fit a 10-column border, so the
        // frame falls back to a plain top border plus a titled row (tool-card
        // overflow behavior) — both stay width-bounded.
        assert!(
            output.lines[0].text.starts_with('╭'),
            "title chrome missing: {:?}",
            output.lines[0].text
        );
        assert_eq!(
            output.lines[1].text, "│ code … │",
            "fallback titled row missing: {:?}",
            output.lines[1].text
        );
        assert!(
            UnicodeWidthStr::width(output.lines[0].text.as_str()) <= 10,
            "title exceeded width: {:?}",
            output.lines[0].text
        );
        let bottom = format!("╰{}╯", "─".repeat(8));
        assert_eq!(
            output.lines.last().map(|line| line.text.as_str()),
            Some(bottom.as_str())
        );
        let body = code_body_lines(&output);
        assert!(body.len() >= 2, "long line must wrap: {body:?}");
        for line in &body {
            assert!(
                UnicodeWidthStr::width(line.text.as_str()) <= 10,
                "overflow {:?}: width {}",
                line.text,
                UnicodeWidthStr::width(line.text.as_str())
            );
            assert!(line.text.starts_with("│ "));
            assert!(line.text.ends_with('│'));
            assert_eq!(line.language.as_deref(), Some("rust"));
        }
        let rejoined: String = body
            .iter()
            .map(|line| {
                let inner = line.text.strip_prefix("│ ").unwrap_or(&line.text);
                inner
                    .strip_suffix(" │")
                    .unwrap_or(inner)
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert_eq!(rejoined, "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn fenced_max_width_line_aligns_frame() {
        // A body line exactly as wide as the content band must stay inside
        // the frame: every output row (top, body, bottom) is exactly `width`
        // cells.
        for width in [10usize, 20, 30, 76, 80] {
            let content_width = width.saturating_sub(4).max(1);
            let source = format!("```\n{}\n```", "a".repeat(content_width));
            let output = render(&source, width);
            for line in &output.lines {
                assert_eq!(
                    UnicodeWidthStr::width(line.text.as_str()),
                    width,
                    "row must be exactly {width} wide at width {width}: {:?}",
                    line.text
                );
            }
        }
    }

    #[test]
    fn fenced_leading_space_lines_align_frame() {
        // Indented code (Makefile/trace style) keeps its leading whitespace
        // and every frame row stays exactly `width` cells.
        for width in [20, 30, 40, 76] {
            let source =
                "```\n  two\n    four\n        eight\nbusy + Steer --> session.steer\n```";
            let output = render(source, width);
            assert!(
                output.plain_text().contains("  two"),
                "leading indentation must survive at {width}: {:?}",
                output.plain_text()
            );
            for line in &output.lines {
                assert_eq!(
                    UnicodeWidthStr::width(line.text.as_str()),
                    width,
                    "row must be exactly {width} wide at width {width}: {:?}",
                    line.text
                );
            }
        }
    }

    #[test]
    fn fenced_wide_cjk_narrow_widths_no_overflow() {
        // A CJK character wider than the content band must clamp to the
        // ellipsis form instead of pushing the right border past the corner
        // (the lossless overflow row used to escape the frame, RC1).
        for width in [5, 6, 7, 8, 10, 30] {
            let output = render("```\n你好\n```", width);
            let plain = output.plain_text();
            for line in &output.lines {
                assert_eq!(
                    UnicodeWidthStr::width(line.text.as_str()),
                    width,
                    "row must be exactly {width} wide at width {width}: {plain:?}"
                );
            }
        }
        // The narrowest frame cannot fit the 2-cell cluster: the body must
        // show the clamped ellipsis, never the raw wide character.
        let narrow = render("```\n你好\n```", 5);
        assert!(
            narrow.plain_text().contains('…'),
            "overflow row must clamp to the ellipsis: {:?}",
            narrow.plain_text()
        );
    }

    #[test]
    fn fenced_emoji_and_regional_flag_align() {
        // Multi-code-point graphemes (flags, ZWJ families) count their true
        // terminal width: body rows stay exactly frame-width instead of
        // under-padding when measured char-by-char (RC3).
        for width in [10, 20, 30] {
            let output = render("```\n🚀🚀🚀\n🇯🇵\n👨‍👩‍👧\n```", width);
            let plain = output.plain_text();
            for line in &output.lines {
                assert_eq!(
                    UnicodeWidthStr::width(line.text.as_str()),
                    width,
                    "row must be exactly {width} wide at width {width}: {plain:?}"
                );
            }
        }
    }

    #[test]
    fn fenced_box_drawing_content_non_cjk_aligns() {
        // Ambiguous glyphs (`│ ─ → ·`) count 1 cell in the neutral width
        // function, so non-CJK-locale frames align exactly. This documents
        // RC4: divergence only appears in CJK-locale terminals, where these
        // glyphs render 2 cells wide (see `display_width`'s doc note).
        let output = render("```\n│ a ─→ b · c\n╭──╮\n```", 30);
        let plain = output.plain_text();
        for line in &output.lines {
            assert_eq!(
                UnicodeWidthStr::width(line.text.as_str()),
                30,
                "row must be exactly 30 wide: {plain:?}"
            );
        }
    }

    #[test]
    fn empty_and_unclosed_fences_stay_explicit() {
        let empty = render("```bash\n```", 20);
        let empty_expected = format!(
            "╭── code · bash ───╮\n│ {} │\n╰{}╯",
            " ".repeat(16),
            "─".repeat(18),
        );
        assert_eq!(empty.plain_text(), empty_expected);
        assert_eq!(code_body_lines(&empty).len(), 1);
        assert_eq!(
            code_body_lines(&empty)[0].text,
            format!("│ {} │", " ".repeat(16))
        );
        assert_eq!(code_body_lines(&empty)[0].language.as_deref(), Some("bash"));
        assert!(empty.diagnostics.is_empty());

        let blank_body = render("```sh\n\n```", 20);
        // A blank source line still yields one padded body row via wrap_verbatim.
        let blank_expected = format!(
            "╭── code · sh ─────╮\n│ {} │\n╰{}╯",
            " ".repeat(16),
            "─".repeat(18),
        );
        assert_eq!(blank_body.plain_text(), blank_expected);
        assert_eq!(code_body_lines(&blank_body)[0].language.as_deref(), Some("sh"));

        let unclosed = render_markdown_streaming(
            "```json\n{\"a\":1}",
            &MarkdownRenderOptions {
                width: 24,
                ..MarkdownRenderOptions::default()
            },
        );
        // A still-open fence renders a COMPLETE frame: tool-card top border,
        // side-bordered body rows, and a temporary bottom that carries the
        // unclosed marker instead of faking a closed block. At width 24 the
        // 18-column marker cannot fit the border line, so the frame falls
        // back to an inner marker row above a plain bottom border.
        let unclosed_expected = format!(
            "╭── code · json ──{}╮\n│ {{\"a\":1}}{} │\n│ … (unclosed fence){} │\n╰{}╯",
            "─".repeat(5),
            " ".repeat(13),
            " ".repeat(2),
            "─".repeat(22),
        );
        assert_eq!(unclosed.plain_text(), unclosed_expected);
        assert!(matches!(
            unclosed.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
        assert!(code_body_lines(&unclosed)
            .iter()
            .all(|line| line.language.as_deref() == Some("json")));
        // The marker bottom is fence chrome, so it stays language-neutral.
        assert!(
            unclosed
                .lines
                .iter()
                .filter(|line| line.role == LineRole::CodeFence)
                .all(|line| line.language.is_none()),
            "unclosed marker chrome must stay language-neutral"
        );
    }

    #[test]
    fn unclosed_fences_get_full_frame_in_complete_mode_too() {
        // The complete (non-streaming) renderer must not pretend a never-closed
        // fence ended: same full frame plus marker bottom as streaming mode.
        let unclosed = render("```python\ndef f():\n    return 1\n", 30);
        // Width 30 fits the 18-column marker on the border line: the bottom
        // embeds `… (unclosed fence)` between the frame corners.
        let expected = format!(
            "╭── code · python ──{}╮\n│ def f():{} │\n│     return 1{} │\n╰── … (unclosed fence) ──{}╯",
            "─".repeat(9),
            " ".repeat(18),
            " ".repeat(14),
            "─".repeat(4),
        );
        assert_eq!(unclosed.plain_text(), expected);
        assert!(matches!(
            unclosed.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
    }

    #[test]
    fn unclosed_fence_marker_falls_back_width_bounded() {
        // Narrow widths: the marker cannot fit the border line, so the frame
        // keeps a plain bottom border plus an inner marker row — every row
        // stays within the width budget.
        let output = render_markdown_streaming(
            "```rust\nlet x = 1;",
            &MarkdownRenderOptions {
                width: 10,
                ..MarkdownRenderOptions::default()
            },
        );
        for line in output.plain_lines() {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 10,
                "unbounded unclosed chrome: {line:?}"
            );
        }
        let plain = output.plain_text();
        assert!(plain.starts_with("╭"), "top border missing: {plain}");
        // The full 18-column marker cannot fit a 10-column frame; the inner
        // marker row carries the truncated `… (un…` form (fit_text), so the
        // marker presence is asserted on its truncated prefix.
        assert!(plain.contains("… (un"), "marker missing: {plain}");
        assert!(plain.ends_with("╯"), "plain bottom border missing: {plain}");
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
    }

    #[test]
    fn fence_title_and_borders_remain_width_bounded() {
        let long_lang = "x".repeat(40);
        let output = render(&format!("```{long_lang}\ncode\n```"), 12);
        for line in output.plain_lines() {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 12,
                "unbounded chrome: {line:?}"
            );
        }
        // The label cannot fit the 12-column border: the frame uses a plain
        // top border and keeps the truncated label on a titled row.
        assert_eq!(output.lines[0].text, format!("╭{}╮", "─".repeat(10)));
        assert_eq!(output.lines[1].text, "│ code · … │");
        assert_eq!(output.lines[0].role, LineRole::CodeFence);
        assert_eq!(output.lines[1].role, LineRole::CodeFence);
        assert_eq!(output.lines.last().map(|line| line.role), Some(LineRole::CodeFence));
        assert!(code_body_lines(&output)
            .iter()
            .all(|line| line.language.as_deref() == Some(long_lang.as_str())));
    }

    fn mermaid_card_count(text: &str) -> usize {
        text.lines()
            .filter(|line| line.contains("┌─ mermaid ·"))
            .count()
    }

    fn mermaid_footer_count(output: &MarkdownRenderOutput) -> usize {
        output
            .lines
            .iter()
            .filter(|line| line.role == LineRole::MermaidBorder && line.text == "└─")
            .count()
    }

    #[test]
    fn successful_class_diagram_card_uses_class_title_once() {
        // Exact user-reported classDiagram: members + Application-->Session and
        // Agent..>AgentTool : via context. Successful render is one class card.
        let source = "```mermaid\n\
classDiagram\n\
class Application {\n\
+run()\n\
}\n\
class Session {\n\
+id: String\n\
}\n\
class Agent {\n\
+tools: Vec\n\
}\n\
class AgentTool {\n\
+name: String\n\
}\n\
Application --> Session\n\
Agent ..> AgentTool : via context\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 1, "{text}");
        assert!(text.contains("┌─ mermaid · classDiagram"), "{text}");
        assert!(!text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(text.contains("+run()"), "{text}");
        assert!(text.contains("+id: String"), "{text}");
        assert!(text.contains("Application ───▶ Session"), "{text}");
        assert!(text.contains("Agent ··via context··▶ AgentTool"), "{text}");
        assert_eq!(mermaid_footer_count(&output), 1, "{text}");
        assert_eq!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::Diagnostic)
                .count(),
            0
        );
    }

    #[test]
    fn successful_labeled_subgraph_flowchart_card_is_single() {
        // Exact user-reported flowchart LR with subgraph records["SessionRecord types"].
        let source = "```mermaid\n\
flowchart LR\n\
subgraph records[\"SessionRecord types\"]\n\
A[Session] --> B[Message]\n\
B --> C[ToolCall]\n\
end\n\
X[User] --> A\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 1, "{text}");
        assert!(text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(text.contains("subgraph records · SessionRecord types"), "{text}");
        assert!(text.contains("end subgraph records"), "{text}");
        assert!(text.contains("A · Session"), "{text}");
        assert!(text.contains("X ───▶ A"), "{text}");
        assert_eq!(mermaid_footer_count(&output), 1, "{text}");
    }

    #[test]
    fn over_budget_flowchart_card_splits_into_numbered_frames() {
        // A 112-node chain needs ~5k cells (per-chunk budget 3_200, 4x total
        // ceiling): the card must render as two numbered frames with stub
        // notes on the crossing edge instead of falling back to the source.
        let mut diagram = String::from("flowchart TD\n");
        for i in 1..112 {
            diagram.push_str(&format!("A{i} --> A{}\n", i + 1));
        }
        let source = format!("```mermaid\n{diagram}```");
        let output = render_markdown(
            &source,
            &MarkdownRenderOptions {
                width: 48,
                mermaid: MermaidLimits {
                    max_nodes: 256,
                    max_edges: 256,
                    ..MermaidLimits::default()
                },
                ..MarkdownRenderOptions::default()
            },
        );
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 2, "{text}");
        assert!(text.contains("┌─ mermaid · flowchart [part 1/2]"), "{text}");
        assert!(text.contains("┌─ mermaid · flowchart [part 2/2]"), "{text}");
        assert!(
            text.lines()
                .filter(|line| line.starts_with("┌─ mermaid · flowchart"))
                .all(|line| line.contains("[part")),
            "split cards must never use the unnumbered title: {text}"
        );
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        // The single crossing edge appears once as a source stub and once as
        // a target stub (in the exact same order they read in the panels).
        assert!(text.contains("│ A72 → …"), "{text}");
        assert!(text.contains("│ … → A73"), "{text}");
        assert_eq!(mermaid_footer_count(&output), 2, "{text}");
    }

    #[test]
    fn reported_subgraph_flowchart_card_never_falls_back_across_widths() {
        // Exact user-reported shape: subgraphs CWD/TR/RM/AD/PJ/SNAP/SESS/
        // EXT/ORCH/SBX with boxed nodes and labeled edges. At wide widths the
        // rendered cells used to cross the 4x total ceiling, so the card fell
        // back to the raw source; it must render as one or more numbered
        // diagram frames at every width, with no source fallback and no
        // diagnostic.
        let source = format!("```mermaid\n{}\n```", super::super::mermaid::reported_subgraph_flowchart_source());
        for width in [48usize, 60, 80, 100, 120, 160, 200] {
            let output = render(&source, width);
            let text = output.plain_text();
            assert!(mermaid_card_count(&text) >= 1, "width {width}: {text}");
            assert!(text.contains("┌─ mermaid · flowchart"), "width {width}: {text}");
            assert!(!text.contains("source fallback"), "width {width}: {text}");
            assert!(!text.contains("! mermaid:"), "width {width}: {text}");
            assert!(output.diagnostics.is_empty(), "width {width}: {:?}", output.diagnostics);
            assert_eq!(
                output
                    .lines
                    .iter()
                    .filter(|line| line.role == LineRole::Diagnostic)
                    .count(),
                0,
                "width {width}"
            );
            for id in ["CWD", "TR", "RM", "AD", "PJ", "SNAP", "SESS", "EXT", "ORCH", "SBX"] {
                assert!(text.contains(&format!("subgraph {id} ·")), "width {width}: {text}");
            }
        }
    }

    #[test]
    fn successful_sequence_diagram_card_is_single_and_truthful() {
        // Sequence participants, aliases, arrows, note, and an unknown-arrow
        // line that stays visible as a plain row rather than failing the card.
        let source = "```mermaid\n\
sequenceDiagram\n\
participant A as Alice\n\
actor B as Bob\n\
A ->> B: hello\n\
B -->> A: ack\n\
note over A, B: working\n\
A => B: hmm\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 1, "{text}");
        assert!(text.contains("┌─ mermaid · sequenceDiagram"), "{text}");
        assert!(!text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(text.contains("Alice ── Bob"), "{text}");
        assert!(text.contains("│ │ Alice │──▶│ Bob : hello │"), "{text}");
        assert!(text.contains("│ │ Bob   │···▶│ Alice : ack │"), "{text}");
        assert!(text.contains("│ │ [note over A, B: working] │"), "{text}");
        assert!(text.contains("│ │ A => B: hmm │"), "{text}");
        assert_eq!(mermaid_footer_count(&output), 1, "{text}");
    }

    #[test]
    fn unsupported_mermaid_kind_card_shows_source_and_diagnostic() {
        // pie is not a supported diagram kind: the card must fall back to the
        // verbatim source plus a diagnostic rather than a fabricated layout.
        let source = "```mermaid\n\
pie\n\
\"Breakfast\" 5\n\
\"Lunch\" 3\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 1, "{text}");
        assert!(text.contains("┌─ mermaid · source fallback"), "{text}");
        assert!(text.contains("\"Breakfast\" 5"), "{text}");
        assert!(text.contains("! mermaid:"), "{text}");
        assert!(!output.diagnostics.is_empty(), "{text}");
        assert_eq!(mermaid_footer_count(&output), 1, "{text}");
    }

    #[test]
    fn class_and_flowchart_cards_do_not_duplicate_fallback_chrome() {
        let source = "```mermaid\n\
classDiagram\n\
class Application {\n\
+run()\n\
}\n\
class Session {\n\
+id: String\n\
}\n\
class Agent {\n\
+tools: Vec\n\
}\n\
class AgentTool {\n\
+name: String\n\
}\n\
Application --> Session\n\
Agent ..> AgentTool : via context\n\
```\n\n```mermaid\n\
flowchart LR\n\
subgraph records[\"SessionRecord types\"]\n\
A[Session] --> B[Message]\n\
B --> C[ToolCall]\n\
end\n\
X[User] --> A\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(mermaid_card_count(&text), 2, "{text}");
        assert!(text.contains("┌─ mermaid · classDiagram"), "{text}");
        assert!(text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert_eq!(mermaid_footer_count(&output), 2, "{text}");
        assert_eq!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::Diagnostic)
                .count(),
            0
        );
    }

    #[test]
    fn realistic_prose_tables_are_bounded_and_keep_semantic_ranges() {
        let source = "| **Document** | Role |\n\
| --- | --- |\n\
| Architecture decision record | Explains why committed conversation remains separate from transient composer frames |\n\
| Contributor guide | Shows maintainers how to extend `table rendering` without losing semantic styles |\n\n\
| Approach | Strengths | Trade-offs |\n\
| --- | --- | --- |\n\
| Shared renderer | One wrapping implementation serves print and live terminal views | Adapters consume explicit semantic ranges |\n\
| TUI reparsing | Quick to prototype | Duplicates parsing and can recolor ordinary content |";
        let output = render(source, 78);
        assert!(output.lines.iter().all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 78));
        assert_eq!(output.lines.iter().filter(|line| line.role == LineRole::TableBorder).count(), 6);
        assert!(output.lines.iter().any(|line| {
            line.role == LineRole::TableHeader
                && line.inline_styles.iter().any(|styled| styled.style == super::super::inline::InlineStyle::Bold)
        }));
        assert!(output.lines.iter().any(|line| {
            line.role == LineRole::TableBody
                && line.inline_styles.iter().any(|styled| styled.style == super::super::inline::InlineStyle::Code)
        }));
        for line in output.lines.iter().filter(|line| {
            matches!(line.role, LineRole::TableBorder | LineRole::TableHeader | LineRole::TableBody)
        }) {
            for (offset, character) in line.text.char_indices().filter(|(_, character)| "┌┬┐├┼┤│└┴┘─".contains(*character)) {
                assert!(line.inline_styles.iter().any(|styled| {
                    styled.style == super::super::inline::InlineStyle::TableBorder
                        && styled.range.start <= offset
                        && styled.range.end >= offset + character.len_utf8()
                }), "unmarked border {character:?} in {:?}", line.text);
            }
        }
    }

    fn list_marker_range(line: &NeutralLine) -> Option<&InlineStyleRange> {
        line.inline_styles
            .iter()
            .find(|styled| styled.style == super::super::inline::InlineStyle::ListMarker)
    }

    #[test]
    fn list_lines_are_base_text_with_marker_style_only_on_the_prefix() {
        // OMP colors only the bullet/number; the item body (and any nested
        // indent) stays in the base text style. The renderer expresses this as
        // base Text lines whose FIRST row carries an InlineStyle::ListMarker
        // range covering the marker prefix (after the leading indent) only.
        let output = render("- 第一条要点\n1. 编号步骤\n  2. 嵌套项", 40);
        let lines = &output.lines;
        assert_eq!(lines.len(), 3, "one row per item: {lines:?}");
        for line in lines {
            assert_eq!(line.role, LineRole::Text, "list lines are base text: {line:?}");
        }
        assert_eq!(lines[0].text, "• 第一条要点");
        // "•" is 3 UTF-8 bytes + trailing space = 4 bytes.
        assert_eq!(list_marker_range(&lines[0]).expect("bullet marker").range, (0..4));
        assert_eq!(lines[1].text, "1. 编号步骤");
        assert_eq!(list_marker_range(&lines[1]).expect("ordered marker").range, (0..3));
        // Nested item: the 2-space indent is outside the marker range so the
        // indent renders in the base color, not the marker color.
        assert_eq!(lines[2].text, "  2. 嵌套项");
        assert_eq!(list_marker_range(&lines[2]).expect("nested marker").range, (2..5));
    }

    #[test]
    fn list_item_with_empty_text_keeps_the_marker_row() {
        // A bullet or number with no item text (`- ` / `1. `) must still
        // render its marker row instead of vanishing or panicking: the item
        // body is empty, but the marker prefix keeps the list line visible.
        let output = render("- \n- 第二条\n1. ", 40);
        let lines = &output.lines;
        assert_eq!(lines.len(), 3, "every item renders a row: {lines:?}");
        // "•" is 3 UTF-8 bytes + trailing space = 4 bytes; the marker range
        // covers exactly the visible prefix even with an empty body.
        assert_eq!(lines[0].text, "• ");
        assert_eq!(list_marker_range(&lines[0]).expect("empty bullet marker").range, (0..4));
        assert_eq!(lines[1].text, "• 第二条");
        assert_eq!(lines[2].text, "1. ");
        assert_eq!(list_marker_range(&lines[2]).expect("empty ordered marker").range, (0..3));
    }

    #[test]
    fn task_list_items_render_distinct_checkbox_glyphs() {
        // `- [ ]` / `- [x]` render as `☐` (unchecked) / `☑` (checked) glyphs,
        // never as literal `[ ]` / `[x]` text. `[X]` is the same as `[x]`.
        let output = render("- [ ] open\n- [x] done\n- [X] DONE", 40);
        let lines = &output.lines;
        assert_eq!(lines.len(), 3, "one row per item: {lines:?}");
        for line in lines {
            assert_eq!(line.role, LineRole::Text, "list lines are base text: {line:?}");
        }
        assert_eq!(lines[0].text, "• ☐ open");
        assert_eq!(lines[1].text, "• ☑ done");
        assert_eq!(lines[2].text, "• ☑ DONE");
        // The checkbox is chrome like the bullet: the whole `• ☐ ` prefix
        // (8 UTF-8 bytes) carries the ListMarker style; the body is unstyled.
        assert_eq!(list_marker_range(&lines[0]).expect("unchecked marker").range, (0..8));
        assert_eq!(list_marker_range(&lines[1]).expect("checked marker").range, (0..8));
        assert_eq!(
            lines[0].inline_styles.len(),
            1,
            "item body must stay base text: {:?}",
            lines[0]
        );
    }

    #[test]
    fn task_list_item_with_empty_body_keeps_the_checkbox() {
        // `- [ ]` / `- [x]` with no body text keep the checkbox row instead of
        // vanishing or dropping the marker.
        let output = render("- [ ]\n- [x]\n- 第二条", 40);
        let lines = &output.lines;
        assert_eq!(lines.len(), 3, "every item renders a row: {lines:?}");
        assert_eq!(lines[0].text, "• ☐ ");
        assert_eq!(list_marker_range(&lines[0]).expect("empty unchecked").range, (0..8));
        assert_eq!(lines[1].text, "• ☑ ");
        assert_eq!(lines[2].text, "• 第二条");
    }

    #[test]
    fn task_list_allows_multiple_spaces_before_the_bracket() {
        // `-  [ ] item` (extra spaces after the bullet) must still parse as a
        // task item.
        let output = render("-  [ ] spaced\n-   [x] extra", 40);
        let lines = &output.lines;
        assert_eq!(lines[0].text, "• ☐ spaced");
        assert_eq!(lines[1].text, "• ☑ extra");
    }

    #[test]
    fn nested_task_list_items_render_with_indented_checkbox() {
        // Nested task lists keep the leading indent outside the marker range,
        // exactly like nested plain bullets.
        let output = render("- [ ] outer\n  - [x] inner", 40);
        let lines = &output.lines;
        assert_eq!(lines[0].text, "• ☐ outer");
        assert_eq!(lines[1].text, "  • ☑ inner");
        assert_eq!(
            list_marker_range(&lines[1]).expect("nested checkbox").range,
            (2..10)
        );
    }

    #[test]
    fn non_task_brackets_stay_literal_item_text() {
        // `[~]` is not a standard task marker and `[ ]` glued to the body
        // (no whitespace after `]`) is not a task marker either: both render
        // as literal text inside the item body.
        let output = render("- [~] pending\n- [ ]no-space", 40);
        let lines = &output.lines;
        assert_eq!(lines[0].text, "• [~] pending");
        assert_eq!(lines[1].text, "• [ ]no-space");
        // The literal brackets are body text: the marker range still covers
        // only the bullet prefix.
        assert_eq!(list_marker_range(&lines[0]).expect("bullet marker").range, (0..4));
    }

    #[test]
    fn wrapped_task_list_items_keep_the_checkbox_on_the_first_row_only() {
        // The checkbox glyph participates in prefix width like any marker:
        // wrapping keeps `☐ ` on the first row and indents continuations by
        // the full prefix display width (3 cells — the glyphs are width 1).
        let output = render("- [ ] long task item", 8);
        let lines = &output.lines;
        assert!(lines.len() >= 2, "long task must wrap: {lines:?}");
        assert_eq!(lines[0].text, "• ☐ long");
        for line in &lines[1..] {
            assert!(
                line.text.starts_with("   "),
                "continuation indents by prefix width: {line:?}"
            );
            assert!(
                line.inline_styles
                    .iter()
                    .all(|styled| styled.style != super::super::inline::InlineStyle::ListMarker),
                "continuation rows must not carry the marker style: {line:?}"
            );
        }
    }

    #[test]
    fn wrapped_list_items_keep_the_marker_on_the_first_row_only() {
        let output = render("- 这是第一条特别长的要点，需要折行显示", 10);
        let lines = &output.lines;
        assert!(lines.len() >= 2, "long item must wrap: {lines:?}");
        for line in lines {
            assert_eq!(line.role, LineRole::Text);
        }
        assert!(
            lines[0].inline_styles.iter().any(|styled| {
                styled.style == super::super::inline::InlineStyle::ListMarker
                    && styled.range == (0..4)
            }),
            "first row carries the marker range: {:?}",
            lines[0]
        );
        for line in &lines[1..] {
            assert!(
                line.inline_styles
                    .iter()
                    .all(|styled| styled.style != super::super::inline::InlineStyle::ListMarker),
                "continuation rows must not carry the marker style: {line:?}"
            );
            assert!(
                line.text.starts_with(' '),
                "continuation keeps the prefix-width indent: {line:?}"
            );
        }
        let rejoined: String = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    line.text.trim_start_matches("• ").to_owned()
                } else {
                    line.text.trim_start().to_owned()
                }
            })
            .collect();
        assert_eq!(rejoined, "这是第一条特别长的要点，需要折行显示");
    }

    #[test]
    fn d78_cjk_sequence_block_streams_and_settles_without_panic() {
        // The exact run-1 crash trigger (D78TmuxRepro F2): a sequenceDiagram
        // whose CJK block keywords (`loop 每个模型回合`, `alt 有工具调用`,
        // `else 无工具调用`) put byte 10 inside a multibyte char, so
        // `keyword_tail` used to slice `statement[..10]` at a non-char
        // boundary and panicked mid-mermaid-stream (exit 101). The full
        // block must stream (unclosed fence → code frame) and settle (closed
        // fence → rendered sequence card) without panicking.
        let diagram = "\
sequenceDiagram
    autonumber
    actor U as 用户
    participant AP as \"Application<br/>application.rs:1402\"
    participant SE as \"Session<br/>session.rs:3685 run()\"
    participant AG as \"pi-agent Agent<br/>agent.rs / loop_runtime.rs\"
    participant PR as \"pi-ai provider<br/>stream\"
    participant TL as \"Tool 目录<br/>tools.rs\"

    U->>AP: prompt(text)
    AP->>SE: run(prompt)
    SE->>SE: run_messages(messages)
    SE->>SE: inject_selection_messages()<br/>selector 选择 + memory 注入
    SE->>SE: begin_run() → ClaimedRun
    SE->>SE: execute_with_retries()<br/>session.rs:3853
    SE->>AG: agent.prompt_messages()
    AG->>AG: run_agent_loop → run_loop
    loop 每个模型回合
        AG->>PR: 流式请求（含工具定义）
        PR-->>AG: assistant 消息 / tool_call
        alt 有工具调用
            AG->>TL: 执行 AgentTool.execute()
            TL-->>AG: ToolResult
            AG->>AG: 工具结果回合继续
        else 无工具调用
            AG-->>SE: 回合结束（stop_reason）
        end
    end
    SE->>SE: finish_run() → 记录/事件
    SE-->>AP: RunResult
    AP-->>U: 渲染结果";
        let source = format!("```mermaid\n{diagram}\n```");
        let options = MarkdownRenderOptions {
            width: 96,
            ..MarkdownRenderOptions::default()
        };

        // Stream it in small char-boundary chunks like provider deltas; every
        // prefix must render without panicking (this is the path that crashed
        // in the field).
        let boundaries = source
            .char_indices()
            .map(|(index, _)| index)
            .chain([source.len()])
            .collect::<Vec<_>>();
        let mut renderer = StreamingMarkdownRenderer::new(options.clone());
        let mut previous = 0usize;
        for &end in boundaries.iter().step_by(24).skip(1) {
            renderer.push_str(&source[previous..end]);
            previous = end;
            assert!(
                !renderer.output().lines.is_empty(),
                "every streaming prefix must render rows"
            );
        }
        if previous < source.len() {
            renderer.push_str(&source[previous..]);
        }
        let streamed = renderer.output();
        assert!(
            streamed
                .lines
                .iter()
                .any(|line| line.text.contains("┌─ mermaid · sequenceDiagram")),
            "closed fence must render the sequence card: {}",
            streamed.plain_text()
        );

        // The settled (complete) render must also succeed — this is the
        // transcript re-render path after the stream settles.
        let complete = render_markdown(&source, &options);
        let text = complete.plain_text();
        assert!(text.contains("┌─ mermaid · sequenceDiagram"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(text.contains("loop 每个模型回合"), "{text}");
        assert!(text.contains("alt 有工具调用"), "{text}");
        assert!(text.contains("else 无工具调用"), "{text}");
        assert!(complete.diagnostics.is_empty(), "{:?}", complete.diagnostics);
    }

    #[test]
    fn streaming_and_complete_render_survive_hostile_mermaid_content() {
        // D78's fallback content shapes (CJK labels, `<br/>`, quoted
        // subgraph titles, classDef lines, stateDiagram-v2, pie, multi-node
        // edges) must never panic the mermaid path: every statement either
        // parses, falls back to source, or produces a diagnostic — none may
        // unwind, in complete mode or in the streaming tail at ANY valid
        // prefix length.
        let hostile = [
            // Run-1 fallback shapes: CJK/`<br/>` node labels + classDef line.
            "flowchart LR\nMEMTOOL[\"memory · recall · retain · reflect\"]\nMCPTOOL[\"mcp_tool（McpRegistry）<br/>mcp.rs:554\"]\nMEMTOOL --> MCPTOOL\nclassDef lay fill:#eef7ff,stroke:#1565c0\nclass L0,L1 lay\n",
            // Quoted subgraph title without an id (run-2 diagram 2 shape).
            "graph TD\nsubgraph \"会话核心\"\nS[\"Session<br/>session.rs\"]\nend\n",
            // stateDiagram-v2 (run-2 diagrams 7-8): always fallback.
            "stateDiagram-v2\n[*] --> Idle: new Session\nIdle --> Running: run / continue_run<br/>begin_run 声明回合\nParked --> Running: hub 消息唤醒/revival\n",
            // Unsupported diagram kind.
            "pie\n\"Breakfast\" 5\n\"Lunch\" 3\n",
            // CJK sequence block keywords.
            "sequenceDiagram\nparticipant U as 用户\nloop 每个模型回合\nU->>S: run(prompt)\nalt 有工具调用\nS->>T: 执行工具\nelse 无工具调用\nend\nend\n",
            // Multi-node edges with quoted labels.
            "flowchart LR\nA[\"x\"] -->|\"y\"| B\nB --> C & D\nA & E --> F\n",
            // classDiagram members + relations.
            "classDiagram\nclass Application {\n+run()\n}\nApplication --> Session\nAgent ..> AgentTool : via context\n",
            // Unterminated bracket mid-statement (partial stream artifact).
            "flowchart LR\nA[\"x\" --> B\n",
        ];
        let options = MarkdownRenderOptions {
            width: 48,
            ..MarkdownRenderOptions::default()
        };
        for block in hostile {
            let source = format!("```mermaid\n{block}\n```");
            let _ = render_markdown(&source, &options);
            let mut renderer = StreamingMarkdownRenderer::new(options.clone());
            renderer.push_str(&source);
            let _ = renderer.output();
            // Every valid prefix length (worst-case partial content).
            let mut boundaries = source.char_indices().map(|(index, _)| index).collect::<Vec<_>>();
            boundaries.push(source.len());
            for end in boundaries {
                let mut renderer = StreamingMarkdownRenderer::new(options.clone());
                renderer.push_str(&source[..end]);
                let _ = renderer.output();
            }
        }
    }
}
