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
pub enum MermaidDiagramKind {
    Flowchart,
    ClassDiagram,
    Sequence,
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
    /// One entry per rendered panel. Diagrams that fit the output budget have
    /// exactly one entry; over-budget flowcharts and class diagrams are split
    /// into ordered panels, each of at most `MermaidLimits::max_output_cells`
    /// cells (see `split_flowchart_chunks` / `split_class_chunks`).
    pub chunks: Vec<Vec<String>>,
    pub diagram: MermaidFlowchart,
    pub kind: MermaidDiagramKind,
}

pub fn parse_mermaid(
    source: &str,
    limits: MermaidLimits,
) -> Result<MermaidFlowchart, MermaidDiagnostic> {
    check_source_limit(source, limits)?;
    let source_statements = statements(source);
    let header_line = source_statements.first().map(|(line, _)| *line);
    let Some(header_line) = header_line else {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Mermaid diagram is empty",
            None,
        ));
    };
    let header = source_statements.first().map(|(_, header)| header.as_str()).unwrap_or_default();
    if header.eq_ignore_ascii_case("classDiagram") {
        return parse_class_diagram(source, limits).map(|parsed| parsed.diagram);
    }
    if is_sequence_header(header) {
        return parse_sequence_diagram(source, limits).map(|parsed| sequence_to_flowchart(&parsed));
    }
    parse_flowchart(source_statements, header_line, limits).map(|parsed| parsed.diagram)
}

pub fn render_mermaid_unicode(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    check_source_limit(source, limits)?;
    let source_statements = statements(source);
    let header_line = source_statements.first().map(|(line, _)| *line);
    let Some(header_line) = header_line else {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Mermaid diagram is empty",
            None,
        ));
    };
    let header = source_statements.first().map(|(_, header)| header.as_str()).unwrap_or_default();
    let width = width.clamp(4, limits.max_output_cells.max(4));

    if header.eq_ignore_ascii_case("classDiagram") {
        return render_class_diagram(source, width, limits);
    }
    if is_sequence_header(header) {
        return render_sequence_diagram(source, width, limits);
    }

    let parsed = parse_flowchart(source_statements, header_line, limits)?;
    let diagram = parsed.diagram;
    let lines = render_flowchart_lines(&diagram, &parsed.subgraphs, width);
    let cells = rendered_cells(&lines);
    if cells > limits.max_output_cells {
        // Over-budget flowcharts split into ordered panels instead of
        // erroring or falling back to the raw source: every panel stays within
        // `max_output_cells`, edges crossing a panel boundary become stub
        // notes, and supported syntax always renders.
        let chunks = split_flowchart_chunks(&diagram, &parsed.subgraphs, width, limits.max_output_cells);
        return Ok(MermaidArt {
            chunks,
            diagram,
            kind: MermaidDiagramKind::Flowchart,
        });
    }
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram,
        kind: MermaidDiagramKind::Flowchart,
    })
}

/// Render a flowchart the way `render_mermaid_unicode` always did: header,
/// nodes in declaration order with interleaved subgraph markers, then the
/// `edges` section. This is the single-chunk output; `split_flowchart_chunks`
/// re-packs the same pieces into bounded panels.
fn render_flowchart_lines(
    diagram: &MermaidFlowchart,
    subgraphs: &[MermaidSubgraph],
    width: usize,
) -> Vec<String> {
    let mut lines = vec![fit_text(
        &format!("flowchart {}", flowchart_direction_text(diagram.direction)),
        width,
    )];
    for (index, node) in diagram.nodes.iter().enumerate() {
        append_flowchart_node_lines(&mut lines, index, node, subgraphs, width);
    }
    if !diagram.edges.is_empty() {
        lines.push(fit_text("edges", width));
        for edge in &diagram.edges {
            lines.push(flowchart_edge_line(edge, width));
        }
    }
    lines
}

fn flowchart_direction_text(direction: FlowDirection) -> &'static str {
    match direction {
        FlowDirection::TopDown => "TD",
        FlowDirection::BottomUp => "BU",
        FlowDirection::LeftRight => "LR",
        FlowDirection::RightLeft => "RL",
    }
}

fn append_flowchart_node_lines(
    output: &mut Vec<String>,
    index: usize,
    node: &MermaidNode,
    subgraphs: &[MermaidSubgraph],
    width: usize,
) {
    for group in subgraphs.iter().filter(|group| group.start_node == index) {
        output.push(fit_text(
            &format!("subgraph {} · {}", group.id, sanitize_mermaid_label(&group.title)),
            width,
        ));
    }
    append_node(output, node, width);
    for group in subgraphs.iter().filter(|group| group.end_node == index + 1) {
        output.push(fit_text(&format!("end subgraph {}", group.id), width));
    }
}

fn flowchart_edge_line(edge: &MermaidEdge, width: usize) -> String {
    let connector = match (&edge.label, edge.arrow) {
        (Some(label), true) => format!(" ─{}─▶ ", sanitize_mermaid_label(label)),
        (Some(label), false) => format!(" ─{}── ", sanitize_mermaid_label(label)),
        (None, true) => " ───▶ ".to_owned(),
        (None, false) => " ──── ".to_owned(),
    };
    fit_text(&format!("{}{}{}", edge.from, connector, edge.to), width)
}

/// Greedily partition `costs` into contiguous chunks, each summing to at most
/// `limit`. A single element that alone exceeds `limit` becomes a chunk of its
/// own (a node cannot be split) rather than being dropped.
fn greedy_chunk_of(costs: &[usize], limit: usize) -> Vec<usize> {
    let mut chunk_of = Vec::with_capacity(costs.len());
    let mut current_cost = 0usize;
    let mut current_chunk = 0usize;
    for (index, cost) in costs.iter().copied().enumerate() {
        if index > 0 && current_cost.saturating_add(cost) > limit {
            current_chunk += 1;
            current_cost = 0;
        }
        chunk_of.push(current_chunk);
        current_cost = current_cost.saturating_add(cost);
    }
    chunk_of
}

/// Split an over-budget flowchart into ordered panels: nodes are packed in
/// declaration order so each panel's rendered cells stay within `limit` (each
/// node unit also carries the rendered cost of its outgoing edges, drawn in
/// the source node's panel). Edges whose endpoints land in different panels
/// become stub notes (`A → …` in the source panel, `… → B` in the target
/// panel); edges whose source panel already holds the target render in full.
fn split_flowchart_chunks(
    diagram: &MermaidFlowchart,
    subgraphs: &[MermaidSubgraph],
    width: usize,
    limit: usize,
) -> Vec<Vec<String>> {
    let header = fit_text(
        &format!("flowchart {}", flowchart_direction_text(diagram.direction)),
        width,
    );
    let mut costs = Vec::with_capacity(diagram.nodes.len());
    for (index, node) in diagram.nodes.iter().enumerate() {
        let mut scratch = Vec::new();
        append_flowchart_node_lines(&mut scratch, index, node, subgraphs, width);
        let mut cost = rendered_cells(&scratch);
        cost = cost.saturating_add(
            diagram
                .edges
                .iter()
                .filter(|edge| edge.from == node.id)
                .map(|edge| display_width(&flowchart_edge_line(edge, width)))
                .sum::<usize>(),
        );
        costs.push(cost);
    }
    if let Some(first) = costs.first_mut() {
        *first = first.saturating_add(display_width(&header));
    }
    let chunk_of_node = greedy_chunk_of(&costs, limit);
    let chunk_count = chunk_of_node.last().copied().unwrap_or(0).saturating_add(1);
    let node_index: HashMap<&str, usize> = diagram
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();
    let last_chunk = chunk_count.saturating_sub(1);
    let from_chunk = |edge: &MermaidEdge| -> usize {
        node_index
            .get(edge.from.as_str())
            .and_then(|index| chunk_of_node.get(*index).copied())
            .unwrap_or(last_chunk)
    };
    let to_chunk = |edge: &MermaidEdge| -> Option<usize> {
        node_index
            .get(edge.to.as_str())
            .and_then(|index| chunk_of_node.get(*index).copied())
    };

    let mut chunks = Vec::with_capacity(chunk_count);
    for chunk in 0..chunk_count {
        let mut lines = Vec::new();
        if chunk == 0 {
            lines.push(header.clone());
        }
        for (index, node) in diagram.nodes.iter().enumerate() {
            if chunk_of_node[index] != chunk {
                continue;
            }
            append_flowchart_node_lines(&mut lines, index, node, subgraphs, width);
        }
        let mut edge_lines: Vec<String> = Vec::new();
        for edge in &diagram.edges {
            let source_chunk = from_chunk(edge);
            let target_chunk = to_chunk(edge).unwrap_or(source_chunk);
            if source_chunk == chunk && target_chunk == chunk {
                edge_lines.push(flowchart_edge_line(edge, width));
            } else if source_chunk == chunk {
                edge_lines.push(fit_text(&format!("{} → …", edge.from), width));
            } else if target_chunk == chunk {
                edge_lines.push(fit_text(&format!("… → {}", edge.to), width));
            }
        }
        if !edge_lines.is_empty() {
            lines.push(fit_text("edges", width));
            lines.extend(edge_lines);
        }
        chunks.push(lines);
    }
    chunks
}

#[derive(Clone, Debug)]
struct ParsedFlowchart {
    diagram: MermaidFlowchart,
    subgraphs: Vec<MermaidSubgraph>,
}

#[derive(Clone, Debug)]
struct MermaidSubgraph {
    id: String,
    title: String,
    start_node: usize,
    end_node: usize,
}

#[derive(Clone, Debug)]
struct ParsedClassDiagram {
    diagram: MermaidFlowchart,
    classes: Vec<MermaidClass>,
    relations: Vec<ClassRelation>,
}

#[derive(Clone, Debug)]
struct MermaidClass {
    name: String,
    members: Vec<String>,
}

#[derive(Clone, Debug)]
struct ClassRelation {
    from: String,
    to: String,
    label: Option<String>,
    dotted: bool,
}

fn check_source_limit(source: &str, limits: MermaidLimits) -> Result<(), MermaidDiagnostic> {
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
    Ok(())
}

fn parse_flowchart(
    source_statements: Vec<(usize, String)>,
    header_line: usize,
    limits: MermaidLimits,
) -> Result<ParsedFlowchart, MermaidDiagnostic> {
    let header = &source_statements[0].1;
    let direction = parse_header(header).ok_or_else(|| {
        diagnostic(
            MermaidDiagnosticKind::UnsupportedDiagram,
            "Only flowchart/graph, classDiagram, and sequenceDiagram diagrams are supported",
            Some(header_line),
        )
    })?;
    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    let mut subgraphs = Vec::new();
    let mut open_subgraph: Option<(String, String, usize, usize)> = None;
    for (line, statement) in source_statements.into_iter().skip(1) {
        if let Some(declaration) = statement.strip_prefix("subgraph").map(str::trim) {
            if declaration.is_empty() || open_subgraph.is_some() {
                return Err(invalid_line(line, &statement));
            }
            let mut cursor = Cursor::new(declaration);
            let group = cursor.node().ok_or_else(|| invalid_line(line, &statement))?;
            if !cursor.done() {
                return Err(invalid_line(line, &statement));
            }
            open_subgraph = Some((group.id, group.label, nodes.len(), line));
            continue;
        }
        if statement.eq_ignore_ascii_case("end") {
            let Some((id, title, start_node, _)) = open_subgraph.take() else {
                return Err(invalid_line(line, &statement));
            };
            subgraphs.push(MermaidSubgraph {
                id,
                title,
                start_node,
                end_node: nodes.len(),
            });
            continue;
        }
        parse_statement(
            &statement,
            line,
            &mut nodes,
            &mut node_indices,
            &mut edges,
            limits,
        )?;
    }
    if let Some((_, _, _, line)) = open_subgraph {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Flowchart subgraph is missing end",
            Some(line),
        ));
    }
    if nodes.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Flowchart contains no nodes",
            Some(header_line),
        ));
    }
    Ok(ParsedFlowchart {
        diagram: MermaidFlowchart {
            direction,
            nodes,
            edges,
        },
        subgraphs,
    })
}

fn parse_class_diagram(
    source: &str,
    limits: MermaidLimits,
) -> Result<ParsedClassDiagram, MermaidDiagnostic> {
    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    let mut classes = Vec::new();
    let mut relations = Vec::new();
    let mut current_class: Option<(String, Vec<String>, usize)> = None;
    let mut saw_header = false;

    for (index, raw_line) in source.lines().enumerate() {
        let line = index + 1;
        let statement = raw_line
            .split_once("%%")
            .map_or(raw_line, |(before, _)| before)
            .trim();
        if statement.is_empty() {
            continue;
        }
        if !saw_header {
            if !statement.eq_ignore_ascii_case("classDiagram") {
                return Err(diagnostic(
                    MermaidDiagnosticKind::UnsupportedDiagram,
                    "Only flowchart/graph, classDiagram, and sequenceDiagram diagrams are supported",
                    Some(line),
                ));
            }
            saw_header = true;
            continue;
        }
        if current_class.is_some() {
            if statement == "}" {
                let (name, members, _) = current_class.take().expect("class is open");
                classes.push(MermaidClass { name, members });
            } else if matches!(statement.chars().next(), Some('+' | '-' | '#' | '~')) {
                current_class
                    .as_mut()
                    .expect("class is open")
                    .1
                    .push(sanitize_mermaid_label(statement));
            } else {
                return Err(invalid_class_line(line, statement));
            }
            continue;
        }
        if let Some(declaration) = statement.strip_prefix("class ") {
            let Some(name) = declaration.strip_suffix('{').map(str::trim) else {
                return Err(invalid_class_line(line, statement));
            };
            if !valid_identifier(name) {
                return Err(invalid_class_line(line, statement));
            }
            let node = MermaidNode {
                id: name.to_owned(),
                label: name.to_owned(),
                shape: MermaidNodeShape::Rectangle,
            };
            upsert_node(node, &mut nodes, &mut node_indices, limits, line)?;
            current_class = Some((name.to_owned(), Vec::new(), line));
            continue;
        }
        let relation = parse_class_relation(statement, line)?;
        for name in [&relation.from, &relation.to] {
            upsert_node(
                MermaidNode {
                    id: (*name).clone(),
                    label: (*name).clone(),
                    shape: MermaidNodeShape::Rectangle,
                },
                &mut nodes,
                &mut node_indices,
                limits,
                line,
            )?;
        }
        if edges.len() >= limits.max_edges {
            return Err(diagnostic(
                MermaidDiagnosticKind::OversizeGraph,
                format!("Mermaid edge limit of {} exceeded", limits.max_edges),
                Some(line),
            ));
        }
        edges.push(MermaidEdge {
            from: relation.from.clone(),
            to: relation.to.clone(),
            label: relation.label.clone(),
            arrow: true,
        });
        relations.push(relation);
    }
    if let Some((_, _, line)) = current_class {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Class block is missing }",
            Some(line),
        ));
    }
    if nodes.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Class diagram contains no classes",
            Some(1),
        ));
    }
    Ok(ParsedClassDiagram {
        diagram: MermaidFlowchart {
            direction: FlowDirection::LeftRight,
            nodes,
            edges,
        },
        classes,
        relations,
    })
}

fn parse_class_relation(statement: &str, line: usize) -> Result<ClassRelation, MermaidDiagnostic> {
    let (left, remainder, dotted) = if let Some((left, right)) = statement.split_once("..>") {
        (left, right, true)
    } else if let Some((left, right)) = statement.split_once("-->") {
        (left, right, false)
    } else {
        return Err(invalid_class_line(line, statement));
    };
    let (right, label) = remainder.split_once(':').map_or((remainder, None), |(right, label)| {
        (right, Some(sanitize_mermaid_label(label.trim())))
    });
    let from = left.trim();
    let to = right.trim();
    if !valid_identifier(from) || !valid_identifier(to) || label.as_deref() == Some("") {
        return Err(invalid_class_line(line, statement));
    }
    Ok(ClassRelation {
        from: from.to_owned(),
        to: to.to_owned(),
        label,
        dotted,
    })
}

fn render_class_diagram(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let parsed = parse_class_diagram(source, limits)?;
    let mut lines = vec![fit_text("classDiagram", width)];
    for node in &parsed.diagram.nodes {
        append_class(
            &mut lines,
            &node.id,
            class_members(&parsed.classes, &node.id),
            width,
        );
    }
    if !parsed.relations.is_empty() {
        lines.push(fit_text("edges", width));
        for relation in &parsed.relations {
            lines.push(class_relation_line(relation, width));
        }
    }
    // Class boxes are self-contained, so over-budget class diagrams split by
    // class into bounded panels (relations crossing a boundary become stub
    // notes) instead of erroring or falling back to the raw source.
    let cells = rendered_cells(&lines);
    if cells > limits.max_output_cells {
        let chunks = split_class_chunks(&parsed, width, limits.max_output_cells);
        return Ok(MermaidArt {
            chunks,
            diagram: parsed.diagram,
            kind: MermaidDiagramKind::ClassDiagram,
        });
    }
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram: parsed.diagram,
        kind: MermaidDiagramKind::ClassDiagram,
    })
}

fn class_members<'a>(classes: &'a [MermaidClass], name: &str) -> &'a [String] {
    classes
        .iter()
        .find(|class| class.name == name)
        .map_or(&[][..], |class| class.members.as_slice())
}

fn class_relation_line(relation: &ClassRelation, width: usize) -> String {
    let connector = match (relation.dotted, &relation.label) {
        (true, Some(label)) => format!(" ··{}··▶ ", sanitize_mermaid_label(label)),
        (true, None) => " ····▶ ".to_owned(),
        (false, Some(label)) => format!(" ─{}─▶ ", sanitize_mermaid_label(label)),
        (false, None) => " ───▶ ".to_owned(),
    };
    fit_text(&format!("{}{}{}", relation.from, connector, relation.to), width)
}

/// Split an over-budget class diagram by class into bounded panels: each
/// class unit carries the rendered cost of its outgoing relations, and
/// relations whose endpoints land in different panels become stub notes,
/// mirroring `split_flowchart_chunks`.
fn split_class_chunks(
    parsed: &ParsedClassDiagram,
    width: usize,
    limit: usize,
) -> Vec<Vec<String>> {
    let header = fit_text("classDiagram", width);
    let mut costs = Vec::with_capacity(parsed.diagram.nodes.len());
    for node in &parsed.diagram.nodes {
        let mut scratch = Vec::new();
        append_class(
            &mut scratch,
            &node.id,
            class_members(&parsed.classes, &node.id),
            width,
        );
        let mut cost = rendered_cells(&scratch);
        cost = cost.saturating_add(
            parsed
                .relations
                .iter()
                .filter(|relation| relation.from == node.id)
                .map(|relation| display_width(&class_relation_line(relation, width)))
                .sum::<usize>(),
        );
        costs.push(cost);
    }
    if let Some(first) = costs.first_mut() {
        *first = first.saturating_add(display_width(&header));
    }
    let chunk_of_class = greedy_chunk_of(&costs, limit);
    let chunk_count = chunk_of_class.last().copied().unwrap_or(0).saturating_add(1);
    let class_index: HashMap<&str, usize> = parsed
        .diagram
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();
    let last_chunk = chunk_count.saturating_sub(1);
    let from_chunk = |relation: &ClassRelation| -> usize {
        class_index
            .get(relation.from.as_str())
            .and_then(|index| chunk_of_class.get(*index).copied())
            .unwrap_or(last_chunk)
    };
    let to_chunk = |relation: &ClassRelation| -> Option<usize> {
        class_index
            .get(relation.to.as_str())
            .and_then(|index| chunk_of_class.get(*index).copied())
    };

    let mut chunks = Vec::with_capacity(chunk_count);
    for chunk in 0..chunk_count {
        let mut lines = Vec::new();
        if chunk == 0 {
            lines.push(header.clone());
        }
        for (index, node) in parsed.diagram.nodes.iter().enumerate() {
            if chunk_of_class[index] != chunk {
                continue;
            }
            append_class(
                &mut lines,
                &node.id,
                class_members(&parsed.classes, &node.id),
                width,
            );
        }
        let mut relation_lines: Vec<String> = Vec::new();
        for relation in &parsed.relations {
            let source_chunk = from_chunk(relation);
            let target_chunk = to_chunk(relation).unwrap_or(source_chunk);
            if source_chunk == chunk && target_chunk == chunk {
                relation_lines.push(class_relation_line(relation, width));
            } else if source_chunk == chunk {
                relation_lines.push(fit_text(&format!("{} → …", relation.from), width));
            } else if target_chunk == chunk {
                relation_lines.push(fit_text(&format!("… → {}", relation.to), width));
            }
        }
        if !relation_lines.is_empty() {
            lines.push(fit_text("edges", width));
            lines.extend(relation_lines);
        }
        chunks.push(lines);
    }
    chunks
}

fn append_class(output: &mut Vec<String>, name: &str, members: &[String], width: usize) {
    if width < 5 {
        output.push(fit_text(name, width));
        return;
    }
    let inner_width = std::iter::once(name)
        .chain(members.iter().map(String::as_str))
        .map(display_width)
        .max()
        .unwrap_or(1)
        .min(width - 2)
        .max(1);
    output.push(format!("┌{}┐", "─".repeat(inner_width)));
    output.push(format!("│{}│", pad_to_width(&fit_text(name, inner_width), inner_width)));
    if !members.is_empty() {
        output.push(format!("├{}┤", "─".repeat(inner_width)));
        for member in members {
            output.push(format!(
                "│{}│",
                pad_to_width(&fit_text(member, inner_width), inner_width)
            ));
        }
    }
    output.push(format!("└{}┘", "─".repeat(inner_width)));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceArrow {
    /// `->>`: solid line, closed arrowhead.
    SolidClosed,
    /// `-->>`: dotted line, closed arrowhead.
    DottedClosed,
    /// `-)`: solid line, open arrowhead.
    SolidOpen,
    /// `-->`: dotted line, no arrowhead.
    DottedOpen,
    /// `-x`: solid line, cross end.
    SolidCross,
}

const SEQUENCE_ARROWS: &[(&str, SequenceArrow)] = &[
    ("->>", SequenceArrow::SolidClosed),
    ("-->>", SequenceArrow::DottedClosed),
    ("-)", SequenceArrow::SolidOpen),
    ("-->", SequenceArrow::DottedOpen),
    ("-x", SequenceArrow::SolidCross),
    ("-X", SequenceArrow::SolidCross),
    ("--x", SequenceArrow::SolidCross),
    ("--X", SequenceArrow::SolidCross),
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct SequenceParticipant {
    id: String,
    label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SequenceMessage {
    from: String,
    to: String,
    arrow: SequenceArrow,
    label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SequenceRow {
    Message(SequenceMessage),
    /// Sanitized body after the `note ` keyword, e.g. `over A, B: text`.
    Note(String),
    /// Unknown statement kept as a visible plain-text row.
    Plain(String),
}

#[derive(Clone, Debug)]
struct ParsedSequence {
    participants: Vec<SequenceParticipant>,
    rows: Vec<SequenceRow>,
}

fn is_sequence_header(header: &str) -> bool {
    header.eq_ignore_ascii_case("sequenceDiagram") || header.eq_ignore_ascii_case("sequence")
}

fn parse_sequence_diagram(
    source: &str,
    limits: MermaidLimits,
) -> Result<ParsedSequence, MermaidDiagnostic> {
    let mut participants = Vec::new();
    let mut rows = Vec::new();
    let mut message_count = 0usize;
    let mut saw_header = false;

    for (index, raw_line) in source.lines().enumerate() {
        let line = index + 1;
        let statement = raw_line
            .split_once("%%")
            .map_or(raw_line, |(before, _)| before)
            .trim();
        if statement.is_empty() {
            continue;
        }
        if !saw_header {
            if !is_sequence_header(statement) {
                return Err(diagnostic(
                    MermaidDiagnosticKind::UnsupportedDiagram,
                    "Only flowchart/graph, classDiagram, and sequenceDiagram diagrams are supported",
                    Some(line),
                ));
            }
            saw_header = true;
            continue;
        }
        parse_sequence_statement(
            statement,
            line,
            &mut participants,
            &mut rows,
            &mut message_count,
            limits,
        )?;
    }
    if participants.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Sequence diagram contains no participants",
            Some(1),
        ));
    }
    Ok(ParsedSequence { participants, rows })
}

fn parse_sequence_statement(
    statement: &str,
    line: usize,
    participants: &mut Vec<SequenceParticipant>,
    rows: &mut Vec<SequenceRow>,
    message_count: &mut usize,
    limits: MermaidLimits,
) -> Result<(), MermaidDiagnostic> {
    if let Some(rest) = keyword_tail(statement, "participant")
        .or_else(|| keyword_tail(statement, "actor"))
    {
        let rest = rest.trim();
        let (id, alias) = rest
            .split_once(" as ")
            .map_or((rest, None), |(id, alias)| (id, Some(alias)));
        let id = id.trim();
        let label = match alias {
            Some(alias) => {
                let label = alias.trim().trim_matches(['"', '\'']).trim();
                if label.is_empty() {
                    None
                } else {
                    Some(sanitize_mermaid_label(label))
                }
            }
            None => Some(id.to_owned()),
        };
        let Some(label) = label else {
            rows.push(SequenceRow::Plain(sanitize_mermaid_label(statement)));
            return Ok(());
        };
        if !valid_identifier(id) {
            rows.push(SequenceRow::Plain(sanitize_mermaid_label(statement)));
            return Ok(());
        }
        upsert_sequence_participant(participants, id.to_owned(), label, limits, line)?;
        return Ok(());
    }
    if keyword_tail(statement, "autonumber").is_some() {
        return Ok(());
    }
    if let Some(body) = keyword_tail(statement, "note") {
        let body = sanitize_mermaid_label(body.trim());
        if body.is_empty() {
            rows.push(SequenceRow::Plain(sanitize_mermaid_label(statement)));
        } else {
            rows.push(SequenceRow::Note(body));
        }
        return Ok(());
    }
    if let Some(message) = parse_sequence_message(statement) {
        upsert_sequence_participant(
            participants,
            message.from.clone(),
            message.from.clone(),
            limits,
            line,
        )?;
        upsert_sequence_participant(
            participants,
            message.to.clone(),
            message.to.clone(),
            limits,
            line,
        )?;
        if *message_count >= limits.max_edges {
            return Err(diagnostic(
                MermaidDiagnosticKind::OversizeGraph,
                format!("Mermaid message limit of {} exceeded", limits.max_edges),
                Some(line),
            ));
        }
        *message_count += 1;
        rows.push(SequenceRow::Message(message));
        return Ok(());
    }
    rows.push(SequenceRow::Plain(sanitize_mermaid_label(statement)));
    Ok(())
}

fn upsert_sequence_participant(
    participants: &mut Vec<SequenceParticipant>,
    id: String,
    label: String,
    limits: MermaidLimits,
    line: usize,
) -> Result<(), MermaidDiagnostic> {
    if participants.iter().any(|participant| participant.id == id) {
        return Ok(());
    }
    if participants.len() >= limits.max_nodes {
        return Err(diagnostic(
            MermaidDiagnosticKind::OversizeGraph,
            format!(
                "Mermaid participant limit of {} exceeded",
                limits.max_nodes
            ),
            Some(line),
        ));
    }
    participants.push(SequenceParticipant { id, label });
    Ok(())
}

fn keyword_tail<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let len = keyword.len();
    // `len` may fall inside a multibyte char (e.g. a CJK statement like
    // `loop 每个模型回合` checked against the 10-byte `autonumber`); slicing
    // at a non-char-boundary would panic, so bail out instead.
    if statement.len() < len
        || !statement.is_char_boundary(len)
        || !statement[..len].eq_ignore_ascii_case(keyword)
    {
        return None;
    }
    if statement.len() > len
        && !statement[len..].chars().next().is_some_and(char::is_whitespace)
    {
        return None;
    }
    Some(&statement[len..])
}

fn parse_sequence_message(statement: &str) -> Option<SequenceMessage> {
    let mut best: Option<(usize, usize, SequenceArrow)> = None;
    for (token, arrow) in SEQUENCE_ARROWS {
        let Some(index) = statement.find(token) else {
            continue;
        };
        let better = match best {
            None => true,
            Some((best_index, best_len, _)) => {
                index < best_index || (index == best_index && token.len() > best_len)
            }
        };
        if better {
            best = Some((index, token.len(), *arrow));
        }
    }
    let (index, len, arrow) = best?;
    let from = statement[..index].trim();
    let remainder = &statement[index + len..];
    let (to, label) = remainder
        .split_once(':')
        .map_or((remainder, None), |(to, label)| (to, Some(label.trim())));
    let to = to.trim();
    if !valid_identifier(from) || !valid_identifier(to) {
        return None;
    }
    let label = label
        .filter(|text| !text.is_empty())
        .map(sanitize_mermaid_label);
    Some(SequenceMessage {
        from: from.to_owned(),
        to: to.to_owned(),
        arrow,
        label,
    })
}

fn sequence_to_flowchart(parsed: &ParsedSequence) -> MermaidFlowchart {
    MermaidFlowchart {
        direction: FlowDirection::LeftRight,
        nodes: parsed
            .participants
            .iter()
            .map(|participant| MermaidNode {
                id: participant.id.clone(),
                label: participant.label.clone(),
                shape: MermaidNodeShape::Plain,
            })
            .collect(),
        edges: parsed
            .rows
            .iter()
            .filter_map(|row| match row {
                SequenceRow::Message(message) => Some(MermaidEdge {
                    from: message.from.clone(),
                    to: message.to.clone(),
                    label: message.label.clone(),
                    arrow: true,
                }),
                SequenceRow::Note(_) | SequenceRow::Plain(_) => None,
            })
            .collect(),
    }
}

/// Sequence diagrams render as a single aligned panel and are NOT split by
/// participant: every message row is padded against the widest participant
/// label, so rows cannot be re-flowed into independent per-participant panels
/// without losing that alignment (participant splits would also strand notes
/// and arrows that span arbitrary participants). Over-budget sequence diagrams
/// therefore keep the `OutputLimit` diagnostic, unlike flowcharts and class
/// diagrams which split into bounded panels.
fn render_sequence_diagram(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let parsed = parse_sequence_diagram(source, limits)?;
    let minimum_lines = 3usize.saturating_add(parsed.rows.len());
    if minimum_lines > limits.max_output_cells {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Mermaid output cell limit is too small for this sequence diagram",
            None,
        ));
    }
    let header = parsed
        .participants
        .iter()
        .map(|participant| sanitize_mermaid_label(&participant.label))
        .collect::<Vec<_>>()
        .join(" ── ");
    let header = fit_text(&header, width);
    let rule_width = display_width(&header);
    let mut lines = vec![
        fit_text("sequenceDiagram", width),
        header,
        "─".repeat(rule_width),
    ];
    let max_from = parsed
        .participants
        .iter()
        .map(|participant| display_width(&participant.label))
        .max()
        .unwrap_or(0);
    for row in &parsed.rows {
        match row {
            SequenceRow::Message(message) => {
                let from_label = parsed
                    .participants
                    .iter()
                    .find(|participant| participant.id == message.from)
                    .map_or(&message.from, |participant| &participant.label);
                let to_label = parsed
                    .participants
                    .iter()
                    .find(|participant| participant.id == message.to)
                    .map_or(&message.to, |participant| &participant.label);
                let label_part = message
                    .label
                    .as_ref()
                    .map_or(String::new(), |label| format!(" : {label}"));
                lines.push(fit_text(
                    &format!(
                        "│ {} │{}│ {}{} │",
                        pad_to_width(from_label, max_from),
                        sequence_arrow_text(message.arrow),
                        to_label,
                        label_part
                    ),
                    width,
                ));
            }
            SequenceRow::Note(body) => {
                lines.push(fit_text(&format!("│ [note {body}] │"), width));
            }
            SequenceRow::Plain(text) => {
                lines.push(fit_text(&format!("│ {text} │"), width));
            }
        }
    }
    check_output_cells(&lines, limits)?;
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram: sequence_to_flowchart(&parsed),
        kind: MermaidDiagramKind::Sequence,
    })
}

fn sequence_arrow_text(arrow: SequenceArrow) -> &'static str {
    match arrow {
        SequenceArrow::SolidClosed => "──▶",
        SequenceArrow::DottedClosed => "···▶",
        SequenceArrow::SolidOpen => "──○",
        SequenceArrow::DottedOpen => "···",
        SequenceArrow::SolidCross => "──✕",
    }
}

fn sanitize_mermaid_label(text: &str) -> String {
    sanitize_inline(text)
        .replace("<br/>", " · ")
        .replace("<br />", " · ")
        .replace("<br>", " · ")
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
}

/// Total rendered cell count of `lines` — the metric `check_output_cells` and
/// the per-chunk split budgets operate on.
fn rendered_cells(lines: &[String]) -> usize {
    lines.iter().map(|line| display_width(line)).sum::<usize>()
}

fn check_output_cells(lines: &[String], limits: MermaidLimits) -> Result<(), MermaidDiagnostic> {
    let cells = rendered_cells(lines);
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
    Ok(())
}

fn invalid_class_line(line: usize, statement: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!("Unsupported classDiagram syntax: {}", fit_text(statement, 80)),
        Some(line),
    )
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
    let mut from = cursor
        .node_group()
        .ok_or_else(|| invalid_line(line, statement))?;
    for node in &from {
        upsert_node(node.clone(), nodes, node_indices, limits, line)?;
    }
    cursor.whitespace();
    if cursor.done() {
        return Ok(());
    }

    while !cursor.done() {
        let (arrow, label) = cursor.edge().ok_or_else(|| invalid_line(line, statement))?;
        let targets = cursor
            .node_group()
            .ok_or_else(|| invalid_line(line, statement))?;
        for node in &targets {
            upsert_node(node.clone(), nodes, node_indices, limits, line)?;
        }
        // `A & B --> C & D` expands to the cartesian product A->C, A->D,
        // B->C, B->D; reject the whole statement when the expansion would
        // exceed the edge limit.
        let edge_count = from.len().saturating_mul(targets.len());
        if edges.len().saturating_add(edge_count) > limits.max_edges {
            return Err(diagnostic(
                MermaidDiagnosticKind::OversizeGraph,
                format!("Mermaid edge limit of {} exceeded", limits.max_edges),
                Some(line),
            ));
        }
        for source in &from {
            for target in &targets {
                edges.push(MermaidEdge {
                    from: source.id.clone(),
                    to: target.id.clone(),
                    label: label.clone(),
                    arrow,
                });
            }
        }
        from = targets;
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

    /// Parses a `&`-joined list of one or more nodes, matching upstream
    /// mermaid multi-node statements such as `A & B --> C & D`. Returns
    /// `None` when an ampersand is not followed by a node (stray `&`, e.g.
    /// `A & --> C` or `A --> B &`).
    fn node_group(&mut self) -> Option<Vec<MermaidNode>> {
        let mut group = vec![self.node()?];
        loop {
            self.whitespace();
            if !self.rest.starts_with('&') {
                return Some(group);
            }
            self.rest = &self.rest[1..];
            group.push(self.node()?);
        }
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


/// Exact user-reported shape: subgraphs CWD/TR/RM/AD/PJ/SNAP/SESS/EXT/ORCH/
/// SBX with boxed nodes and labeled edges, sized like a real architecture
/// diagram (long descriptive labels, edges crossing subgraph boundaries) so
/// the full render crosses the former 4x total output ceiling at typical
/// widths. Shared by the mermaid.rs and render.rs regression suites so the
/// card-level fallback behavior is tested against the same input.
#[cfg(test)]
pub(crate) fn reported_subgraph_flowchart_source() -> String {
    let mut source = String::from("flowchart LR\n");
    let groups = [
        ("CWD", "current working directory resolution and trust checks"),
        ("TR", "transcript compaction with rewind checkpoints"),
        ("RM", "runtime state machine and mode transitions"),
        ("AD", "agent directory with skills and memory index"),
        ("PJ", "project-scoped settings and .pi overlay"),
        ("SNAP", "snapshot-based recovery and rollback"),
        ("SESS", "session store with branch fork and resume"),
        ("EXT", "extension host quickjs event matrix"),
        ("ORCH", "orchestration jobs mailbox and todo dag"),
        ("SBX", "sandbox isolation with deny rules"),
    ];
    let mut prev: Option<&str> = None;
    for (id, title) in groups {
        source.push_str(&format!("subgraph {id}[\"{title}\"]\n"));
        for n in 1..=5 {
            source.push_str(&format!(
                "{id}{n}[{title} component {n} with state and event handling]\n"
            ));
            if n > 1 {
                source.push_str(&format!("{id}{} -->|data flow| {id}{n}\n", n - 1));
            }
        }
        source.push_str("end\n");
        if let Some(prev_id) = prev {
            source.push_str(&format!("{prev_id}5 -->|handoff| {id}1\n"));
        }
        prev = Some(id);
    }
    for (i, (id, _)) in groups.iter().enumerate() {
        for (j, (other, _)) in groups.iter().enumerate() {
            if i != j && (i + j) % 3 == 0 {
                source.push_str(&format!("{id}5 -->|ref {i}:{j}| {other}1\n"));
            }
        }
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact user-reported classDiagram: members + solid/dotted relations.
    const CLASS_SOURCE: &str = "\
classDiagram
class Application {
+run()
}
class Session {
+id: String
}
class Agent {
+tools: Vec
}
class AgentTool {
+name: String
}
Application --> Session
Agent ..> AgentTool : via context
";

    /// Exact user-reported flowchart LR with labeled subgraph.
    const FLOW_SOURCE: &str = "\
flowchart LR
subgraph records[\"SessionRecord types\"]
A[Session] --> B[Message]
B --> C[ToolCall]
end
X[User] --> A
";

    #[test]
    fn class_diagram_parses_members_and_relations() {
        let parsed = parse_class_diagram(CLASS_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(
            parsed.classes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Application", "Session", "Agent", "AgentTool"]
        );
        assert_eq!(parsed.classes[0].members, vec!["+run()".to_owned()]);
        assert_eq!(parsed.classes[1].members, vec!["+id: String".to_owned()]);
        assert_eq!(parsed.classes[2].members, vec!["+tools: Vec".to_owned()]);
        assert_eq!(parsed.classes[3].members, vec!["+name: String".to_owned()]);
        assert_eq!(parsed.relations.len(), 2);
        assert_eq!(parsed.relations[0].from, "Application");
        assert_eq!(parsed.relations[0].to, "Session");
        assert!(!parsed.relations[0].dotted);
        assert_eq!(parsed.relations[0].label, None);
        assert_eq!(parsed.relations[1].from, "Agent");
        assert_eq!(parsed.relations[1].to, "AgentTool");
        assert!(parsed.relations[1].dotted);
        assert_eq!(parsed.relations[1].label.as_deref(), Some("via context"));
        let chart = parse_mermaid(CLASS_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(chart.nodes.len(), 4);
        assert_eq!(chart.edges.len(), 2);
    }

    #[test]
    fn class_diagram_render_includes_members_and_relation_edges() {
        let art = render_mermaid_unicode(CLASS_SOURCE, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::ClassDiagram);
        let text = art.chunks[0].join("\n");
        assert!(text.starts_with("classDiagram"), "{text}");
        assert!(text.contains("+run()"), "{text}");
        assert!(text.contains("+id: String"), "{text}");
        assert!(text.contains("+tools: Vec"), "{text}");
        assert!(text.contains("+name: String"), "{text}");
        assert!(text.contains("Application ───▶ Session"), "{text}");
        assert!(text.contains("Agent ··via context··▶ AgentTool"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
    }

    #[test]
    fn labeled_subgraph_parses_id_title_and_nested_edges() {
        let statements = statements(FLOW_SOURCE);
        let header_line = statements[0].0;
        let parsed = parse_flowchart(statements, header_line, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.diagram.direction, FlowDirection::LeftRight);
        assert_eq!(parsed.subgraphs.len(), 1);
        assert_eq!(parsed.subgraphs[0].id, "records");
        assert_eq!(parsed.subgraphs[0].title, "SessionRecord types");
        assert_eq!(parsed.subgraphs[0].start_node, 0);
        assert_eq!(parsed.subgraphs[0].end_node, 3);
        assert_eq!(
            parsed
                .diagram
                .nodes
                .iter()
                .map(|n| (n.id.as_str(), n.label.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("A", "Session"),
                ("B", "Message"),
                ("C", "ToolCall"),
                ("X", "User"),
            ]
        );
        assert_eq!(parsed.diagram.edges.len(), 3);
        assert_eq!(parsed.diagram.edges[0].from, "A");
        assert_eq!(parsed.diagram.edges[0].to, "B");
        assert_eq!(parsed.diagram.edges[2].from, "X");
        assert_eq!(parsed.diagram.edges[2].to, "A");
    }

    #[test]
    fn labeled_subgraph_render_keeps_title_and_end_markers() {
        let art = render_mermaid_unicode(FLOW_SOURCE, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        let text = art.chunks[0].join("\n");
        assert!(text.starts_with("flowchart LR"), "{text}");
        assert!(
            text.contains("subgraph records · SessionRecord types"),
            "{text}"
        );
        assert!(text.contains("end subgraph records"), "{text}");
        assert!(text.contains("A · Session"), "{text}");
        assert!(text.contains("B · Message"), "{text}");
        assert!(text.contains("C · ToolCall"), "{text}");
        assert!(text.contains("X · User"), "{text}");
        assert!(text.contains("A ───▶ B"), "{text}");
        assert!(text.contains("B ───▶ C"), "{text}");
        assert!(text.contains("X ───▶ A"), "{text}");
    }

    /// Multi-node (`&`-joined) flowchart statements expand to the cartesian
    /// product of sources and targets, matching upstream mermaid.
    #[test]
    fn flowchart_multi_node_source_side_expands() {
        let chart = parse_mermaid("flowchart LR\nA & B --> C", MermaidLimits::default()).unwrap();
        assert_eq!(
            chart
                .nodes
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        assert_eq!(chart.edges.len(), 2);
        assert_eq!(
            (chart.edges[0].from.as_str(), chart.edges[0].to.as_str()),
            ("A", "C")
        );
        assert_eq!(
            (chart.edges[1].from.as_str(), chart.edges[1].to.as_str()),
            ("B", "C")
        );
    }

    #[test]
    fn flowchart_multi_node_target_side_expands() {
        let chart = parse_mermaid("flowchart LR\nA --> C & D", MermaidLimits::default()).unwrap();
        assert_eq!(chart.edges.len(), 2);
        assert_eq!(
            (chart.edges[0].from.as_str(), chart.edges[0].to.as_str()),
            ("A", "C")
        );
        assert_eq!(
            (chart.edges[1].from.as_str(), chart.edges[1].to.as_str()),
            ("A", "D")
        );
    }

    #[test]
    fn flowchart_multi_node_both_sides_is_cartesian_product() {
        let chart =
            parse_mermaid("flowchart LR\nA & B --> C & D", MermaidLimits::default()).unwrap();
        assert_eq!(chart.nodes.len(), 4);
        assert_eq!(chart.edges.len(), 4);
        let pairs: Vec<_> = chart
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(pairs, vec![("A", "C"), ("A", "D"), ("B", "C"), ("B", "D")]);
    }

    #[test]
    fn flowchart_multi_node_labels_and_shapes_parse() {
        let chart = parse_mermaid(
            "flowchart LR\nA[\"x\"] & B[\"y\"] --> C",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(
            chart
                .nodes
                .iter()
                .map(|n| (n.id.as_str(), n.label.as_str(), n.shape))
                .collect::<Vec<_>>(),
            vec![
                ("A", "x", MermaidNodeShape::Rectangle),
                ("B", "y", MermaidNodeShape::Rectangle),
                ("C", "C", MermaidNodeShape::Plain),
            ]
        );
        assert_eq!(chart.edges.len(), 2);
        assert_eq!(
            (chart.edges[0].from.as_str(), chart.edges[0].to.as_str()),
            ("A", "C")
        );
        assert_eq!(
            (chart.edges[1].from.as_str(), chart.edges[1].to.as_str()),
            ("B", "C")
        );
    }

    #[test]
    fn flowchart_multi_node_chained_groups_expand_per_segment() {
        let chart = parse_mermaid(
            "flowchart LR\nA & B --> C & D --> E",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(chart.nodes.len(), 5);
        let pairs: Vec<_> = chart
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("A", "C"),
                ("A", "D"),
                ("B", "C"),
                ("B", "D"),
                ("C", "E"),
                ("D", "E"),
            ]
        );
    }

    #[test]
    fn flowchart_multi_node_user_reported_statements_parse() {
        let chart = parse_mermaid(
            "flowchart LR\nENV & HOME & PROJ & CLI --> AUTH & SET & CAT & RES",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(chart.nodes.len(), 8);
        assert_eq!(chart.edges.len(), 16);
        let chart = parse_mermaid(
            "flowchart LR\nTOOLS --> BASH & EDIT & FS & WEB & IMG & ASK & PROC",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(chart.nodes.len(), 8);
        assert_eq!(chart.edges.len(), 7);
    }

    #[test]
    fn flowchart_multi_node_expansion_enforces_limits() {
        // An expansion that would exceed max_edges is rejected.
        let err = parse_mermaid(
            "flowchart LR\nA & B --> C & D",
            MermaidLimits {
                max_edges: 3,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("edge limit"), "{}", err.message);

        // An expansion exactly at the limit is accepted.
        let chart = parse_mermaid(
            "flowchart LR\nA & B --> C & D",
            MermaidLimits {
                max_edges: 4,
                ..MermaidLimits::default()
            },
        )
        .unwrap();
        assert_eq!(chart.edges.len(), 4);

        // Existing single-edge limit behavior is unchanged.
        let err = parse_mermaid(
            "flowchart LR\nA --> B\nC --> D",
            MermaidLimits {
                max_edges: 1,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("edge limit"), "{}", err.message);

        // A `&`-joined group still counts against max_nodes.
        let err = parse_mermaid(
            "flowchart LR\nA & B & C & D --> E",
            MermaidLimits {
                max_nodes: 3,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("node limit"), "{}", err.message);
    }

    #[test]
    fn flowchart_multi_node_stray_ampersand_errors() {
        for source in [
            "flowchart LR\nA & --> C",
            "flowchart LR\nA --> B &",
            "flowchart LR\nA --> B & & C",
            "flowchart LR\n& A --> C",
        ] {
            let err = parse_mermaid(source, MermaidLimits::default()).unwrap_err();
            assert_eq!(err.kind, MermaidDiagnosticKind::InvalidSyntax, "{source}");
        }
    }

    #[test]
    fn flowchart_multi_node_plain_declaration_declares_all_nodes() {
        let chart = parse_mermaid("flowchart LR\nA & B", MermaidLimits::default()).unwrap();
        assert_eq!(chart.edges.len(), 0);
        assert_eq!(
            chart
                .nodes
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
    }

    #[test]
    fn flowchart_multi_node_renders_expanded_edges() {
        let art = render_mermaid_unicode(
            "flowchart LR\nA & B --> C & D",
            48,
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        let text = art.chunks[0].join("\n");
        for edge in ["A ───▶ C", "A ───▶ D", "B ───▶ C", "B ───▶ D"] {
            assert!(text.contains(edge), "{edge}: {text}");
        }
    }

    #[test]
    fn small_flowchart_stays_a_single_chunk() {
        let art = render_mermaid_unicode("flowchart LR\nA --> B", 48, MermaidLimits::default())
            .unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        assert_eq!(art.chunks.len(), 1, "small diagrams must not split");
        assert_eq!(art.chunks[0][0], "flowchart LR");
        assert!(art.chunks[0].iter().any(|line| line == "A ───▶ B"));
    }

    #[test]
    fn over_budget_flowchart_splits_into_bounded_chunks_with_stub_edges() {
        // A 112-node chain renders to ~5.1k cells: over the 3_200 per-chunk
        // budget but under the 4x total ceiling, so it must split into two
        // panels with stub notes on the single edge crossing the boundary
        // instead of erroring with OutputLimit.
        let mut source = String::from("flowchart TD\n");
        for i in 1..112 {
            source.push_str(&format!("A{i} --> A{}\n", i + 1));
        }
        let limits = MermaidLimits {
            max_nodes: 256,
            max_edges: 256,
            ..MermaidLimits::default()
        };
        let art = render_mermaid_unicode(&source, 48, limits).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        assert!(
            art.chunks.len() >= 2,
            "over-budget graph must split, got {} chunk(s)",
            art.chunks.len()
        );
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(
                cells <= limits.max_output_cells,
                "chunk {index} exceeds the per-chunk budget: {cells} cells"
            );
        }
        // The diagram header lives in the first panel only.
        assert!(art.chunks[0].first().is_some_and(|line| line.starts_with("flowchart ")));
        assert!(art.chunks[1..]
            .iter()
            .all(|chunk| chunk.first().is_some_and(|line| !line.starts_with("flowchart "))));
        // Every node appears in exactly one panel; every edge is either full
        // (same panel) or a stub note across the boundary.
        let text = art.chunks.concat().join("\n");
        for i in 1..=112 {
            assert!(text.contains(&format!("A{i}")), "node A{i} missing: {text}");
        }
        assert!(text.contains("A1 ───▶ A2"), "in-chunk edge must stay full: {text}");
        // Exactly one edge crosses the boundary: the source panel carries its
        // `A{n} → …` stub, the target panel the matching `… → A{n+1}` stub.
        let source_stubs = art.chunks[0]
            .iter()
            .filter(|line| line.ends_with(" → …"))
            .collect::<Vec<_>>();
        let target_stubs = art.chunks[1]
            .iter()
            .filter(|line| line.starts_with("… → "))
            .collect::<Vec<_>>();
        assert_eq!(source_stubs.len(), 1, "{text}");
        assert_eq!(target_stubs.len(), 1, "{text}");
        let from_id = source_stubs[0].trim_end_matches(" → …").trim_start_matches('A');
        let to_id = target_stubs[0].trim_start_matches("… → ").trim_start_matches('A');
        assert_eq!(
            to_id.parse::<u32>().unwrap(),
            from_id.parse::<u32>().unwrap() + 1,
            "stubs must describe the same crossing edge: {text}"
        );
    }

    #[test]
    fn reported_subgraph_flowchart_renders_or_splits_across_widths() {
        // Regression: this shape used to return OutputLimit at widths >= 80
        // (the rendered cells crossed the 4x total ceiling), which made the
        // card fall back to the raw source. It must render as a diagram —
        // single or split into bounded panels — at every width, never
        // erroring and never losing a node.
        let source = reported_subgraph_flowchart_source();
        for width in [24usize, 32, 48, 60, 80, 100, 120, 160, 200, 240, 300] {
            let art = render_mermaid_unicode(&source, width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must not fail: {err:?}"));
            assert_eq!(art.kind, MermaidDiagramKind::Flowchart, "width {width}");
            assert!(!art.chunks.is_empty(), "width {width}");
            for (index, chunk) in art.chunks.iter().enumerate() {
                let cells = rendered_cells(chunk);
                assert!(
                    cells <= MermaidLimits::default().max_output_cells,
                    "width {width}: chunk {index} exceeds the per-chunk budget: {cells} cells"
                );
            }
            let text = art.chunks.concat().join("\n");
            for (id, _) in [
                ("CWD", 0), ("TR", 0), ("RM", 0), ("AD", 0), ("PJ", 0),
                ("SNAP", 0), ("SESS", 0), ("EXT", 0), ("ORCH", 0), ("SBX", 0),
            ] {
                assert!(
                    text.contains(&format!("subgraph {id} ·")),
                    "width {width}: subgraph {id} header missing"
                );
                for n in 1..=5 {
                    assert!(
                        text.contains(&format!("{id}{n}")),
                        "width {width}: node {id}{n} missing"
                    );
                }
            }
        }
    }

    #[test]
    fn flowchart_beyond_any_total_ceiling_still_splits_into_bounded_chunks() {
        // A 500-node chain renders to ~14.5k cells: far past the former 4x
        // total ceiling (12_800). Size must never flip a supported flowchart
        // to the source fallback, so it splits into bounded panels instead of
        // erroring with OutputLimit.
        let mut source = String::from("flowchart TD\n");
        for i in 1..500 {
            source.push_str(&format!("A{i} --> A{}\n", i + 1));
        }
        let limits = MermaidLimits {
            max_nodes: 1024,
            max_edges: 1024,
            ..MermaidLimits::default()
        };
        let art = render_mermaid_unicode(&source, 48, limits).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        assert!(
            art.chunks.len() >= 5,
            "a ~14.5k-cell graph must split into many panels, got {}",
            art.chunks.len()
        );
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(
                cells <= limits.max_output_cells,
                "chunk {index} exceeds the per-chunk budget: {cells} cells"
            );
        }
        let text = art.chunks.concat().join("\n");
        for i in 1..=500 {
            assert!(text.contains(&format!("A{i}")), "node A{i} missing: {text}");
        }
    }

    #[test]
    fn over_budget_class_diagram_splits_into_bounded_chunks() {
        // 190 chained classes render to ~7.6k cells: over the per-chunk budget
        // but under the total ceiling, so the class diagram must split with
        // stub notes on relations crossing panel boundaries.
        let mut source = String::from("classDiagram\n");
        for i in 1..=190 {
            source.push_str(&format!("class C{i} {{\n+m\n}}\n"));
        }
        for i in 1..190 {
            source.push_str(&format!("C{i} --> C{}\n", i + 1));
        }
        let limits = MermaidLimits {
            max_nodes: 256,
            max_edges: 256,
            ..MermaidLimits::default()
        };
        let art = render_mermaid_unicode(&source, 48, limits).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::ClassDiagram);
        assert!(
            art.chunks.len() >= 2,
            "over-budget class diagram must split, got {} chunk(s)",
            art.chunks.len()
        );
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(
                cells <= limits.max_output_cells,
                "chunk {index} exceeds the per-chunk budget: {cells} cells"
            );
        }
        assert!(art.chunks[0].first().is_some_and(|line| line.starts_with("classDiagram")));
        let text = art.chunks.concat().join("\n");
        for i in 1..=190 {
            assert!(text.contains(&format!("C{i}")), "class C{i} missing: {text}");
        }
        assert!(text.contains("──▶"), "in-chunk relations must stay full: {text}");
        assert!(
            text.contains("→ …") && text.contains("… →"),
            "cross-chunk relation stubs missing: {text}"
        );
    }

    #[test]
    fn class_diagram_beyond_any_total_ceiling_still_splits_into_bounded_chunks() {
        // 400 chained classes render to ~15k cells: past the former 4x total
        // ceiling, so this used to return OutputLimit and fall back to the
        // raw source. It must split into bounded panels instead.
        let mut source = String::from("classDiagram\n");
        for i in 1..=400 {
            source.push_str(&format!("class C{i} {{\n+m\n}}\n"));
        }
        for i in 1..400 {
            source.push_str(&format!("C{i} --> C{}\n", i + 1));
        }
        let limits = MermaidLimits {
            max_nodes: 1024,
            max_edges: 1024,
            ..MermaidLimits::default()
        };
        let art = render_mermaid_unicode(&source, 48, limits).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::ClassDiagram);
        assert!(
            art.chunks.len() >= 5,
            "a ~15k-cell class diagram must split into many panels, got {}",
            art.chunks.len()
        );
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(
                cells <= limits.max_output_cells,
                "chunk {index} exceeds the per-chunk budget: {cells} cells"
            );
        }
        let text = art.chunks.concat().join("\n");
        for i in 1..=400 {
            assert!(text.contains(&format!("C{i}")), "class C{i} missing: {text}");
        }
        assert!(
            text.contains("→ …") && text.contains("… →"),
            "cross-chunk relation stubs missing: {text}"
        );
    }

    /// Full-featured sequence source: aliased participants, all five arrow
    /// forms, a note, an ignored autonumber, and an unknown-arrow line that
    /// must degrade to a plain row instead of failing the whole diagram.
    const SEQUENCE_SOURCE: &str = "\
sequenceDiagram
autonumber
participant A as Alice
actor B as Bob
A ->> B: hello
B -->> A: ack
A -) B: ping
B --> A: pong
A -x B: quit
note over A, B: working
A => B: hmm
";

    #[test]
    fn sequence_diagram_parses_participants_messages_notes_and_plain_rows() {
        let parsed = parse_sequence_diagram(SEQUENCE_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(
            parsed
                .participants
                .iter()
                .map(|p| (p.id.as_str(), p.label.as_str()))
                .collect::<Vec<_>>(),
            vec![("A", "Alice"), ("B", "Bob")]
        );
        let expected_rows: Vec<(&str, &str, SequenceArrow, Option<&str>)> = vec![
            ("A", "B", SequenceArrow::SolidClosed, Some("hello")),
            ("B", "A", SequenceArrow::DottedClosed, Some("ack")),
            ("A", "B", SequenceArrow::SolidOpen, Some("ping")),
            ("B", "A", SequenceArrow::DottedOpen, Some("pong")),
            ("A", "B", SequenceArrow::SolidCross, Some("quit")),
        ];
        let rows: Vec<_> = parsed.rows.iter().collect();
        assert_eq!(rows.len(), 7, "{rows:?}");
        for (index, (from, to, arrow, label)) in expected_rows.iter().enumerate() {
            let SequenceRow::Message(message) = &rows[index] else {
                panic!("row {index} is not a message: {:?}", rows[index]);
            };
            assert_eq!(&message.from, from, "row {index}");
            assert_eq!(&message.to, to, "row {index}");
            assert_eq!(message.arrow, *arrow, "row {index}");
            assert_eq!(message.label.as_deref(), *label, "row {index}");
        }
        assert_eq!(rows[5], &SequenceRow::Note("over A, B: working".to_owned()));
        assert_eq!(rows[6], &SequenceRow::Plain("A => B: hmm".to_owned()));
    }

    #[test]
    fn sequence_diagram_render_is_golden() {
        let art = render_mermaid_unicode(SEQUENCE_SOURCE, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Sequence);
        assert_eq!(
            art.chunks[0].join("\n"),
            "sequenceDiagram\n\
Alice ── Bob\n\
────────────\n\
│ Alice │──▶│ Bob : hello │\n\
│ Bob   │···▶│ Alice : ack │\n\
│ Alice │──○│ Bob : ping │\n\
│ Bob   │···│ Alice : pong │\n\
│ Alice │──✕│ Bob : quit │\n\
│ [note over A, B: working] │\n\
│ A => B: hmm │"
        );
        assert_eq!(art.diagram.nodes.len(), 2);
        assert_eq!(art.diagram.edges.len(), 5);
        assert_eq!(art.diagram.edges[0].label.as_deref(), Some("hello"));
    }

    #[test]
    fn sequence_diagram_auto_creates_undeclared_participants() {
        let parsed = parse_sequence_diagram(
            "sequenceDiagram\nX ->> Y: hi",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(
            parsed
                .participants
                .iter()
                .map(|p| (p.id.as_str(), p.label.as_str()))
                .collect::<Vec<_>>(),
            vec![("X", "X"), ("Y", "Y")]
        );
        let SequenceRow::Message(message) = &parsed.rows[0] else {
            panic!("expected message row: {:?}", parsed.rows[0]);
        };
        assert_eq!((message.from.as_str(), message.to.as_str()), ("X", "Y"));
    }

    #[test]
    fn sequence_diagram_user_reported_arrow_forms_render() {
        // The exact reported shape: participant declarations plus
        // `L->>T: execute tools` and `T-->>L: ToolResult`.
        let source = "sequenceDiagram\nparticipant L\nparticipant T\nL->>T: execute tools\nT-->>L: ToolResult";
        let parsed = parse_sequence_diagram(source, MermaidLimits::default()).unwrap();
        assert_eq!(
            parsed
                .participants
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["L", "T"]
        );
        let rows: Vec<_> = parsed.rows.iter().collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        for (index, arrow) in [SequenceArrow::SolidClosed, SequenceArrow::DottedClosed]
            .iter()
            .enumerate()
        {
            let SequenceRow::Message(message) = &rows[index] else {
                panic!("row {index} is not a message: {:?}", rows[index]);
            };
            assert_eq!(message.arrow, *arrow, "row {index}");
        }
        assert_eq!(rows[0], &SequenceRow::Message(SequenceMessage {
            from: "L".to_owned(),
            to: "T".to_owned(),
            arrow: SequenceArrow::SolidClosed,
            label: Some("execute tools".to_owned()),
        }));
        assert_eq!(rows[1], &SequenceRow::Message(SequenceMessage {
            from: "T".to_owned(),
            to: "L".to_owned(),
            arrow: SequenceArrow::DottedClosed,
            label: Some("ToolResult".to_owned()),
        }));
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        assert_eq!(
            art.chunks[0].join("\n"),
            "sequenceDiagram\n\
L ── T\n\
──────\n\
│ L │──▶│ T : execute tools │\n\
│ T │···▶│ L : ToolResult │"
        );
    }

    #[test]
    fn sequence_bare_header_and_aliases_are_accepted() {
        let chart = parse_mermaid(
            "sequence\nparticipant A as \"Alice\"\nA ->> B: hi",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(chart.nodes.len(), 2);
        assert_eq!(chart.nodes[0].label, "Alice");
        let art = render_mermaid_unicode(
            "sequence\nparticipant A as \"Alice\"\nA ->> B: hi",
            48,
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Sequence);
        assert_eq!(art.chunks[0][1], "Alice ── B");
    }

    #[test]
    fn sequence_compact_arrow_forms_parse() {
        let parsed = parse_sequence_diagram(
            "sequenceDiagram\nA->>B: sync\nB-->>A: reply\nC-)D: open\nE-->F: dotted\nG-xH: cross",
            MermaidLimits::default(),
        )
        .unwrap();
        let arrows: Vec<_> = parsed
            .rows
            .iter()
            .map(|row| match row {
                SequenceRow::Message(message) => Some(message.arrow),
                _ => None,
            })
            .collect();
        assert_eq!(
            arrows,
            vec![
                Some(SequenceArrow::SolidClosed),
                Some(SequenceArrow::DottedClosed),
                Some(SequenceArrow::SolidOpen),
                Some(SequenceArrow::DottedOpen),
                Some(SequenceArrow::SolidCross),
            ]
        );
    }

    #[test]
    fn sequence_diagram_parse_mermaid_maps_participants_and_messages() {
        let chart = parse_mermaid(SEQUENCE_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(
            chart
                .nodes
                .iter()
                .map(|n| (n.id.as_str(), n.label.as_str()))
                .collect::<Vec<_>>(),
            vec![("A", "Alice"), ("B", "Bob")]
        );
        assert_eq!(chart.edges.len(), 5);
        assert_eq!(chart.edges[0].from, "A");
        assert_eq!(chart.edges[0].to, "B");
        assert_eq!(chart.edges[0].label.as_deref(), Some("hello"));
    }

    #[test]
    fn sequence_diagram_limits_are_enforced() {
        let err = parse_sequence_diagram(
            "sequenceDiagram\nparticipant A\nparticipant B\nparticipant C",
            MermaidLimits {
                max_nodes: 2,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("participant limit"), "{}", err.message);

        let err = parse_sequence_diagram(
            "sequenceDiagram\nA ->> B: one\nA ->> B: two",
            MermaidLimits {
                max_edges: 1,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("message limit"), "{}", err.message);

        let err = render_mermaid_unicode(
            SEQUENCE_SOURCE,
            48,
            MermaidLimits {
                max_output_cells: 4,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OutputLimit);
    }

    #[test]
    fn sequence_multibyte_block_keywords_do_not_panic() {
        // Regression: `keyword_tail` sliced `statement[..len]` at the keyword
        // byte length without checking the char boundary, so a CJK statement
        // like `loop 每个模型回合` (byte 10 lands inside 个) panicked while
        // being checked against the 10-byte `autonumber` keyword. Unknown
        // block keywords must degrade to plain rows, never panic.
        let source = "sequenceDiagram\n\
U->>AP: prompt(text)\n\
loop 每个模型回合\n\
AP->>SE: run(prompt)\n\
alt 有工具调用\n\
SE->>TL: 执行工具\n\
else 无工具调用\n\
SE-->>AP: RunResult\n\
end\n\
AP-->>U: 渲染结果\n";
        let parsed = parse_sequence_diagram(source, MermaidLimits::default()).unwrap();
        let rows: Vec<_> = parsed.rows.iter().collect();
        assert_eq!(rows.len(), 9, "{rows:?}");
        assert!(matches!(rows[1], SequenceRow::Plain(p) if p == "loop 每个模型回合"));
        assert!(matches!(rows[3], SequenceRow::Plain(p) if p == "alt 有工具调用"));
        assert!(matches!(rows[5], SequenceRow::Plain(p) if p == "else 无工具调用"));
        assert!(matches!(rows[7], SequenceRow::Plain(p) if p == "end"));
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Sequence);
        let text = art.chunks[0].join("\n");
        assert!(text.contains("loop 每个模型回合"), "{text}");
        assert!(text.contains("AP : prompt(text)"), "{text}");
        assert!(text.contains("SE : run(prompt)"), "{text}");
    }

    #[test]
    fn over_budget_sequence_diagram_keeps_output_limit_diagnostic() {
        // Sequence diagrams are NOT participant-split (rows align against the
        // widest participant label, see render_sequence_diagram), so an
        // over-budget sequence must keep the OutputLimit diagnostic even
        // though flowcharts and class diagrams split into bounded panels.
        let source = format!("sequenceDiagram\n{}", "A ->> B: m\n".repeat(1_000));
        let limits = MermaidLimits {
            max_edges: 2_048,
            ..MermaidLimits::default()
        };
        let err = render_mermaid_unicode(&source, 48, limits).unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OutputLimit);
        assert!(err.message.contains("needs"), "{}", err.message);
    }

    #[test]
    fn unsupported_mermaid_kinds_still_fall_back_to_diagnostics() {
        for source in [
            "pie\n\"Breakfast\" 5\n\"Lunch\" 3",
            "gantt\ntitle A Gantt Diagram",
            "stateDiagram-v2\nA --> B",
        ] {
            let err = parse_mermaid(source, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::UnsupportedDiagram,
                "{source}: {err:?}"
            );
            let err = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::UnsupportedDiagram,
                "{source}: {err:?}"
            );
        }
    }
}
