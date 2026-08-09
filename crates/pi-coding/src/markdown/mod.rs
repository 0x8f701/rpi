//! Bounded, terminal-neutral Markdown analysis and layout.
//!
//! This module owns no terminal, network, process, script, or file execution.
//! Callers receive styled-neutral lines and may map [`LineRole`] to TUI, print,
//! or export presentation.
//!
//! All frame math uses [`display_width`]'s convention (East Asian
//! Wide/Fullwidth = 2 cells, everything else = 1). East Asian **Ambiguous**
//! glyphs count 1 cell, the ECMA-48 neutral default; a CJK-locale terminal
//! paints them 2 cells wide, so verbatim frames can diverge by one cell per
//! Ambiguous glyph there. That is a terminal-locale limitation, not a layout
//! bug — run CJK locales with `LC_CTYPE=C` or a narrow-Ambiguous terminal.

mod analysis;
mod inline;
mod mermaid;
mod render;
mod table;
mod text;

pub use analysis::{
    AnalysisMode, ListItem, ListMarker, MarkdownBlock, MarkdownDocument, analyze_markdown,
    analyze_markdown_with_mode,
};
pub use inline::{InlineStyle, InlineStyleRange};
pub use mermaid::{
    DEFAULT_MAX_MERMAID_EDGES, DEFAULT_MAX_MERMAID_NODES, DEFAULT_MAX_MERMAID_OUTPUT_CELLS,
    FlowDirection, MAX_MERMAID_SOURCE_BYTES, MermaidArt, MermaidDiagnostic,
    MermaidDiagnosticKind, MermaidDiagramKind, MermaidEdge, MermaidFlowchart, MermaidLimits,
    MermaidNode, MermaidNodeShape, parse_mermaid, render_mermaid_unicode,
};
pub use render::{
    LineRole, MarkdownRenderOptions, MarkdownRenderOutput, NeutralLine, RenderDiagnostic,
    StreamingMarkdownRenderer, render_markdown, render_markdown_streaming,
};
pub use table::{TableAlignment, TableBlock, TableLayout, layout_table};
pub use text::{display_width, fit_text, wrap_verbatim};
