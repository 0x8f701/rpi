use crate::markdown::{LineRole, MarkdownRenderOptions, MarkdownRenderOutput, render_markdown};

const EXPORT_MARKDOWN_WIDTH: usize = 100;

pub(super) fn render_markdown_html(source: &str) -> String {
    let rendered = render_markdown(
        source,
        &MarkdownRenderOptions {
            width: EXPORT_MARKDOWN_WIDTH,
            ..MarkdownRenderOptions::default()
        },
    );
    render_neutral_html(&rendered)
}

fn render_neutral_html(rendered: &MarkdownRenderOutput) -> String {
    let mut output = String::new();
    for line in &rendered.lines {
        if !output.is_empty() {
            output.push('\n');
        }
        let role = role_name(line.role);
        output.push_str("<div class=\"md-line md-line--");
        output.push_str(role);
        output.push_str("\">");
        output.push_str(&escape_text(&line.text));
        if line.text.is_empty() {
            output.push_str("&nbsp;");
        }
        output.push_str("</div>");
    }
    if rendered.lines.is_empty() {
        output.push_str("<div class=\"md-line md-line--text\">&nbsp;</div>");
    }
    output
}

const fn role_name(role: LineRole) -> &'static str {
    match role {
        LineRole::Text => "text",
        LineRole::Heading(1) => "heading-1",
        LineRole::Heading(2) => "heading-2",
        LineRole::Heading(3) => "heading-3",
        LineRole::Heading(4) => "heading-4",
        LineRole::Heading(5) => "heading-5",
        LineRole::Heading(_) => "heading-6",
        LineRole::ListMarker => "list-marker",
        LineRole::Quote => "quote",
        LineRole::Code => "code",
        LineRole::CodeFence => "code-fence",
        LineRole::TableBorder => "table-border",
        LineRole::TableHeader => "table-header",
        LineRole::TableBody => "table-body",
        LineRole::MermaidBorder => "mermaid-border",
        LineRole::MermaidNode => "mermaid-node",
        LineRole::MermaidEdge => "mermaid-edge",
        LineRole::Diagnostic => "diagnostic",
        LineRole::ThematicBreak => "thematic-break",
    }
}

fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_adapter_preserves_shared_text_and_roles() {
        let source = "# Heading\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: nope\n```";
        let neutral = render_markdown(
            source,
            &MarkdownRenderOptions {
                width: EXPORT_MARKDOWN_WIDTH,
                ..MarkdownRenderOptions::default()
            },
        );
        let html = render_neutral_html(&neutral);
        for line in neutral.plain_lines() {
            if !line.is_empty() {
                assert!(html.contains(&escape_text(&line)), "missing {line:?}");
            }
        }
        assert!(html.contains("md-line--heading-1"));
        assert!(html.contains("md-line--table-header"));
        assert!(html.contains("md-line--mermaid-edge"));
        assert!(html.contains("md-line--diagnostic"));
        assert!(html.contains("source fallback"));
    }

    #[test]
    fn html_adapter_escapes_untrusted_source() {
        let html = render_markdown_html("<script>alert('x')</script>");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&#39;x&#39;"));
    }
}
