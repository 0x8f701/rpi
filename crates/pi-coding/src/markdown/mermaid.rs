use std::collections::HashMap;

use super::text::{display_width, fit_text, pad_to_width, sanitize_inline};

pub const MAX_MERMAID_SOURCE_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_MERMAID_NODES: usize = 64;
pub const DEFAULT_MAX_MERMAID_EDGES: usize = 128;
pub const DEFAULT_MAX_MERMAID_OUTPUT_CELLS: usize = 3_200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowDirection {
    TopDown,
    BottomUp,
    LeftRight,
    RightLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MermaidNodeShape {
    Rectangle,
    Rounded,
    Circle,
    Diamond,
    Plain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidNode {
    pub id: String,
    pub label: String,
    pub shape: MermaidNodeShape,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidEdge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub arrow: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidFlowchart {
    pub direction: FlowDirection,
    pub nodes: Vec<MermaidNode>,
    pub edges: Vec<MermaidEdge>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MermaidLimits {
    pub max_source_bytes: usize,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_output_cells: usize,
}

impl Default for MermaidLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: MAX_MERMAID_SOURCE_BYTES,
            max_nodes: DEFAULT_MAX_MERMAID_NODES,
            max_edges: DEFAULT_MAX_MERMAID_EDGES,
            max_output_cells: DEFAULT_MAX_MERMAID_OUTPUT_CELLS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MermaidDiagnosticKind {
    OversizeSource,
    OversizeGraph,
    UnsupportedDiagram,
    InvalidSyntax,
    OutputLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidDiagnostic {
    pub kind: MermaidDiagnosticKind,
    pub message: String,
    pub line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MermaidArt {
    pub lines: Vec<String>,
    pub diagram: MermaidFlowchart,
}

pub fn parse_mermaid(
    source: &str,
    limits: MermaidLimits,
) -> Result<MermaidFlowchart, MermaidDiagnostic> {
    if source.len() > limits.max_source_bytes {
        return Err(diagnostic(
            MermaidDiagnosticKind::OversizeSource,
            format!(
                "Mermaid source is {} bytes; limit is {} bytes",
                source.len(),
                limits.max_source_bytes
            ),
            None,
        ));
    }

    let statements = statements(source);
    let Some((header_line, header)) = statements.first() else {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Mermaid diagram is empty",
            None,
        ));
    };
    let header_line = *header_line;
    let direction = parse_header(header).ok_or_else(|| {
        diagnostic(
            MermaidDiagnosticKind::UnsupportedDiagram,
            "Only flowchart/graph diagrams are supported",
            Some(header_line),
        )
    })?;

    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    for (line, statement) in statements.into_iter().skip(1) {
        parse_statement(
            &statement,
            line,
            &mut nodes,
            &mut node_indices,
            &mut edges,
            limits,
        )?;
    }
    if nodes.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Flowchart contains no nodes",
            Some(header_line),
        ));
    }
    Ok(MermaidFlowchart {
        direction,
        nodes,
        edges,
    })
}

pub fn render_mermaid_unicode(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let diagram = parse_mermaid(source, limits)?;
    let width = width.clamp(4, limits.max_output_cells.max(4));
    let estimated_cells = 12usize
        .saturating_add(
            diagram
                .nodes
                .iter()
                .map(|node| {
                    display_width(&node.id)
                        .saturating_add(display_width(&sanitize_inline(&node.label)))
                        .saturating_add(6)
                })
                .sum::<usize>(),
        )
        .saturating_add(
            diagram
                .edges
                .iter()
                .map(|edge| {
                    display_width(&edge.from)
                        .saturating_add(display_width(&edge.to))
                        .saturating_add(
                            edge.label
                                .as_ref()
                                .map_or(0, |label| display_width(label)),
                        )
                        .saturating_add(7)
                })
                .sum::<usize>(),
        );
    if estimated_cells > limits.max_output_cells.saturating_mul(8) {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Mermaid labels exceed the configured output cell budget",
            None,
        ));
    }
    let minimum_lines = 1usize
        .saturating_add(diagram.nodes.len())
        .saturating_add(usize::from(!diagram.edges.is_empty()))
        .saturating_add(diagram.edges.len());
    if minimum_lines > limits.max_output_cells {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Mermaid output cell limit is too small for this graph",
            None,
        ));
    }
    let mut lines = Vec::new();
    let direction = match diagram.direction {
        FlowDirection::TopDown => "TD",
        FlowDirection::BottomUp => "BU",
        FlowDirection::LeftRight => "LR",
        FlowDirection::RightLeft => "RL",
    };
    lines.push(fit_text(&format!("flowchart {direction}"), width));

    for node in &diagram.nodes {
        append_node(&mut lines, node, width);
    }
    if !diagram.edges.is_empty() {
        lines.push(fit_text("edges", width));
        for edge in &diagram.edges {
            let connector = match (&edge.label, edge.arrow) {
                (Some(label), true) => format!(" ─{}─▶ ", sanitize_inline(label)),
                (Some(label), false) => format!(" ─{}── ", sanitize_inline(label)),
                (None, true) => " ───▶ ".to_owned(),
                (None, false) => " ──── ".to_owned(),
            };
            lines.push(fit_text(
                &format!("{}{}{}", edge.from, connector, edge.to),
                width,
            ));
        }
    }

    let cells = lines.iter().map(|line| display_width(line)).sum::<usize>();
    if cells > limits.max_output_cells {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            format!(
                "Rendered Mermaid output needs {cells} cells; limit is {}",
                limits.max_output_cells
            ),
            None,
        ));
    }
    Ok(MermaidArt { lines, diagram })
}

fn append_node(output: &mut Vec<String>, node: &MermaidNode, width: usize) {
    if width < 5 {
        output.push(fit_text(&format!("{}:{}", node.id, node.label), width));
        return;
    }
    let title = format!("{} · {}", node.id, sanitize_inline(&node.label));
    let inner_width = display_width(&title).min(width - 2).max(1);
    let content = pad_to_width(&fit_text(&title, inner_width), inner_width);
    match node.shape {
        MermaidNodeShape::Diamond => output.push(fit_text(&format!("◇ {content} ◇"), width)),
        MermaidNodeShape::Circle => output.push(fit_text(&format!("({content})"), width)),
        MermaidNodeShape::Rounded => {
            output.push(format!("╭{}╮", "─".repeat(inner_width)));
            output.push(format!("│{content}│"));
            output.push(format!("╰{}╯", "─".repeat(inner_width)));
        }
        MermaidNodeShape::Rectangle | MermaidNodeShape::Plain => {
            output.push(format!("┌{}┐", "─".repeat(inner_width)));
            output.push(format!("│{content}│"));
            output.push(format!("└{}┘", "─".repeat(inner_width)));
        }
    }
}

fn parse_header(header: &str) -> Option<FlowDirection> {
    let mut parts = header.split_whitespace();
    let kind = parts.next()?;
    if !kind.eq_ignore_ascii_case("flowchart") && !kind.eq_ignore_ascii_case("graph") {
        return None;
    }
    let direction = parts.next().unwrap_or("TD");
    if parts.next().is_some() {
        return None;
    }
    match direction.to_ascii_uppercase().as_str() {
        "TD" | "TB" => Some(FlowDirection::TopDown),
        "BT" => Some(FlowDirection::BottomUp),
        "LR" => Some(FlowDirection::LeftRight),
        "RL" => Some(FlowDirection::RightLeft),
        _ => None,
    }
}

fn parse_statement(
    statement: &str,
    line: usize,
    nodes: &mut Vec<MermaidNode>,
    node_indices: &mut HashMap<String, usize>,
    edges: &mut Vec<MermaidEdge>,
    limits: MermaidLimits,
) -> Result<(), MermaidDiagnostic> {
    let mut cursor = Cursor::new(statement);
    let first = cursor.node().ok_or_else(|| invalid_line(line, statement))?;
    upsert_node(first.clone(), nodes, node_indices, limits, line)?;
    cursor.whitespace();
    if cursor.done() {
        return Ok(());
    }

    let mut from = first.id;
    while !cursor.done() {
        let (arrow, label) = cursor.edge().ok_or_else(|| invalid_line(line, statement))?;
        let next = cursor.node().ok_or_else(|| invalid_line(line, statement))?;
        upsert_node(next.clone(), nodes, node_indices, limits, line)?;
        if edges.len() >= limits.max_edges {
            return Err(diagnostic(
                MermaidDiagnosticKind::OversizeGraph,
                format!("Mermaid edge limit of {} exceeded", limits.max_edges),
                Some(line),
            ));
        }
        edges.push(MermaidEdge {
            from,
            to: next.id.clone(),
            label,
            arrow,
        });
        from = next.id;
        cursor.whitespace();
    }
    Ok(())
}

fn upsert_node(
    node: MermaidNode,
    nodes: &mut Vec<MermaidNode>,
    indices: &mut HashMap<String, usize>,
    limits: MermaidLimits,
    line: usize,
) -> Result<(), MermaidDiagnostic> {
    if let Some(index) = indices.get(&node.id).copied() {
        if node.label != node.id || node.shape != MermaidNodeShape::Plain {
            nodes[index] = node;
        }
        return Ok(());
    }
    if nodes.len() >= limits.max_nodes {
        return Err(diagnostic(
            MermaidDiagnosticKind::OversizeGraph,
            format!("Mermaid node limit of {} exceeded", limits.max_nodes),
            Some(line),
        ));
    }
    indices.insert(node.id.clone(), nodes.len());
    nodes.push(node);
    Ok(())
}

fn statements(source: &str) -> Vec<(usize, String)> {
    let mut output = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let without_comment = line.split_once("%%").map_or(line, |(before, _)| before);
        for part in without_comment.split(';') {
            let statement = part.trim();
            if !statement.is_empty() {
                output.push((index + 1, statement.to_owned()));
            }
        }
    }
    output
}


struct Cursor<'a> {
    rest: &'a str,
}

impl<'a> Cursor<'a> {
    fn new(rest: &'a str) -> Self {
        Self { rest }
    }

    fn whitespace(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn done(&self) -> bool {
        self.rest.trim().is_empty()
    }

    fn node(&mut self) -> Option<MermaidNode> {
        self.whitespace();
        let id_len = self
            .rest
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.'))
            .last()
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        if id_len == 0 {
            return None;
        }
        let id = self.rest[..id_len].to_owned();
        self.rest = &self.rest[id_len..];
        self.whitespace();

        let Some(open) = self.rest.chars().next() else {
            return Some(MermaidNode {
                label: id.clone(),
                id,
                shape: MermaidNodeShape::Plain,
            });
        };
        let (shape, opening, closing) = match open {
            '[' => (MermaidNodeShape::Rectangle, '[', ']'),
            '(' => {
                if self.rest.starts_with("((") {
                    (MermaidNodeShape::Circle, '(', ')')
                } else {
                    (MermaidNodeShape::Rounded, '(', ')')
                }
            }
            '{' => (MermaidNodeShape::Diamond, '{', '}'),
            _ => {
                return Some(MermaidNode {
                    label: id.clone(),
                    id,
                    shape: MermaidNodeShape::Plain,
                });
            }
        };
        let opening_count = usize::from(self.rest.starts_with(&format!("{opening}{opening}"))) + 1;
        let closing_text = closing.to_string().repeat(opening_count);
        let body_start = opening.len_utf8() * opening_count;
        let closing_index = self.rest[body_start..].find(&closing_text)? + body_start;
        let label = self.rest[body_start..closing_index]
            .trim_matches(['"', '\''])
            .trim()
            .to_owned();
        self.rest = &self.rest[closing_index + closing_text.len()..];
        Some(MermaidNode { id, label, shape })
    }

    fn edge(&mut self) -> Option<(bool, Option<String>)> {
        self.whitespace();
        let arrow = if let Some(rest) = self.rest.strip_prefix("-->") {
            self.rest = rest;
            true
        } else if let Some(rest) = self.rest.strip_prefix("---") {
            self.rest = rest;
            false
        } else {
            return None;
        };
        self.whitespace();
        let label = if let Some(rest) = self.rest.strip_prefix('|') {
            let end = rest.find('|')?;
            let label = sanitize_inline(rest[..end].trim());
            self.rest = &rest[end + 1..];
            Some(label)
        } else {
            None
        };
        self.whitespace();
        Some((arrow, label))
    }
}

fn diagnostic(
    kind: MermaidDiagnosticKind,
    message: impl Into<String>,
    line: Option<usize>,
) -> MermaidDiagnostic {
    MermaidDiagnostic {
        kind,
        message: message.into(),
        line,
    }
}

fn invalid_line(line: usize, statement: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!("Unsupported flowchart syntax: {}", fit_text(statement, 80)),
        Some(line),
    )
}
