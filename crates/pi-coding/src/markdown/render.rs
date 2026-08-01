use std::ops::Range;

use super::analysis::{
    AnalysisMode, ListMarker, MarkdownBlock, analyze_with_limit, streaming_tail_start,
};
use super::inline::{InlineStyleRange, shifted_styles, wrap_inline};
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
                append_wrapped(&mut output.lines, &text, width, LineRole::Heading(level), "");
            }
            MarkdownBlock::List { items, .. } => {
                for item in items {
                    let marker = match item.marker {
                        ListMarker::Bullet(_) => "•".to_owned(),
                        ListMarker::Ordered { number, delimiter } => format!("{number}{delimiter}"),
                    };
                    let task = match item.checked {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let indent = "  ".repeat(item.depth);
                    let prefix = format!("{indent}{marker} {task}");
                    append_wrapped(
                        &mut output.lines,
                        &item.text,
                        width,
                        LineRole::ListMarker,
                        &prefix,
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
                    append_code(&mut output.lines, &info, &source, width);
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
                    append_wrapped(&mut output.lines, &line, width, LineRole::Quote, "│ ");
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
            };
            output.lines.push(NeutralLine::new(
                fit_text(&format!("┌─ mermaid · {kind}"), width),
                LineRole::MermaidBorder,
            ));
            let mut in_edges = false;
            for line in art.lines {
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

fn append_code(lines: &mut Vec<NeutralLine>, info: &str, source: &str, width: usize) {
    let language = normalize_fence_language(info);
    let title = match language.as_deref() {
        Some(lang) => format!("┌─ code · {lang}"),
        None => "┌─ code".to_owned(),
    };
    lines.push(NeutralLine::new(
        fit_text(&title, width),
        LineRole::CodeFence,
    ));

    // One-cell internal horizontal padding; wrap against the remaining width.
    let body_width = width.saturating_sub(1).max(1);
    let mut emitted_body = false;
    for line in source.lines() {
        for wrapped in wrap_verbatim(line, body_width) {
            let mut text = String::with_capacity(wrapped.len().saturating_add(1));
            text.push(' ');
            text.push_str(&wrapped);
            lines.push(NeutralLine::code(text, language.clone()));
            emitted_body = true;
        }
    }
    if !emitted_body {
        // Empty / blank-only fences still expose one explicit padded body row.
        lines.push(NeutralLine::code(" ", language.clone()));
    }

    lines.push(NeutralLine::new(
        fit_text("└─", width),
        LineRole::CodeFence,
    ));
}

fn append_source_lines<'a>(
    output: &mut Vec<NeutralLine>,
    lines: impl IntoIterator<Item = &'a str>,
    width: usize,
    role: LineRole,
) {
    for line in lines {
        append_wrapped(output, line, width, role, "");
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

fn append_wrapped(
    output: &mut Vec<NeutralLine>,
    text: &str,
    width: usize,
    role: LineRole,
    prefix: &str,
) {
    let prefix_width = display_width(prefix);
    if prefix_width >= width {
        output.push(NeutralLine::new(fit_text(prefix, width), role));
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
        output.push(NeutralLine {
            text: rendered,
            role,
            inline_styles: shifted_styles(&line.styles, content_offset),
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
                    line.text.starts_with(' '),
                    "body must keep one-cell pad: {:?}",
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
                assert_eq!(output.lines[0].text, format!("┌─ code · {lang}"));
            } else {
                assert_eq!(output.lines[0].text, "┌─ code");
            }
            assert_eq!(output.lines.last().map(|line| line.text.as_str()), Some("└─"));
        }
    }

    #[test]
    fn code_fences_preserve_leading_indentation_with_padding() {
        // Contract: fenced code is layout-verbatim after one-cell pad; tabs expand
        // to four spaces and indentation must survive for Python/Makefile samples.
        let output = render("```\n    indented\n\ttabbed\n  two\n```", 30);
        assert_eq!(
            output.plain_text(),
            "┌─ code\n     indented\n     tabbed\n   two\n└─"
        );
        assert!(output.diagnostics.is_empty());
        for line in code_body_lines(&output) {
            assert_eq!(line.language, None);
        }
    }

    #[test]
    fn code_body_wraps_within_width_after_padding() {
        let output = render("```rust\nabcdefghijklmnopqrstuvwxyz\n```", 10);
        assert_eq!(output.lines[0].role, LineRole::CodeFence);
        assert!(
            output.lines[0].text.starts_with("┌─"),
            "title chrome missing: {:?}",
            output.lines[0].text
        );
        assert!(
            UnicodeWidthStr::width(output.lines[0].text.as_str()) <= 10,
            "title exceeded width: {:?}",
            output.lines[0].text
        );
        assert_eq!(output.lines.last().map(|line| line.text.as_str()), Some("└─"));
        let body = code_body_lines(&output);
        assert!(body.len() >= 2, "long line must wrap: {body:?}");
        for line in &body {
            assert!(
                UnicodeWidthStr::width(line.text.as_str()) <= 10,
                "overflow {:?}: width {}",
                line.text,
                UnicodeWidthStr::width(line.text.as_str())
            );
            assert!(line.text.starts_with(' '));
            assert_eq!(line.language.as_deref(), Some("rust"));
        }
        let rejoined: String = body
            .iter()
            .map(|line| line.text.trim_start_matches(' '))
            .collect();
        assert_eq!(rejoined, "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn empty_and_unclosed_fences_stay_explicit() {
        let empty = render("```bash\n```", 20);
        assert_eq!(empty.plain_text(), "┌─ code · bash\n \n└─");
        assert_eq!(code_body_lines(&empty).len(), 1);
        assert_eq!(code_body_lines(&empty)[0].text, " ");
        assert_eq!(code_body_lines(&empty)[0].language.as_deref(), Some("bash"));
        assert!(empty.diagnostics.is_empty());

        let blank_body = render("```sh\n\n```", 20);
        // A blank source line still yields one padded body row via wrap_verbatim.
        assert_eq!(blank_body.plain_text(), "┌─ code · sh\n \n└─");
        assert_eq!(code_body_lines(&blank_body)[0].language.as_deref(), Some("sh"));

        let unclosed = render_markdown_streaming(
            "```json\n{\"a\":1}",
            &MarkdownRenderOptions {
                width: 24,
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(unclosed.plain_text().contains("┌─ code · json"));
        assert!(unclosed.plain_text().contains(" {\"a\":1}"));
        assert!(matches!(
            unclosed.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
        assert!(code_body_lines(&unclosed)
            .iter()
            .all(|line| line.language.as_deref() == Some("json")));
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
        assert!(output.lines[0].text.starts_with("┌─ code · "));
        assert_eq!(output.lines[0].role, LineRole::CodeFence);
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
}
