//! Ratatui conversion for the shared terminal-neutral Markdown renderer.

use pi_coding::markdown::{
    LineRole, MarkdownRenderOptions, MarkdownRenderOutput, RenderDiagnostic,
    StreamingMarkdownRenderer,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Text},
};

/// Complete semantic style map for every [`LineRole`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkdownRatatuiStyles {
    pub text: Style,
    pub heading_1: Style,
    pub heading_2: Style,
    pub heading_3: Style,
    pub heading_4: Style,
    pub heading_5: Style,
    pub heading_6: Style,
    pub list_marker: Style,
    pub quote: Style,
    pub code: Style,
    pub code_fence: Style,
    pub table_border: Style,
    pub table_header: Style,
    pub table_body: Style,
    pub mermaid_border: Style,
    pub mermaid_node: Style,
    pub mermaid_edge: Style,
    pub diagnostic: Style,
    pub thematic_break: Style,
}

impl Default for MarkdownRatatuiStyles {
    fn default() -> Self {
        let plain = Style::default();
        Self {
            text: plain,
            heading_1: plain.add_modifier(Modifier::BOLD),
            heading_2: plain.add_modifier(Modifier::BOLD),
            heading_3: plain.add_modifier(Modifier::BOLD),
            heading_4: plain.add_modifier(Modifier::BOLD),
            heading_5: plain.add_modifier(Modifier::BOLD),
            heading_6: plain.add_modifier(Modifier::BOLD),
            list_marker: plain,
            quote: plain.add_modifier(Modifier::ITALIC),
            code: plain,
            code_fence: plain,
            table_border: plain,
            table_header: plain.add_modifier(Modifier::BOLD),
            table_body: plain,
            mermaid_border: plain,
            mermaid_node: plain,
            mermaid_edge: plain,
            diagnostic: plain.add_modifier(Modifier::ITALIC),
            thematic_break: plain,
        }
    }
}

impl MarkdownRatatuiStyles {
    #[must_use]
    pub const fn style_for(self, role: LineRole) -> Style {
        match role {
            LineRole::Text => self.text,
            LineRole::Heading(1) => self.heading_1,
            LineRole::Heading(2) => self.heading_2,
            LineRole::Heading(3) => self.heading_3,
            LineRole::Heading(4) => self.heading_4,
            LineRole::Heading(5) => self.heading_5,
            LineRole::Heading(_) => self.heading_6,
            LineRole::ListMarker => self.list_marker,
            LineRole::Quote => self.quote,
            LineRole::Code => self.code,
            LineRole::CodeFence => self.code_fence,
            LineRole::TableBorder => self.table_border,
            LineRole::TableHeader => self.table_header,
            LineRole::TableBody => self.table_body,
            LineRole::MermaidBorder => self.mermaid_border,
            LineRole::MermaidNode => self.mermaid_node,
            LineRole::MermaidEdge => self.mermaid_edge,
            LineRole::Diagnostic => self.diagnostic,
            LineRole::ThematicBreak => self.thematic_break,
        }
    }
}

/// Styled Ratatui output plus the shared renderer diagnostics.
#[derive(Clone, Debug, Default)]
pub struct RatatuiMarkdownOutput {
    pub lines: Vec<Line<'static>>,
    pub diagnostics: Vec<RenderDiagnostic>,
    pub truncated: bool,
}

impl RatatuiMarkdownOutput {
    #[must_use]
    pub fn text(&self) -> Text<'static> {
        Text::from(self.lines.clone())
    }
}

/// Convert neutral Markdown output without reparsing or changing its width.
#[must_use]
pub fn to_ratatui(
    output: &MarkdownRenderOutput,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    RatatuiMarkdownOutput {
        lines: output
            .lines
            .iter()
            .map(|line| {
                let style = styles.style_for(line.role);
                let mut rendered = Line::styled(line.text.clone(), style);
                for span in &mut rendered.spans {
                    span.style = style;
                }
                rendered
            })
            .collect(),
        diagnostics: output.diagnostics.clone(),
        truncated: output.truncated,
    }
}

/// Render bounded Markdown at `width` and convert it to Ratatui lines.
#[must_use]
pub fn render_ratatui_markdown(
    source: &str,
    width: u16,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    let options = MarkdownRenderOptions {
        width: usize::from(width.max(1)),
        ..MarkdownRenderOptions::default()
    };
    to_ratatui(&pi_coding::markdown::render_markdown(source, &options), styles)
}

/// Render bounded in-progress Markdown at `width` and convert it to Ratatui lines.
///
/// Streaming mode keeps incomplete tables and Mermaid fences visible as source
/// until their closing syntax arrives, matching the shared print renderer.
#[must_use]
pub fn render_ratatui_markdown_streaming(
    source: &str,
    width: u16,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    let mut renderer = StreamingRatatuiMarkdownRenderer::new(width, styles);
    renderer.push_str(source);
    renderer.output()
}

/// Stateful streaming adapter that preserves the shared renderer's frozen prefix.
#[derive(Clone, Debug)]
pub struct StreamingRatatuiMarkdownRenderer {
    renderer: StreamingMarkdownRenderer,
    styles: MarkdownRatatuiStyles,
}

impl StreamingRatatuiMarkdownRenderer {
    #[must_use]
    pub fn new(width: u16, styles: MarkdownRatatuiStyles) -> Self {
        Self::with_options(
            MarkdownRenderOptions {
                width: usize::from(width.max(1)),
                ..MarkdownRenderOptions::default()
            },
            styles,
        )
    }

    #[must_use]
    pub fn with_options(
        mut options: MarkdownRenderOptions,
        styles: MarkdownRatatuiStyles,
    ) -> Self {
        options.width = options.width.max(1);
        Self {
            renderer: StreamingMarkdownRenderer::new(options),
            styles,
        }
    }

    pub fn push_str(&mut self, chunk: &str) {
        self.renderer.push_str(chunk);
    }

    #[must_use]
    pub fn output(&self) -> RatatuiMarkdownOutput {
        to_ratatui(&self.renderer.output(), self.styles)
    }

    #[must_use]
    pub fn frozen_source_bytes(&self) -> usize {
        self.renderer.frozen_source_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::markdown::{LineRole, NeutralLine};
    use ratatui::style::{Color, Modifier};

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn every_line_role_maps_to_its_explicit_style() {
        let styles = MarkdownRatatuiStyles {
            text: Style::default().fg(Color::White),
            heading_1: Style::default().fg(Color::Red),
            heading_2: Style::default().fg(Color::Green),
            heading_3: Style::default().fg(Color::Blue),
            heading_4: Style::default().fg(Color::Yellow),
            heading_5: Style::default().fg(Color::Magenta),
            heading_6: Style::default().fg(Color::Cyan),
            list_marker: Style::default().fg(Color::LightBlue),
            quote: Style::default().fg(Color::LightGreen),
            code: Style::default().fg(Color::LightRed),
            code_fence: Style::default().fg(Color::LightYellow),
            table_border: Style::default().fg(Color::DarkGray),
            table_header: Style::default().fg(Color::Gray),
            table_body: Style::default().fg(Color::LightCyan),
            mermaid_border: Style::default().fg(Color::LightMagenta),
            mermaid_node: Style::default().fg(Color::LightGreen),
            mermaid_edge: Style::default().fg(Color::LightBlue),
            diagnostic: Style::default().fg(Color::Red).add_modifier(Modifier::ITALIC),
            thematic_break: Style::default().fg(Color::Gray),
        };
        let roles = [
            LineRole::Text,
            LineRole::Heading(1),
            LineRole::Heading(2),
            LineRole::Heading(3),
            LineRole::Heading(4),
            LineRole::Heading(5),
            LineRole::Heading(6),
            LineRole::Heading(9),
            LineRole::ListMarker,
            LineRole::Quote,
            LineRole::Code,
            LineRole::CodeFence,
            LineRole::TableBorder,
            LineRole::TableHeader,
            LineRole::TableBody,
            LineRole::MermaidBorder,
            LineRole::MermaidNode,
            LineRole::MermaidEdge,
            LineRole::Diagnostic,
            LineRole::ThematicBreak,
        ];
        let neutral = MarkdownRenderOutput {
            lines: roles
                .iter()
                .enumerate()
                .map(|(index, role)| NeutralLine {
                    text: index.to_string(),
                    role: *role,
                })
                .collect(),
            diagnostics: Vec::new(),
            truncated: false,
        };
        let converted = to_ratatui(&neutral, styles);
        for (line, role) in converted.lines.iter().zip(roles) {
            assert_eq!(line.style, styles.style_for(role));
        }
        assert_eq!(converted.lines[7].style, styles.heading_6);
    }

    #[test]
    fn headings_tables_mermaid_and_fallback_match_neutral_text() {
        let source = "# Heading\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: nope\n```";
        let options = MarkdownRenderOptions {
            width: 32,
            ..MarkdownRenderOptions::default()
        };
        let neutral = pi_coding::markdown::render_markdown(source, &options);
        let tui = to_ratatui(&neutral, MarkdownRatatuiStyles::default());
        assert_eq!(
            tui.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
        assert!(tui.lines.iter().any(|line| line_text(line).contains("┌─ mermaid · flowchart")));
        assert!(tui.lines.iter().any(|line| line_text(line).contains("source fallback")));
        assert_eq!(tui.diagnostics, neutral.diagnostics);
    }

    #[test]
    fn streaming_output_matches_neutral_and_keeps_frozen_prefix_stable() {
        let styles = MarkdownRatatuiStyles::default();
        let mut streaming = StreamingRatatuiMarkdownRenderer::new(40, styles);
        streaming.push_str("# Stable\n\nmutable");
        let first = streaming.output();
        let frozen = streaming.frozen_source_bytes();
        assert_eq!(frozen, "# Stable\n".len());
        let prefix = first.lines[..1].iter().map(line_text).collect::<Vec<_>>();

        streaming.push_str(" tail\n\n| a | b |\n| --- | --- |\n| 1 | 2 |");
        let second = streaming.output();
        assert_eq!(
            second.lines[..prefix.len()].iter().map(line_text).collect::<Vec<_>>(),
            prefix
        );
        let source = "# Stable\n\nmutable tail\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
        let neutral = pi_coding::markdown::render_markdown_streaming(
            source,
            &MarkdownRenderOptions {
                width: 40,
                ..MarkdownRenderOptions::default()
            },
        );
        assert_eq!(
            second.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
    }

    #[test]
    fn streaming_adapter_matches_shared_neutral_output_from_chunks() {
        let source = "# Stable\n\n- [x] done\n  2. next\n\n| 名称 | 状态 |\n| --- | --- |\n| 東京 | ✅ |\n\n```mermaid\nflowchart TD\nA --> B";
        let options = MarkdownRenderOptions {
            width: 32,
            ..MarkdownRenderOptions::default()
        };
        let neutral = pi_coding::markdown::render_markdown_streaming(source, &options);
        let mut renderer = StreamingRatatuiMarkdownRenderer::new(
            32,
            MarkdownRatatuiStyles::default(),
        );
        for chunk in [
            "# Stable\n\n- [x] done\n  2. next\n\n| 名称 |",
            " 状态 |\n| --- | --- |\n",
            "| 東京 | ✅ |\n\n```mermaid\nflowchart TD\nA --> B",
        ] {
            renderer.push_str(chunk);
        }
        let tui = renderer.output();
        assert_eq!(
            tui.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
        assert_eq!(tui.diagnostics, neutral.diagnostics);
        assert!(tui.lines.iter().any(|line| line_text(line).contains("flowchart TD")));
        assert!(!tui.lines.iter().any(|line| line_text(line).contains("mermaid · flowchart")));
    }

    #[test]
    fn zero_width_is_safely_clamped() {
        let output = render_ratatui_markdown("---", 0, MarkdownRatatuiStyles::default());
        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].width(), 1);
    }
}
