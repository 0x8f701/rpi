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

    let parsed = parse_flowchart(source_statements, header_line, limits)?;
    let diagram = parsed.diagram;
    let estimated_cells = 12usize
        .saturating_add(
            diagram
                .nodes
                .iter()
                .map(|node| {
                    display_width(&node.id)
                        .saturating_add(display_width(&sanitize_mermaid_label(&node.label)))
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
        )
        .saturating_add(
            parsed
                .subgraphs
                .iter()
                .map(|group| display_width(&group.id).saturating_add(display_width(&group.title)).saturating_add(18))
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
        .saturating_add(parsed.subgraphs.len().saturating_mul(2))
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

    for (index, node) in diagram.nodes.iter().enumerate() {
        for group in parsed.subgraphs.iter().filter(|group| group.start_node == index) {
            lines.push(fit_text(
                &format!("subgraph {} · {}", group.id, sanitize_mermaid_label(&group.title)),
                width,
            ));
        }
        append_node(&mut lines, node, width);
        for group in parsed.subgraphs.iter().filter(|group| group.end_node == index + 1) {
            lines.push(fit_text(&format!("end subgraph {}", group.id), width));
        }
    }
    if !diagram.edges.is_empty() {
        lines.push(fit_text("edges", width));
        for edge in &diagram.edges {
            let connector = match (&edge.label, edge.arrow) {
                (Some(label), true) => format!(" ─{}─▶ ", sanitize_mermaid_label(label)),
                (Some(label), false) => format!(" ─{}── ", sanitize_mermaid_label(label)),
                (None, true) => " ───▶ ".to_owned(),
                (None, false) => " ──── ".to_owned(),
            };
            lines.push(fit_text(
                &format!("{}{}{}", edge.from, connector, edge.to),
                width,
            ));
        }
    }

    check_output_cells(&lines, limits)?;
    Ok(MermaidArt {
        lines,
        diagram,
        kind: MermaidDiagramKind::Flowchart,
    })
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
            "Only flowchart/graph and classDiagram diagrams are supported",
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
                    "Only flowchart/graph and classDiagram diagrams are supported",
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
    let minimum_lines = 1usize
        .saturating_add(parsed.diagram.nodes.len().saturating_mul(3))
        .saturating_add(parsed.classes.iter().map(|class| class.members.len()).sum::<usize>())
        .saturating_add(usize::from(!parsed.relations.is_empty()))
        .saturating_add(parsed.relations.len());
    if minimum_lines > limits.max_output_cells {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Mermaid output cell limit is too small for this class diagram",
            None,
        ));
    }
    let mut lines = vec![fit_text("classDiagram", width)];
    for node in &parsed.diagram.nodes {
        let members = parsed
            .classes
            .iter()
            .find(|class| class.name == node.id)
            .map_or(&[][..], |class| class.members.as_slice());
        append_class(&mut lines, &node.id, members, width);
    }
    if !parsed.relations.is_empty() {
        lines.push(fit_text("edges", width));
        for relation in &parsed.relations {
            let connector = match (relation.dotted, &relation.label) {
                (true, Some(label)) => format!(" ··{}··▶ ", sanitize_mermaid_label(label)),
                (true, None) => " ····▶ ".to_owned(),
                (false, Some(label)) => format!(" ─{}─▶ ", sanitize_mermaid_label(label)),
                (false, None) => " ───▶ ".to_owned(),
            };
            lines.push(fit_text(
                &format!("{}{}{}", relation.from, connector, relation.to),
                width,
            ));
        }
    }
    check_output_cells(&lines, limits)?;
    Ok(MermaidArt {
        lines,
        diagram: parsed.diagram,
        kind: MermaidDiagramKind::ClassDiagram,
    })
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

fn check_output_cells(lines: &[String], limits: MermaidLimits) -> Result<(), MermaidDiagnostic> {
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
        let text = art.lines.join("\n");
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
        let text = art.lines.join("\n");
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
}
