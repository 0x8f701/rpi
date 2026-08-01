//! Bounded, terminal-neutral Markdown analysis and layout.
//!
//! This module owns no terminal, network, process, script, or file execution.
//! Callers receive styled-neutral lines and may map [`LineRole`] to TUI, print,
//! or export presentation.

mod analysis;
mod mermaid;
mod render;
mod table;
mod text;

pub use analysis::{
    AnalysisMode, ListItem, ListMarker, MarkdownBlock, MarkdownDocument, analyze_markdown,
    analyze_markdown_with_mode,
};
pub use mermaid::{
    DEFAULT_MAX_MERMAID_EDGES, DEFAULT_MAX_MERMAID_NODES, DEFAULT_MAX_MERMAID_OUTPUT_CELLS,
    FlowDirection, MAX_MERMAID_SOURCE_BYTES, MermaidArt, MermaidDiagnostic,
    MermaidDiagnosticKind, MermaidEdge, MermaidFlowchart, MermaidLimits, MermaidNode,
    MermaidNodeShape, parse_mermaid, render_mermaid_unicode,
};
pub use render::{
    LineRole, MarkdownRenderOptions, MarkdownRenderOutput, NeutralLine, RenderDiagnostic,
    StreamingMarkdownRenderer, render_markdown, render_markdown_streaming,
};
pub use table::{TableAlignment, TableBlock, TableLayout, layout_table};
