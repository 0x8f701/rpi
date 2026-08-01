use super::analysis::{
    AnalysisMode, ListMarker, MarkdownBlock, analyze_with_limit, streaming_tail_start,
};
use super::mermaid::{
    MermaidDiagnostic, MermaidLimits, render_mermaid_unicode,
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
}

impl NeutralLine {
    fn new(text: impl Into<String>, role: LineRole) -> Self {
        Self {
            text: text.into(),
            role,
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
                    for (index, line) in layout.lines.into_iter().enumerate() {
                        let role = if index == 0 || index == header_rows + 1 || index + 1 == line_count {
                            LineRole::TableBorder
                        } else if index <= header_rows {
                            LineRole::TableHeader
                        } else {
                            LineRole::TableBody
                        };
                        output.lines.push(NeutralLine::new(line, role));
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
            output.lines.push(NeutralLine::new(
                fit_text("┌─ mermaid · flowchart", width),
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
    let title = if info.is_empty() {
        "┌─ code".to_owned()
    } else {
        format!("┌─ code · {}", sanitize_inline(info))
    };
    lines.push(NeutralLine::new(
        fit_text(&title, width),
        LineRole::CodeFence,
    ));
    append_verbatim_wrapped_lines(lines, source.lines(), width, LineRole::Code);
    if source.is_empty() {
        lines.push(NeutralLine::new(String::new(), LineRole::Code));
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
        for wrapped in wrap_text(line, width) {
            output.push(NeutralLine::new(wrapped, role));
        }
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
        .map(|(column, width)| wrap_text(cells.get(column).map(String::as_str).unwrap_or_default(), *width).len())
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
    let wrapped = wrap_text(text, content_width);
    for (index, line) in wrapped.into_iter().enumerate() {
        let continuation = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };
        let mut rendered = String::with_capacity(continuation.len() + line.len());
        rendered.push_str(&continuation);
        rendered.push_str(&line);
        output.push(NeutralLine::new(rendered, role));
    }
}

fn is_mermaid_info(info: &str) -> bool {
    let language = info
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['{', '}', '.']);
    language.eq_ignore_ascii_case("mermaid") || language.eq_ignore_ascii_case("mermaid-js")
}
