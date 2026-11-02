use std::collections::{HashMap, VecDeque};

use unicode_segmentation::UnicodeSegmentation;

use super::text::{display_width, fit_text, pad_to_width, sanitize_inline};

pub const MAX_MERMAID_SOURCE_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_MERMAID_NODES: usize = 64;
pub const DEFAULT_MAX_MERMAID_EDGES: usize = 128;
pub const DEFAULT_MAX_MERMAID_OUTPUT_CELLS: usize = 3_200;
/// Hard cell ceiling for one rendered sequence card. Sequence rows cannot be
/// split into panels (they align against shared lifeline columns), so the
/// capacity grows with the parsed shape instead of the flat flowchart card
/// budget, and this cap keeps hostile shapes bounded. It must stay above
/// everyday 6-participant projections (the reported 6-participant /
/// 11-message source renders 4145 cells at width 117, ~4260 at 120, and a
/// ~14-message variant ~5.1k) while staying below the known-huge 22-row
/// projection (6379-6493 cells at widths 94-96) that the over-budget
/// regressions pin as fallback.
pub const DEFAULT_MAX_SEQUENCE_OUTPUT_CELLS: usize = 6_144;
/// One extra `max_output_cells` slice of sequence budget per this many parsed
/// rows (see `MermaidLimits::sequence_output_cells`). A slice covers eight
/// worst-case rows (arrow + lifeline + label lines) at widths up to ~160.
pub const SEQUENCE_ROWS_PER_BUDGET_SLICE: usize = 8;
/// Participants share the fixed three-line participant-box band, so they buy
/// budget slices at a coarser rate than rows.
pub const SEQUENCE_PARTICIPANTS_PER_BUDGET_SLICE: usize = 16;

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
    StateDiagram,
    ErDiagram,
}

const SUPPORTED_DIAGRAMS: &str = "Only flowchart/graph, classDiagram, sequenceDiagram, stateDiagram, and erDiagram diagrams are supported";

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
    pub dotted: bool,
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
    /// Hard per-card cell ceiling for sequence diagrams; see
    /// `sequence_output_cells`.
    pub max_sequence_output_cells: usize,
}

impl Default for MermaidLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: MAX_MERMAID_SOURCE_BYTES,
            max_nodes: DEFAULT_MAX_MERMAID_NODES,
            max_edges: DEFAULT_MAX_MERMAID_EDGES,
            max_output_cells: DEFAULT_MAX_MERMAID_OUTPUT_CELLS,
            max_sequence_output_cells: DEFAULT_MAX_SEQUENCE_OUTPUT_CELLS,
        }
    }
}

impl MermaidLimits {
    /// Rendered-cell budget for one sequence card. Sequence rows align
    /// against shared participant lifelines, so the render cannot split into
    /// bounded panels the way flowchart and class cards do; instead the flat
    /// `max_output_cells` card budget grows by one slice per
    /// `SEQUENCE_ROWS_PER_BUDGET_SLICE` parsed rows and per
    /// `SEQUENCE_PARTICIPANTS_PER_BUDGET_SLICE` participants, hard-capped at
    /// `max_sequence_output_cells`. An explicitly tightened
    /// `max_output_cells` still binds (slices scale from it), so a budget of
    /// 5 keeps rejecting even a two-participant card.
    pub fn sequence_output_cells(&self, rows: usize, participants: usize) -> usize {
        let slices = rows / SEQUENCE_ROWS_PER_BUDGET_SLICE
            + participants / SEQUENCE_PARTICIPANTS_PER_BUDGET_SLICE;
        self.max_output_cells
            .saturating_add(slices.saturating_mul(self.max_output_cells))
            .min(self.max_sequence_output_cells)
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
    let header = source_statements
        .first()
        .map(|(_, header)| header.as_str())
        .unwrap_or_default();
    if header.eq_ignore_ascii_case("classDiagram") {
        return parse_class_diagram(source, limits).map(|parsed| parsed.diagram);
    }
    if is_sequence_header(header) {
        return parse_sequence_diagram(source, limits).map(|parsed| sequence_to_flowchart(&parsed));
    }
    if is_state_header(header) {
        return parse_state_diagram(source, limits).map(|parsed| parsed.diagram);
    }
    if is_er_header(header) {
        return parse_er_diagram(source, limits).map(|parsed| parsed.diagram);
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
    let header = source_statements
        .first()
        .map(|(_, header)| header.as_str())
        .unwrap_or_default();
    let width = width.clamp(4, limits.max_output_cells.max(4));

    if header.eq_ignore_ascii_case("classDiagram") {
        return render_class_diagram(source, width, limits);
    }
    if is_sequence_header(header) {
        return render_sequence_diagram(source, width, limits);
    }
    if is_state_header(header) {
        return render_state_diagram(source, width, limits);
    }
    if is_er_header(header) {
        return render_er_diagram(source, width, limits);
    }
    let parsed = parse_flowchart(source_statements, header_line, limits)?;
    let diagram = parsed.diagram;
    let lines = match render_flowchart_lines(&diagram, &parsed.subgraphs, width) {
        Ok(lines) if rendered_cells(&lines) <= limits.max_output_cells => lines,
        Ok(_)
        | Err(MermaidDiagnostic {
            kind: MermaidDiagnosticKind::OutputLimit,
            ..
        }) => {
            let chunks = split_flowchart_chunks(
                &diagram,
                &parsed.subgraphs,
                width,
                limits.max_output_cells,
            )?;
            return Ok(MermaidArt {
                chunks,
                diagram,
                kind: MermaidDiagramKind::Flowchart,
            });
        }
        Err(error) => return Err(error),
    };
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram,
        kind: MermaidDiagramKind::Flowchart,
    })
}

fn render_flowchart_lines(
    diagram: &MermaidFlowchart,
    subgraphs: &[MermaidSubgraph],
    width: usize,
) -> Result<Vec<String>, MermaidDiagnostic> {
    let node_indices = (0..diagram.nodes.len()).collect::<Vec<_>>();
    render_flowchart_panel(
        diagram,
        subgraphs,
        &node_indices,
        width,
        true,
        FlowLayout::Sequential,
    )
}

fn flowchart_direction_text(direction: FlowDirection) -> &'static str {
    match direction {
        FlowDirection::TopDown => "TD",
        FlowDirection::BottomUp => "BU",
        FlowDirection::LeftRight => "LR",
        FlowDirection::RightLeft => "RL",
    }
}

const FLOW_UP: u8 = 1;
const FLOW_DOWN: u8 = 2;
const FLOW_LEFT: u8 = 4;
const FLOW_RIGHT: u8 = 8;

#[derive(Clone)]
enum FlowCell {
    Empty,
    WideTail,
    Glyph(String),
    Line(u8),
}

struct FlowCanvas {
    cells: Vec<Vec<FlowCell>>,
}

impl FlowCanvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            cells: vec![vec![FlowCell::Empty; width]; height],
        }
    }

    fn write_text(&mut self, x: usize, y: usize, text: &str) {
        let Some(row) = self.cells.get_mut(y) else {
            return;
        };
        let mut cell = x;
        for grapheme in UnicodeSegmentation::graphemes(text, true) {
            let cells = display_width(grapheme);
            if cells == 0 {
                if let Some(FlowCell::Glyph(existing)) =
                    cell.checked_sub(1).and_then(|index| row.get_mut(index))
                {
                    existing.push_str(grapheme);
                }
                continue;
            }
            if cell.saturating_add(cells) > row.len() {
                break;
            }
            row[cell] = FlowCell::Glyph(grapheme.to_owned());
            for tail in 1..cells {
                row[cell + tail] = FlowCell::WideTail;
            }
            cell += cells;
        }
    }

    fn symbol(&mut self, x: usize, y: usize, symbol: char) {
        if let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) {
            *cell = FlowCell::Glyph(symbol.to_string());
        }
    }

    fn line(&mut self, x: usize, y: usize, mask: u8) {
        let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) else {
            return;
        };
        if matches!(cell, FlowCell::Empty | FlowCell::Line(_)) {
            let current = match cell {
                FlowCell::Line(current) => *current,
                _ => 0,
            };
            *cell = FlowCell::Line(current | mask);
        }
    }

    fn horizontal(&mut self, y: usize, x1: usize, x2: usize) {
        if x1 == x2 {
            return;
        }
        let (left, right) = if x1 < x2 { (x1, x2) } else { (x2, x1) };
        for x in left..=right {
            let mut mask = 0;
            if x > left {
                mask |= FLOW_LEFT;
            }
            if x < right {
                mask |= FLOW_RIGHT;
            }
            self.line(x, y, mask);
        }
    }

    fn vertical(&mut self, x: usize, y1: usize, y2: usize) {
        if y1 == y2 {
            return;
        }
        let (top, bottom) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
        for y in top..=bottom {
            let mut mask = 0;
            if y > top {
                mask |= FLOW_UP;
            }
            if y < bottom {
                mask |= FLOW_DOWN;
            }
            self.line(x, y, mask);
        }
    }

    fn into_lines(self) -> Vec<String> {
        self.cells
            .into_iter()
            .map(|row| {
                let mut line = String::new();
                for cell in row {
                    match cell {
                        FlowCell::Empty => line.push(' '),
                        FlowCell::WideTail => {}
                        FlowCell::Glyph(glyph) => line.push_str(&glyph),
                        FlowCell::Line(mask) => line.push(flow_line_char(mask)),
                    }
                }
                line.trim_end().to_owned()
            })
            .filter(|line| !line.is_empty())
            .collect()
    }
}

fn flow_line_char(mask: u8) -> char {
    match mask {
        1 | 2 | 3 => '│',
        4 | 8 | 12 => '─',
        5 => '┘',
        6 => '┐',
        9 => '└',
        10 => '┌',
        7 => '┤',
        11 => '├',
        13 => '┴',
        14 => '┬',
        15 => '┼',
        _ => '·',
    }
}

#[derive(Clone)]
struct FlowNodeBlock {
    lines: Vec<String>,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
struct FlowPlacement {
    node_index: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
enum FlowLayout {
    Sequential,
    Layered,
}

fn flow_node_block(node: &MermaidNode, width: usize) -> Result<FlowNodeBlock, MermaidDiagnostic> {
    if node.id.starts_with("[*]") && node.label == "●" {
        return Ok(FlowNodeBlock {
            lines: vec!["●".to_owned()],
            width: 1,
            height: 1,
        });
    }
    if width < 13 {
        let rows = split_label_rows(&node.label)
            .map_err(|()| unrepresentable_label(&node.label, "node"))?;
        let lines = rows
            .into_iter()
            .map(|row| fit_text(&row, width.max(1)))
            .collect::<Vec<_>>();
        let block_width = lines.iter().map(|line| display_width(line)).max().unwrap_or(1);
        let height = lines.len();
        return Ok(FlowNodeBlock {
            lines,
            width: block_width,
            height,
        });
    }
    let mut lines = Vec::new();
    append_node(&mut lines, node, width.max(5))?;
    let block_width = lines
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(1);
    for line in &mut lines {
        *line = pad_to_width(line, block_width);
    }
    let height = lines.len();
    Ok(FlowNodeBlock {
        lines,
        width: block_width,
        height,
    })
}

fn render_flowchart_panel(
    diagram: &MermaidFlowchart,
    subgraphs: &[MermaidSubgraph],
    node_indices: &[usize],
    width: usize,
    include_header: bool,
    layout: FlowLayout,
) -> Result<Vec<String>, MermaidDiagnostic> {
    let blocks = node_indices
        .iter()
        .map(|index| flow_node_block(&diagram.nodes[*index], width.saturating_sub(8)))
        .collect::<Result<Vec<_>, _>>()?;
    let horizontal = matches!(
        diagram.direction,
        FlowDirection::LeftRight | FlowDirection::RightLeft
    );
    let placements = match layout {
        FlowLayout::Sequential if horizontal => {
            flow_horizontal_placements(diagram, node_indices, &blocks, width)?
        }
        FlowLayout::Sequential => {
            flow_vertical_placements(node_indices, &blocks, diagram.direction, width)?
        }
        FlowLayout::Layered => flow_layered_placements(diagram, node_indices, &blocks, width)?,
    };
    let internal_edges = diagram
        .edges
        .iter()
        .filter(|edge| {
            node_indices
                .iter()
                .any(|index| diagram.nodes[*index].id == edge.from)
                && node_indices
                    .iter()
                    .any(|index| diagram.nodes[*index].id == edge.to)
        })
        .collect::<Vec<_>>();
    let lane_edges = internal_edges
        .iter()
        .filter(|edge| !flow_edge_is_direct(diagram, &placements, edge, horizontal))
        .count();
    let max_label_width = internal_edges
        .iter()
        .filter_map(|edge| edge.label.as_deref())
        .try_fold(0usize, |max, label| {
            let rows = edge_label_rows(label)?;
            let widest = rows.iter().map(|row| display_width(row)).max().unwrap_or(0);
            Ok::<usize, MermaidDiagnostic>(max.max(widest))
        })?;
    let (canvas_width, canvas_height) =
        flow_canvas_size(&placements, horizontal, lane_edges, max_label_width, width)?;
    let mut canvas = FlowCanvas::new(canvas_width, canvas_height);
    for (placement, block) in placements.iter().zip(&blocks) {
        for (row, line) in block.lines.iter().enumerate() {
            canvas.write_text(placement.x, placement.y + row, line);
        }
    }
    let mut lane = 0usize;
    for edge in &internal_edges {
        let direct = flow_edge_is_direct(diagram, &placements, edge, horizontal);
        draw_flow_edge(
            &mut canvas,
            diagram,
            &placements,
            edge,
            horizontal,
            lane,
            direct,
        )?;
        if !direct {
            lane += 1;
        }
    }
    lane = 0;
    for edge in &internal_edges {
        let direct = flow_edge_is_direct(diagram, &placements, edge, horizontal);
        draw_flow_edge_label(
            &mut canvas,
            diagram,
            &placements,
            edge,
            horizontal,
            lane,
            direct,
        )?;
        if !direct {
            lane += 1;
        }
    }
    if matches!(layout, FlowLayout::Layered) {
        for (placement, block) in placements.iter().zip(&blocks) {
            for (row, line) in block.lines.iter().enumerate() {
                canvas.write_text(placement.x, placement.y + row, line);
            }
        }
    }
    if matches!(layout, FlowLayout::Layered) {
        lane = 0;
        for edge in &internal_edges {
            let direct = flow_edge_is_direct(diagram, &placements, edge, horizontal);
            draw_flow_edge(
                &mut canvas,
                diagram,
                &placements,
                edge,
                horizontal,
                lane,
                direct,
            )?;
            if !direct {
                lane += 1;
            }
        }
    }
    if matches!(layout, FlowLayout::Layered) {
        for (placement, block) in placements.iter().zip(&blocks) {
            if width < 21 || diagram.nodes[placement.node_index].id.starts_with("[*]") {
                for (row, line) in block.lines.iter().enumerate() {
                    canvas.write_text(placement.x, placement.y + row, line);
                }
            }
        }
    }

    let mut lines = Vec::new();
    if include_header {
        lines.push(fit_text(
            &format!("flowchart {}", flowchart_direction_text(diagram.direction)),
            width,
        ));
    }
    for group in subgraphs.iter().rev().filter(|group| {
        node_indices
            .iter()
            .any(|index| *index >= group.start_node && *index < group.end_node)
    }) {
        lines.push(fit_text(
            &format!(
                "subgraph {} · {}",
                group.id,
                single_row_label(&group.title, "subgraph")?
            ),
            width,
        ));
    }
    lines.extend(canvas.into_lines());
    for group in subgraphs.iter().filter(|group| {
        node_indices
            .iter()
            .any(|index| *index >= group.start_node && *index < group.end_node)
    }) {
        lines.push(fit_text(&format!("end subgraph {}", group.id), width));
    }
    for edge in &diagram.edges {
        let source_here = node_indices
            .iter()
            .any(|index| diagram.nodes[*index].id == edge.from);
        let target_here = node_indices
            .iter()
            .any(|index| diagram.nodes[*index].id == edge.to);
        if source_here && !target_here {
            lines.extend(flow_boundary_lines(edge, width)?);
        }
    }
    if lines.iter().any(|line| display_width(line) > width) {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Flowchart canvas exceeds the requested terminal width",
            None,
        ));
    }
    Ok(lines)
}

fn flow_horizontal_placements(
    diagram: &MermaidFlowchart,
    node_indices: &[usize],
    blocks: &[FlowNodeBlock],
    width: usize,
) -> Result<Vec<FlowPlacement>, MermaidDiagnostic> {
    let mut gaps = vec![5usize; blocks.len().saturating_sub(1)];
    for edge in &diagram.edges {
        let Some(source) = node_indices
            .iter()
            .position(|index| diagram.nodes[*index].id == edge.from)
        else {
            continue;
        };
        let Some(target) = node_indices
            .iter()
            .position(|index| diagram.nodes[*index].id == edge.to)
        else {
            continue;
        };
        if source.abs_diff(target) == 1 {
            let gap = source.min(target);
            let label = edge.label.as_deref().map(display_width).unwrap_or(0);
            gaps[gap] = gaps[gap].max(label.saturating_add(4));
        }
    }
    let total = blocks.iter().map(|block| block.width).sum::<usize>() + gaps.iter().sum::<usize>();
    if total > width {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Flowchart rank cannot fit the requested terminal width",
            None,
        ));
    }
    let max_height = blocks.iter().map(|block| block.height).max().unwrap_or(1);
    let mut offsets = Vec::with_capacity(blocks.len());
    let mut x = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        offsets.push(x);
        x += block.width + gaps.get(index).copied().unwrap_or(0);
    }
    let mut placements = Vec::with_capacity(blocks.len());
    for ((node_index, block), offset) in node_indices.iter().copied().zip(blocks).zip(offsets) {
        let placed_x = if diagram.direction == FlowDirection::RightLeft {
            total - offset - block.width
        } else {
            offset
        };
        placements.push(FlowPlacement {
            node_index,
            x: placed_x,
            y: 1 + (max_height - block.height) / 2,
            width: block.width,
            height: block.height,
        });
    }
    Ok(placements)
}

fn flow_vertical_placements(
    node_indices: &[usize],
    blocks: &[FlowNodeBlock],
    direction: FlowDirection,
    width: usize,
) -> Result<Vec<FlowPlacement>, MermaidDiagnostic> {
    const GAP: usize = 4;
    let max_width = blocks.iter().map(|block| block.width).max().unwrap_or(1);
    if max_width > width {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Flowchart node cannot fit the requested terminal width",
            None,
        ));
    }
    let total_height = blocks.iter().map(|block| block.height).sum::<usize>()
        + blocks.len().saturating_sub(1) * GAP;
    let mut y = 0usize;
    let mut placements = Vec::with_capacity(blocks.len());
    for (node_index, block) in node_indices.iter().copied().zip(blocks) {
        let placed_y = if direction == FlowDirection::BottomUp {
            total_height - y - block.height
        } else {
            y
        };
        placements.push(FlowPlacement {
            node_index,
            x: (max_width - block.width) / 2,
            y: placed_y,
            width: block.width,
            height: block.height,
        });
        y += block.height + GAP;
    }
    Ok(placements)
}

fn flow_layered_placements(
    diagram: &MermaidFlowchart,
    node_indices: &[usize],
    blocks: &[FlowNodeBlock],
    width: usize,
) -> Result<Vec<FlowPlacement>, MermaidDiagnostic> {
    let ranks = flow_layered_ranks(diagram, node_indices);
    let rank_count = ranks.iter().copied().max().unwrap_or(0) + 1;
    let horizontal = matches!(
        diagram.direction,
        FlowDirection::LeftRight | FlowDirection::RightLeft
    );
    if horizontal {
        flow_layered_horizontal_placements(diagram, node_indices, blocks, &ranks, rank_count, width)
    } else {
        flow_layered_vertical_placements(node_indices, blocks, &ranks, rank_count, diagram.direction, width)
    }
}

fn flow_layered_ranks(diagram: &MermaidFlowchart, node_indices: &[usize]) -> Vec<usize> {
    let positions = node_indices
        .iter()
        .enumerate()
        .map(|(position, index)| (diagram.nodes[*index].id.as_str(), position))
        .collect::<HashMap<_, _>>();
    let mut outgoing = vec![Vec::new(); node_indices.len()];
    let mut incoming = vec![Vec::new(); node_indices.len()];
    for edge in &diagram.edges {
        let (Some(&source), Some(&target)) =
            (positions.get(edge.from.as_str()), positions.get(edge.to.as_str()))
        else {
            continue;
        };
        if source != target {
            outgoing[source].push(target);
            incoming[target].push(source);
        }
    }

    let mut seen = vec![false; node_indices.len()];
    let mut finish_order = Vec::with_capacity(node_indices.len());
    for node in 0..node_indices.len() {
        flow_finish_order(node, &outgoing, &mut seen, &mut finish_order);
    }
    let mut component_of = vec![usize::MAX; node_indices.len()];
    let mut component_count = 0usize;
    for &node in finish_order.iter().rev() {
        if component_of[node] != usize::MAX {
            continue;
        }
        flow_assign_component(node, &incoming, component_count, &mut component_of);
        component_count += 1;
    }

    let mut component_outgoing = vec![Vec::new(); component_count];
    let mut indegree = vec![0usize; component_count];
    for source in 0..outgoing.len() {
        for &target in &outgoing[source] {
            let source_component = component_of[source];
            let target_component = component_of[target];
            if source_component == target_component
                || component_outgoing[source_component].contains(&target_component)
            {
                continue;
            }
            component_outgoing[source_component].push(target_component);
            indegree[target_component] += 1;
        }
    }
    let mut queue = indegree
        .iter()
        .enumerate()
        .filter_map(|(component, degree)| (*degree == 0).then_some(component))
        .collect::<VecDeque<_>>();
    let mut component_rank = vec![0usize; component_count];
    while let Some(source) = queue.pop_front() {
        for &target in &component_outgoing[source] {
            component_rank[target] = component_rank[target].max(component_rank[source] + 1);
            indegree[target] -= 1;
            if indegree[target] == 0 {
                queue.push_back(target);
            }
        }
    }
    component_of
        .into_iter()
        .map(|component| component_rank[component])
        .collect()
}

fn flow_finish_order(
    node: usize,
    outgoing: &[Vec<usize>],
    seen: &mut [bool],
    order: &mut Vec<usize>,
) {
    if seen[node] {
        return;
    }
    seen[node] = true;
    for &target in &outgoing[node] {
        flow_finish_order(target, outgoing, seen, order);
    }
    order.push(node);
}

fn flow_assign_component(
    node: usize,
    incoming: &[Vec<usize>],
    component: usize,
    component_of: &mut [usize],
) {
    if component_of[node] != usize::MAX {
        return;
    }
    component_of[node] = component;
    for &source in &incoming[node] {
        flow_assign_component(source, incoming, component, component_of);
    }
}

fn flow_layered_vertical_placements(
    node_indices: &[usize],
    blocks: &[FlowNodeBlock],
    ranks: &[usize],
    rank_count: usize,
    direction: FlowDirection,
    width: usize,
) -> Result<Vec<FlowPlacement>, MermaidDiagnostic> {
    const COLUMN_GAP: usize = 4;
    const RANK_GAP: usize = 4;
    let mut row_widths = vec![0usize; rank_count];
    let mut row_heights = vec![0usize; rank_count];
    for (block, rank) in blocks.iter().zip(ranks) {
        if row_widths[*rank] > 0 {
            row_widths[*rank] += COLUMN_GAP;
        }
        row_widths[*rank] += block.width;
        row_heights[*rank] = row_heights[*rank].max(block.height);
    }
    let canvas_width = row_widths.iter().copied().max().unwrap_or(1);
    if canvas_width > width {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "A layered Mermaid rank cannot fit the requested terminal width",
            None,
        ));
    }
    let total_height = row_heights.iter().sum::<usize>()
        + rank_count.saturating_sub(1) * RANK_GAP;
    let mut rank_y = vec![0usize; rank_count];
    let mut y = 0usize;
    for (rank, height) in row_heights.iter().copied().enumerate() {
        rank_y[rank] = y;
        y += height + RANK_GAP;
    }
    let mut row_x = row_widths
        .iter()
        .map(|row_width| (canvas_width - row_width) / 2)
        .collect::<Vec<_>>();
    let mut placements = Vec::with_capacity(blocks.len());
    for ((node_index, block), rank) in node_indices.iter().copied().zip(blocks).zip(ranks) {
        let logical_y = rank_y[*rank] + (row_heights[*rank] - block.height) / 2;
        let placed_y = if direction == FlowDirection::BottomUp {
            total_height - logical_y - block.height
        } else {
            logical_y
        };
        placements.push(FlowPlacement {
            node_index,
            x: row_x[*rank],
            y: placed_y,
            width: block.width,
            height: block.height,
        });
        row_x[*rank] += block.width + COLUMN_GAP;
    }
    Ok(placements)
}

fn flow_layered_horizontal_placements(
    diagram: &MermaidFlowchart,
    node_indices: &[usize],
    blocks: &[FlowNodeBlock],
    ranks: &[usize],
    rank_count: usize,
    width: usize,
) -> Result<Vec<FlowPlacement>, MermaidDiagnostic> {
    const ROW_GAP: usize = 2;
    let mut column_widths = vec![0usize; rank_count];
    let mut column_heights = vec![0usize; rank_count];
    for (block, rank) in blocks.iter().zip(ranks) {
        column_widths[*rank] = column_widths[*rank].max(block.width);
        if column_heights[*rank] > 0 {
            column_heights[*rank] += ROW_GAP;
        }
        column_heights[*rank] += block.height;
    }
    let mut gaps = vec![5usize; rank_count.saturating_sub(1)];
    let positions = node_indices
        .iter()
        .enumerate()
        .map(|(position, index)| (diagram.nodes[*index].id.as_str(), position))
        .collect::<HashMap<_, _>>();
    for edge in &diagram.edges {
        let (Some(&source), Some(&target)) =
            (positions.get(edge.from.as_str()), positions.get(edge.to.as_str()))
        else {
            continue;
        };
        if ranks[source].abs_diff(ranks[target]) != 1 {
            continue;
        }
        let gap = ranks[source].min(ranks[target]);
        let label = edge
            .label
            .as_deref()
            .and_then(|label| edge_label_rows(label).ok())
            .and_then(|rows| rows.into_iter().map(|row| display_width(&row)).max())
            .unwrap_or(0);
        gaps[gap] = gaps[gap].max(label.saturating_add(4));
    }
    let total_width = column_widths.iter().sum::<usize>() + gaps.iter().sum::<usize>();
    if total_width > width {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Layered Mermaid ranks cannot fit the requested terminal width",
            None,
        ));
    }
    let canvas_height = column_heights.iter().copied().max().unwrap_or(1);
    let mut column_x = vec![0usize; rank_count];
    let mut x = 0usize;
    for rank in 0..rank_count {
        column_x[rank] = x;
        x += column_widths[rank] + gaps.get(rank).copied().unwrap_or(0);
    }
    let mut column_y = column_heights
        .iter()
        .map(|column_height| 1 + (canvas_height - column_height) / 2)
        .collect::<Vec<_>>();
    let mut placements = Vec::with_capacity(blocks.len());
    for ((node_index, block), rank) in node_indices.iter().copied().zip(blocks).zip(ranks) {
        let logical_x = column_x[*rank] + (column_widths[*rank] - block.width) / 2;
        let placed_x = if diagram.direction == FlowDirection::RightLeft {
            total_width - logical_x - block.width
        } else {
            logical_x
        };
        placements.push(FlowPlacement {
            node_index,
            x: placed_x,
            y: column_y[*rank],
            width: block.width,
            height: block.height,
        });
        column_y[*rank] += block.height + ROW_GAP;
    }
    Ok(placements)
}

fn flow_edge_is_direct(
    diagram: &MermaidFlowchart,
    placements: &[FlowPlacement],
    edge: &MermaidEdge,
    horizontal: bool,
) -> bool {
    if edge.from == edge.to {
        return false;
    }
    let Some(source) = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.from)
    else {
        return false;
    };
    let Some(target) = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.to)
    else {
        return false;
    };
    if horizontal && source.x == target.x || !horizontal && source.y == target.y {
        return false;
    }
    !placements.iter().any(|node| {
        if node.node_index == source.node_index || node.node_index == target.node_index {
            return false;
        }
        if horizontal {
            node.x > source.x.min(target.x) && node.x < source.x.max(target.x)
        } else {
            node.y > source.y.min(target.y) && node.y < source.y.max(target.y)
        }
    })
}

fn flow_canvas_size(
    placements: &[FlowPlacement],
    horizontal: bool,
    lanes: usize,
    max_label_width: usize,
    width: usize,
) -> Result<(usize, usize), MermaidDiagnostic> {
    let right = placements
        .iter()
        .map(|node| node.x + node.width)
        .max()
        .unwrap_or(1);
    let bottom = placements
        .iter()
        .map(|node| node.y + node.height)
        .max()
        .unwrap_or(1);
    let canvas_width = if horizontal {
        right
    } else {
        (right + lanes * 2 + max_label_width.saturating_add(3)).min(width)
    };
    let canvas_height = if horizontal {
        bottom + lanes * 2 + 2
    } else {
        bottom
    };
    if canvas_width > width {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Flowchart routes cannot fit the requested terminal width",
            None,
        ));
    }
    Ok((canvas_width, canvas_height))
}

fn draw_flow_edge(
    canvas: &mut FlowCanvas,
    diagram: &MermaidFlowchart,
    placements: &[FlowPlacement],
    edge: &MermaidEdge,
    horizontal: bool,
    lane: usize,
    direct: bool,
) -> Result<(), MermaidDiagnostic> {
    let source = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.from)
        .ok_or_else(|| {
            diagnostic(
                MermaidDiagnosticKind::InvalidSyntax,
                "Missing flowchart source",
                None,
            )
        })?;
    let target = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.to)
        .ok_or_else(|| {
            diagnostic(
                MermaidDiagnosticKind::InvalidSyntax,
                "Missing flowchart target",
                None,
            )
        })?;
    if source.node_index == target.node_index {
        draw_flow_self_edge(canvas, source, horizontal, lane, edge.arrow);
    } else if horizontal {
        draw_horizontal_flow_edge(canvas, source, target, lane, direct, edge.arrow);
    } else {
        draw_vertical_flow_edge(canvas, source, target, lane, direct, edge.arrow);
    }
    Ok(())
}

fn draw_flow_edge_label(
    canvas: &mut FlowCanvas,
    diagram: &MermaidFlowchart,
    placements: &[FlowPlacement],
    edge: &MermaidEdge,
    horizontal: bool,
    lane: usize,
    direct: bool,
) -> Result<(), MermaidDiagnostic> {
    let source = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.from)
        .ok_or_else(|| diagnostic(MermaidDiagnosticKind::InvalidSyntax, "Missing flowchart source", None))?;
    let target = placements
        .iter()
        .find(|node| diagram.nodes[node.node_index].id == edge.to)
        .ok_or_else(|| diagnostic(MermaidDiagnosticKind::InvalidSyntax, "Missing flowchart target", None))?;
    let rows = match edge.label.as_deref() {
        Some(label) => {
            let mut rows = edge_label_rows(label)?;
            if edge.dotted && let Some(first) = rows.first_mut() {
                *first = format!("⋯ {first}");
            }
            rows
        }
        None if edge.dotted => vec!["⋯".to_owned()],
        None => Vec::new(),
    };
    if !rows.is_empty() {
        draw_flow_label(canvas, source, target, horizontal, lane, direct, &rows);
    }
    Ok(())
}

fn draw_horizontal_flow_edge(
    canvas: &mut FlowCanvas,
    source: &FlowPlacement,
    target: &FlowPlacement,
    lane: usize,
    direct: bool,
    arrow: bool,
) {
    let rightward = target.x > source.x;
    let source_x = if rightward {
        source.x + source.width - 1
    } else {
        source.x
    };
    let target_x = if rightward {
        target.x
    } else {
        target.x + target.width - 1
    };
    let source_y = source.y + source.height / 2;
    let target_y = target.y + target.height / 2;
    canvas.symbol(source_x, source_y, if rightward { '├' } else { '┤' });
    if direct {
        let outside_source = if rightward {
            source_x + 1
        } else {
            source_x.saturating_sub(1)
        };
        let outside_target = if rightward {
            target_x.saturating_sub(1)
        } else {
            target_x + 1
        };
        canvas.horizontal(source_y, outside_source, outside_target);
        if source_y != target_y {
            canvas.vertical(outside_target, source_y, target_y);
            canvas.horizontal(target_y, outside_target, target_x);
        }
    } else {
        let lane_y = source.y.max(target.y) + source.height.max(target.height) + 1 + lane * 2;
        let outside_source = if rightward {
            source_x + 1
        } else {
            source_x.saturating_sub(1)
        };
        let outside_target = if rightward {
            target_x.saturating_sub(1)
        } else {
            target_x + 1
        };
        canvas.horizontal(source_y, source_x, outside_source);
        canvas.vertical(outside_source, source_y, lane_y);
        canvas.horizontal(lane_y, outside_source, outside_target);
        canvas.vertical(outside_target, lane_y, target_y);
        canvas.horizontal(target_y, outside_target, target_x);
    }
    canvas.symbol(
        target_x,
        target_y,
        if arrow {
            if rightward { '▶' } else { '◀' }
        } else {
            '┼'
        },
    );
}

fn draw_vertical_flow_edge(
    canvas: &mut FlowCanvas,
    source: &FlowPlacement,
    target: &FlowPlacement,
    lane: usize,
    direct: bool,
    arrow: bool,
) {
    let downward = target.y > source.y;
    let source_y = if downward {
        source.y + source.height - 1
    } else {
        source.y
    };
    let target_y = if downward {
        target.y
    } else {
        target.y + target.height - 1
    };
    let source_x = source.x + source.width / 2;
    let target_x = target.x + target.width / 2;
    canvas.symbol(source_x, source_y, if downward { '┬' } else { '┴' });
    if direct {
        let outside_source = if downward {
            source_y + 1
        } else {
            source_y.saturating_sub(1)
        };
        let outside_target = if downward {
            target_y.saturating_sub(1)
        } else {
            target_y + 1
        };
        canvas.vertical(source_x, outside_source, outside_target);
        if source_x != target_x {
            canvas.horizontal(outside_target, source_x, target_x);
        }
        canvas.vertical(target_x, outside_target, target_y);
    } else {
        let lane_x = source.x.max(target.x) + source.width.max(target.width) + 2 + lane * 2;
        let outside_source = if downward {
            source_y + 1
        } else {
            source_y.saturating_sub(1)
        };
        let outside_target = if downward {
            target_y.saturating_sub(1)
        } else {
            target_y + 1
        };
        canvas.vertical(source_x, source_y, outside_source);
        canvas.horizontal(outside_source, source_x, lane_x);
        canvas.vertical(lane_x, outside_source, outside_target);
        canvas.horizontal(outside_target, lane_x, target_x);
        canvas.vertical(target_x, outside_target, target_y);
    }
    canvas.symbol(
        target_x,
        target_y,
        if arrow {
            if downward { '▼' } else { '▲' }
        } else {
            '┼'
        },
    );
}

fn draw_flow_self_edge(
    canvas: &mut FlowCanvas,
    node: &FlowPlacement,
    horizontal: bool,
    lane: usize,
    arrow: bool,
) {
    if horizontal {
        let y = node.y + node.height / 2;
        let right = node.x + node.width - 1;
        let left = node.x;
        let loop_y = node.y + node.height + 1 + lane * 2;
        canvas.symbol(right, y, '├');
        canvas.horizontal(y, right, right + 1);
        canvas.vertical(right + 1, y, loop_y);
        canvas.horizontal(loop_y, left.saturating_sub(1), right + 1);
        canvas.vertical(left.saturating_sub(1), loop_y, y);
        canvas.symbol(left, y, if arrow { '▶' } else { '┼' });
    } else {
        let x = node.x + node.width / 2;
        let top = node.y;
        let bottom = node.y + node.height - 1;
        let loop_x = node.x + node.width + 2 + lane * 2;
        canvas.symbol(x, bottom, '┬');
        canvas.vertical(x, bottom, bottom + 1);
        canvas.horizontal(bottom + 1, x, loop_x);
        canvas.vertical(loop_x, top.saturating_sub(1), bottom + 1);
        canvas.horizontal(top.saturating_sub(1), loop_x, x);
        canvas.symbol(x, top, if arrow { '▼' } else { '┼' });
    }
}

fn draw_flow_label(
    canvas: &mut FlowCanvas,
    source: &FlowPlacement,
    target: &FlowPlacement,
    horizontal: bool,
    lane: usize,
    direct: bool,
    rows: &[String],
) {
    let Some(first) = rows.first() else {
        return;
    };
    let text = format!("[{first}]");
    let (x, y) = if horizontal {
        let y = if direct {
            source.y + source.height / 2
        } else {
            source.y.max(target.y) + source.height.max(target.height) + 1 + lane * 2
        };
        let left = (source.x + source.width).min(target.x + target.width);
        let right = source.x.max(target.x);
        (
            left + right
                .saturating_sub(left)
                .saturating_sub(display_width(&text))
                / 2,
            y,
        )
    } else {
        let y = (source.y + source.height / 2 + target.y + target.height / 2) / 2
            + if direct { 0 } else { lane };
        let preferred = (source.x + source.width).max(target.x + target.width)
            + 1
            + if direct { 0 } else { lane * 2 };
        let widest = rows.iter().map(|row| display_width(row)).max().unwrap_or(0) + 2;
        let x = preferred.min(canvas.cells.first().map_or(0, Vec::len).saturating_sub(widest));
        (x, y)
    };
    canvas.write_text(x, y, &text);
    for (offset, row) in rows[1..].iter().enumerate() {
        canvas.write_text(x + 1, y + 1 + offset, row);
    }
}

/// Cross-panel boundary summary for one edge. A multi-row label renders its
/// first row in brackets after the source id and the remaining rows indented
/// below, so `<br/>` markup becomes readable rows instead of failing.
fn flow_boundary_lines(edge: &MermaidEdge, width: usize) -> Result<Vec<String>, MermaidDiagnostic> {
    let rows = match edge.label.as_deref() {
        None => vec![format!("boundary: {} → … {}", edge.from, edge.to)],
        Some(label) => edge_label_rows(label)?
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 0 {
                    format!("boundary: {} [{row}] → … {}", edge.from, edge.to)
                } else {
                    format!("  {row}")
                }
            })
            .collect(),
    };
    Ok(rows.iter().map(|line| fit_text(line, width)).collect())
}

fn split_flowchart_chunks(
    diagram: &MermaidFlowchart,
    subgraphs: &[MermaidSubgraph],
    width: usize,
    limit: usize,
) -> Result<Vec<Vec<String>>, MermaidDiagnostic> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < diagram.nodes.len() {
        let mut best: Option<(usize, Vec<String>)> = None;
        for end in start + 1..=diagram.nodes.len() {
            let indices = (start..end).collect::<Vec<_>>();
            match render_flowchart_panel(
                diagram,
                subgraphs,
                &indices,
                width,
                start == 0,
                FlowLayout::Sequential,
            ) {
                Ok(lines) if rendered_cells(&lines) <= limit => best = Some((end, lines)),
                Ok(_)
                | Err(MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::OutputLimit,
                    ..
                }) => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let Some((end, lines)) = best else {
            return Err(diagnostic(
                MermaidDiagnosticKind::OutputLimit,
                "A Mermaid flowchart panel cannot fit within the output limits",
                None,
            ));
        };
        chunks.push(lines);
        start = end;
    }
    Ok(chunks)
}

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
    /// Six-character cardinality operator for ER relations (`||--o{`); `None`
    /// for class-diagram relations, which use the solid/dotted arrow form.
    cardinality: Option<String>,
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
            SUPPORTED_DIAGRAMS,
            Some(header_line),
        )
    })?;
    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    let mut subgraphs = Vec::new();
    let mut open_subgraphs: Vec<(String, String, usize, usize)> = Vec::new();
    for (line, statement) in source_statements.into_iter().skip(1) {
        if let Some(declaration) = statement.strip_prefix("subgraph").map(str::trim) {
            if declaration.is_empty() {
                return Err(invalid_line(line, &statement));
            }
            let mut cursor = Cursor::new(declaration);
            let group = cursor
                .node()
                .ok_or_else(|| invalid_line(line, &statement))?;
            if !cursor.done() {
                return Err(invalid_line(line, &statement));
            }
            open_subgraphs.push((group.id, group.label, nodes.len(), line));
            continue;
        }
        if statement.eq_ignore_ascii_case("end") {
            let Some((id, title, start_node, _)) = open_subgraphs.pop() else {
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
        if let Some(rest) = keyword_tail(&statement, "style") {
            validate_style_directive(rest, line, &statement)?;
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
    if let Some(&(_, _, _, line)) = open_subgraphs.last() {
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

/// Validates a `style <id> <prop>:<value>,…` directive. The TUI projection
/// carries no per-node colors, so valid directives are accepted and dropped;
/// a missing/malformed id or property pair fails closed.
fn validate_style_directive(
    rest: &str,
    line: usize,
    statement: &str,
) -> Result<(), MermaidDiagnostic> {
    let rest = rest.trim();
    let (id, properties) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let properties = properties.trim();
    if !valid_identifier(id) || properties.is_empty() {
        return Err(invalid_line(line, statement));
    }
    let all_properties_valid = properties.split(',').all(|pair| {
        pair.split_once(':')
            .is_some_and(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty())
    });
    if !all_properties_valid {
        return Err(invalid_line(line, statement));
    }
    Ok(())
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
                    SUPPORTED_DIAGRAMS,
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
            dotted: relation.dotted,
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
    let (right, label) = remainder
        .split_once(':')
        .map_or((remainder, None), |(right, label)| {
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
        cardinality: None,
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
    fit_text(
        &format!("{}{}{}", relation.from, connector, relation.to),
        width,
    )
}

/// Split an over-budget class diagram by class into bounded panels: each
/// class unit carries the rendered cost of its outgoing relations, and
/// relations whose endpoints land in different panels become stub notes,
/// mirroring `split_flowchart_chunks`.
fn split_class_chunks(parsed: &ParsedClassDiagram, width: usize, limit: usize) -> Vec<Vec<String>> {
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
    let chunk_count = chunk_of_class
        .last()
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
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
    output.push(format!(
        "│{}│",
        pad_to_width(&fit_text(name, inner_width), inner_width)
    ));
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

#[derive(Clone, Debug)]
struct ParsedState {
    diagram: MermaidFlowchart,
    direction: Option<FlowDirection>,
}

fn is_state_header(header: &str) -> bool {
    let lower = header.to_ascii_lowercase();
    lower == "statediagram" || lower == "statediagram-v2"
}

fn is_er_header(header: &str) -> bool {
    header.eq_ignore_ascii_case("erDiagram")
}

/// Parse a `stateDiagram`/`stateDiagram-v2` source into a flowchart projection.
/// States become nodes (`[*]` endpoints as circles, `<<choice>>`/`<<fork>>`/
/// `<<join>>` as diamonds, the rest as boxes); transitions become edges.
/// Composite `state C { … }` blocks are flattened — their inner statements are
/// processed as top-level declarations and transitions.
fn parse_state_diagram(
    source: &str,
    limits: MermaidLimits,
) -> Result<ParsedState, MermaidDiagnostic> {
    let source_statements = statements(source);
    let header_line = source_statements.first().map(|(line, _)| *line);
    let Some(header_line) = header_line else {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Mermaid diagram is empty",
            None,
        ));
    };
    let header = source_statements
        .first()
        .map(|(_, header)| header.as_str())
        .unwrap_or_default();
    if !is_state_header(header) {
        return Err(diagnostic(
            MermaidDiagnosticKind::UnsupportedDiagram,
            SUPPORTED_DIAGRAMS,
            Some(header_line),
        ));
    }
    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    let mut direction: Option<FlowDirection> = None;
    let mut composite_depth = 0usize;
    for (line, statement) in source_statements.into_iter().skip(1) {
        if statement == "}" {
            if composite_depth == 0 {
                return Err(invalid_state_line(line, &statement));
            }
            composite_depth -= 1;
            continue;
        }
        if let Some(rest) = keyword_tail(&statement, "direction") {
            let parsed = parse_state_direction(rest.trim())
                .ok_or_else(|| invalid_state_line(line, &statement))?;
            direction = Some(parsed);
            continue;
        }
        if let Some(rest) = keyword_tail(&statement, "state") {
            let rest = rest.trim();
            if let Some(name_part) = rest.strip_suffix('{') {
                // Both the plain (`state C {`) and aliased (`state "label"
                // as Alias {`) composite headers register the state; the
                // alias form carries its quoted label into the node.
                let (id, label, shape) = parse_state_declaration(name_part.trim())
                    .ok_or_else(|| invalid_state_line(line, &statement))?;
                upsert_node(
                    MermaidNode { id, label, shape },
                    &mut nodes,
                    &mut node_indices,
                    limits,
                    line,
                )?;
                composite_depth += 1;
                continue;
            }
            let (id, label, shape) = parse_state_declaration(rest)
                .ok_or_else(|| invalid_state_line(line, &statement))?;
            upsert_node(
                MermaidNode { id, label, shape },
                &mut nodes,
                &mut node_indices,
                limits,
                line,
            )?;
            continue;
        }
        if statement.contains("-->") {
            parse_state_transition(
                &statement,
                line,
                &mut nodes,
                &mut node_indices,
                &mut edges,
                limits,
            )?;
            continue;
        }
        return Err(invalid_state_line(line, &statement));
    }
    if composite_depth != 0 {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "State composite block is missing }",
            None,
        ));
    }
    if nodes.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "State diagram contains no states",
            Some(header_line),
        ));
    }
    Ok(ParsedState {
        diagram: MermaidFlowchart {
            direction: direction.unwrap_or(FlowDirection::TopDown),
            nodes,
            edges,
        },
        direction,
    })
}

/// Parse a `state` declaration tail (text after `state `) into
/// `(id, label, shape)`. Handles the alias form `"label" as id` (with an
/// optional `<<choice>>`/`<<fork>>`/`<<join>>` marker) and the plain form `id`
/// (same optional marker). Returns `None` for malformed input.
fn parse_state_declaration(rest: &str) -> Option<(String, String, MermaidNodeShape)> {
    if let Some(after_quote) = rest.strip_prefix('"') {
        let close = after_quote.find('"')?;
        let label = after_quote[..close].to_owned();
        let after = after_quote[close + 1..].trim();
        let id_part = keyword_tail(after, "as")?.trim();
        let (id, marker) = split_state_marker(id_part)?;
        Some((id, label, marker.unwrap_or(MermaidNodeShape::Rectangle)))
    } else {
        let (id, marker) = split_state_marker(rest)?;
        let label = id.clone();
        Some((id, label, marker.unwrap_or(MermaidNodeShape::Plain)))
    }
}

/// Split an `id <<marker>>` tail into `(id, Option<shape>)`, validating the id
/// and accepting only `choice`/`fork`/`join` markers (mapped to a diamond).
fn split_state_marker(s: &str) -> Option<(String, Option<MermaidNodeShape>)> {
    if let Some(open) = s.find("<<") {
        let id = s[..open].trim();
        let after = &s[open + 2..];
        let close = after.find(">>")?;
        let marker = after[..close].trim();
        if !after[close + 2..].trim().is_empty() {
            return None;
        }
        if !valid_identifier(id) {
            return None;
        }
        let shape = match marker.to_ascii_lowercase().as_str() {
            "choice" | "fork" | "join" => MermaidNodeShape::Diamond,
            _ => return None,
        };
        Some((id.to_owned(), Some(shape)))
    } else if valid_identifier(s) {
        Some((s.to_owned(), None))
    } else {
        None
    }
}

fn parse_state_direction(s: &str) -> Option<FlowDirection> {
    match s.to_ascii_uppercase().as_str() {
        "TD" | "TB" => Some(FlowDirection::TopDown),
        "BT" => Some(FlowDirection::BottomUp),
        "LR" => Some(FlowDirection::LeftRight),
        "RL" => Some(FlowDirection::RightLeft),
        _ => None,
    }
}

fn is_state_ref(s: &str) -> bool {
    s == "[*]" || valid_identifier(s)
}

/// Parse one or more chained transitions `A --> B --> C` with optional labels
/// `A --> B: label` (a label terminates the chain, consuming the rest of the
/// statement). `[*]` is a valid source/target endpoint.
fn parse_state_transition(
    statement: &str,
    line: usize,
    nodes: &mut Vec<MermaidNode>,
    node_indices: &mut HashMap<String, usize>,
    edges: &mut Vec<MermaidEdge>,
    limits: MermaidLimits,
) -> Result<(), MermaidDiagnostic> {
    let segments: Vec<&str> = statement.split("-->").collect();
    if segments.len() < 2 {
        return Err(invalid_state_line(line, statement));
    }
    let mut source = segments[0].trim().to_owned();
    if !is_state_ref(&source) {
        return Err(invalid_state_line(line, statement));
    }
    let mut source_is_endpoint = source == "[*]";
    upsert_state_ref(
        &source,
        source_is_endpoint,
        nodes,
        node_indices,
        limits,
        line,
    )?;
    for (index, segment) in segments[1..].iter().enumerate() {
        let mut cursor = Cursor::new(segment);
        cursor.whitespace();
        let (target, target_is_endpoint) = if let Some(rest) = cursor.rest.strip_prefix("[*]") {
            cursor.rest = rest;
            (
                MermaidNode {
                    id: "[*]".to_owned(),
                    label: "●".to_owned(),
                    shape: MermaidNodeShape::Circle,
                },
                true,
            )
        } else {
            (
                cursor
                    .node()
                    .ok_or_else(|| invalid_state_line(line, statement))?,
                false,
            )
        };
        if target_is_endpoint && index + 2 < segments.len() {
            return Err(invalid_state_line(line, statement));
        }
        cursor.whitespace();
        let label = if cursor.done() {
            None
        } else if let Some(rest) = cursor.rest.strip_prefix(':') {
            let text = rest.trim();
            if text.is_empty() {
                None
            } else {
                Some(multi_row_label(text, "transition")?)
            }
        } else {
            return Err(invalid_state_line(line, statement));
        };
        if edges.len() >= limits.max_edges {
            return Err(diagnostic(
                MermaidDiagnosticKind::OversizeGraph,
                format!("Mermaid edge limit of {} exceeded", limits.max_edges),
                Some(line),
            ));
        }
        if target_is_endpoint {
            upsert_state_ref("[*]", false, nodes, node_indices, limits, line)?;
        } else {
            upsert_node(target.clone(), nodes, node_indices, limits, line)?;
        }
        let has_label = label.is_some();
        edges.push(MermaidEdge {
            from: state_ref_id(&source, source_is_endpoint),
            to: state_ref_id(&target.id, false),
            label,
            arrow: true,
            dotted: false,
        });
        source = target.id;
        source_is_endpoint = target_is_endpoint;
        if has_label && index + 2 < segments.len() {
            return Err(invalid_state_line(line, statement));
        }
    }
    Ok(())
}

fn state_ref_id(id: &str, is_source: bool) -> String {
    if id == "[*]" {
        if is_source { "[*]start" } else { "[*]end" }.to_owned()
    } else {
        id.to_owned()
    }
}

fn upsert_state_ref(
    id: &str,
    is_source: bool,
    nodes: &mut Vec<MermaidNode>,
    node_indices: &mut HashMap<String, usize>,
    limits: MermaidLimits,
    line: usize,
) -> Result<(), MermaidDiagnostic> {
    let node = if id == "[*]" {
        MermaidNode {
            id: state_ref_id(id, is_source),
            label: "●".to_owned(),
            shape: MermaidNodeShape::Circle,
        }
    } else {
        MermaidNode {
            id: id.to_owned(),
            label: id.to_owned(),
            shape: MermaidNodeShape::Plain,
        }
    };
    upsert_node(node, nodes, node_indices, limits, line)
}

fn render_state_diagram(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let parsed = parse_state_diagram(source, limits)?;
    let node_indices = (0..parsed.diagram.nodes.len()).collect::<Vec<_>>();
    let render = |diagram: &MermaidFlowchart| {
        render_flowchart_panel(
            diagram,
            &[],
            &node_indices,
            width,
            false,
            FlowLayout::Layered,
        )
    };
    let mut lines = match render(&parsed.diagram) {
        Ok(lines) => lines,
        Err(MermaidDiagnostic {
            kind: MermaidDiagnosticKind::OutputLimit,
            ..
        }) if matches!(
            parsed.diagram.direction,
            FlowDirection::LeftRight | FlowDirection::RightLeft
        ) => {
            let mut stacked = parsed.diagram.clone();
            stacked.direction = FlowDirection::TopDown;
            render(&stacked)?
        }
        Err(error) => return Err(error),
    };
    let header = match parsed.direction {
        Some(direction) => format!("stateDiagram {}", flowchart_direction_text(direction)),
        None => "stateDiagram".to_owned(),
    };
    lines.insert(0, fit_text(&header, width));
    check_output_cells(&lines, limits.max_output_cells)?;
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram: parsed.diagram,
        kind: MermaidDiagramKind::StateDiagram,
    })
}

fn invalid_state_line(line: usize, statement: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!(
            "Unsupported stateDiagram syntax: {}",
            fit_text(statement, 80)
        ),
        Some(line),
    )
}

/// Parse an `erDiagram` into the class-diagram projection: entities become
/// nodes (boxes with attribute members), relations become edges carrying the
/// six-character cardinality operator. Reuses `ParsedClassDiagram`/
/// `MermaidClass`/`ClassRelation` so ER renders with the same `append_class`
/// box helper as class diagrams.
fn parse_er_diagram(
    source: &str,
    limits: MermaidLimits,
) -> Result<ParsedClassDiagram, MermaidDiagnostic> {
    let mut nodes = Vec::new();
    let mut node_indices = HashMap::new();
    let mut edges = Vec::new();
    let mut classes = Vec::new();
    let mut relations = Vec::new();
    let mut current_entity: Option<(String, Vec<String>, usize)> = None;
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
            if !is_er_header(statement) {
                return Err(diagnostic(
                    MermaidDiagnosticKind::UnsupportedDiagram,
                    SUPPORTED_DIAGRAMS,
                    Some(line),
                ));
            }
            saw_header = true;
            continue;
        }
        if current_entity.is_some() {
            if statement == "}" {
                let (name, members, _) = current_entity.take().expect("entity is open");
                classes.push(MermaidClass { name, members });
            } else {
                let tokens: Vec<&str> = statement.split_whitespace().collect();
                if tokens.len() < 2 {
                    return Err(invalid_er_line(line, statement));
                }
                let members = &mut current_entity.as_mut().expect("entity is open").1;
                if members.len() >= limits.max_nodes {
                    return Err(diagnostic(
                        MermaidDiagnosticKind::OversizeGraph,
                        format!("Mermaid attribute limit of {} exceeded", limits.max_nodes),
                        Some(line),
                    ));
                }
                members.push(sanitize_mermaid_label(statement));
            }
            continue;
        }
        if let Some(relation) = parse_er_relation(statement, line)? {
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
                dotted: relation.dotted,
            });
            relations.push(relation);
            continue;
        }
        if let Some(name_part) = statement.strip_suffix('{') {
            let name = name_part.trim();
            if !valid_identifier(name) {
                return Err(invalid_er_line(line, statement));
            }
            upsert_node(
                MermaidNode {
                    id: name.to_owned(),
                    label: name.to_owned(),
                    shape: MermaidNodeShape::Rectangle,
                },
                &mut nodes,
                &mut node_indices,
                limits,
                line,
            )?;
            current_entity = Some((name.to_owned(), Vec::new(), line));
            continue;
        }
        if valid_identifier(statement) {
            upsert_node(
                MermaidNode {
                    id: statement.to_owned(),
                    label: statement.to_owned(),
                    shape: MermaidNodeShape::Rectangle,
                },
                &mut nodes,
                &mut node_indices,
                limits,
                line,
            )?;
            continue;
        }
        return Err(invalid_er_line(line, statement));
    }
    if let Some((_, _, line)) = current_entity {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "Entity block is missing }",
            Some(line),
        ));
    }
    if nodes.is_empty() {
        return Err(diagnostic(
            MermaidDiagnosticKind::InvalidSyntax,
            "ER diagram contains no entities",
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

/// Try to parse `A ||--o{ B : label` as an ER relation. Returns `Ok(Some)` for
/// a valid relation, `Ok(None)` when the statement is not a relation (so the
/// caller can try an entity declaration), and `Err` when it looks like a
/// relation but is malformed.
fn parse_er_relation(
    statement: &str,
    line: usize,
) -> Result<Option<ClassRelation>, MermaidDiagnostic> {
    let tokens: Vec<&str> = statement.split_whitespace().collect();
    if tokens.len() < 3 {
        return Ok(None);
    }
    let Some(dotted) = parse_er_operator(tokens[1]) else {
        return Ok(None);
    };
    let from = tokens[0];
    if !valid_identifier(from) {
        return Ok(None);
    }
    let rest = tokens[2..].join(" ");
    let (b_part, label_text) = match rest.split_once(':') {
        Some((target, label)) => (target, Some(label.trim())),
        None => (rest.as_str(), None),
    };
    let to = b_part.trim();
    if to.is_empty() || to.contains(char::is_whitespace) || !valid_identifier(to) {
        return Err(invalid_er_line(line, statement));
    }
    let label = match label_text {
        Some(text) if !text.is_empty() => Some(single_row_label(text, "relation")?),
        _ => None,
    };
    Ok(Some(ClassRelation {
        from: from.to_owned(),
        to: to.to_owned(),
        label,
        dotted,
        cardinality: Some(tokens[1].to_owned()),
    }))
}

/// Recognize a six-character ER cardinality operator and return whether its
/// connector is dotted. The 18 operators are enumerated explicitly to stay
/// panic-free under non-ASCII input.
fn parse_er_operator(op: &str) -> Option<bool> {
    const SOLID: &[&str] = &[
        "||--||", "||--o{", "||--|{", "}o--||", "}o--o{", "}o--|{", "}|--||", "}|--o{", "}|--|{",
    ];
    const DOTTED: &[&str] = &[
        "||..||", "||..o{", "||..|{", "}o..||", "}o..o{", "}o..|{", "}|..||", "}|..o{", "}|..|{",
    ];
    if SOLID.contains(&op) {
        Some(false)
    } else if DOTTED.contains(&op) {
        Some(true)
    } else {
        None
    }
}

fn render_er_diagram(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let parsed = parse_er_diagram(source, limits)?;
    let mut lines = vec![fit_text("erDiagram", width)];
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
            lines.push(er_relation_line(relation, width));
        }
    }
    // ER diagrams are NOT split: relations cross the whole graph, so an
    // over-budget render keeps the OutputLimit diagnostic and falls back to
    // the bordered source.
    check_output_cells(&lines, limits.max_output_cells)?;
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram: parsed.diagram,
        kind: MermaidDiagramKind::ErDiagram,
    })
}

fn er_relation_line(relation: &ClassRelation, width: usize) -> String {
    let op = relation.cardinality.as_deref().unwrap_or("--");
    let label = relation
        .label
        .as_deref()
        .map(|label| format!(" · {label}"))
        .unwrap_or_default();
    fit_text(
        &format!("{} {} {}{}", relation.from, op, relation.to, label),
        width,
    )
}

fn invalid_er_line(line: usize, statement: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!("Unsupported erDiagram syntax: {}", fit_text(statement, 80)),
        Some(line),
    )
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

impl SequenceArrow {
    fn dotted(self) -> bool {
        matches!(self, Self::DottedClosed | Self::DottedOpen)
    }
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
                    SUPPORTED_DIAGRAMS,
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
    if let Some(rest) =
        keyword_tail(statement, "participant").or_else(|| keyword_tail(statement, "actor"))
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
    if let Some(message) = parse_sequence_message(statement)? {
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
            format!("Mermaid participant limit of {} exceeded", limits.max_nodes),
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
        && !statement[len..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        return None;
    }
    Some(&statement[len..])
}

fn parse_sequence_message(statement: &str) -> Result<Option<SequenceMessage>, MermaidDiagnostic> {
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
    let Some((index, len, arrow)) = best else {
        return Ok(None);
    };
    let from = statement[..index].trim();
    let remainder = &statement[index + len..];
    let (to, label) = remainder
        .split_once(':')
        .map_or((remainder, None), |(to, label)| (to, Some(label.trim())));
    let to = to.trim();
    if !valid_identifier(from) || !valid_identifier(to) {
        return Ok(None);
    }
    let label = label
        .filter(|text| !text.is_empty())
        .map(|text| multi_row_label(text, "message"))
        .transpose()?;
    Ok(Some(SequenceMessage {
        from: from.to_owned(),
        to: to.to_owned(),
        arrow,
        label,
    }))
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
                    dotted: message.arrow.dotted(),
                }),
                SequenceRow::Note(_) | SequenceRow::Plain(_) => None,
            })
            .collect(),
    }
}

/// Sequence diagrams render as one bounded topology canvas. Each participant
/// owns a stable lifeline column, and every message connects those columns.
fn render_sequence_diagram(
    source: &str,
    width: usize,
    limits: MermaidLimits,
) -> Result<MermaidArt, MermaidDiagnostic> {
    let parsed = parse_sequence_diagram(source, limits)?;
    let cell_budget =
        limits.sequence_output_cells(parsed.rows.len(), parsed.participants.len());
    let minimum_lines = 5usize.saturating_add(parsed.rows.len());
    if minimum_lines > cell_budget {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Mermaid output cell limit is too small for this sequence diagram",
            None,
        ));
    }
    let column_widths = sequence_column_widths(&parsed.participants, width)?;
    let lifelines = sequence_lifeline_positions(&column_widths);
    let grid_width = sequence_grid_width(&column_widths);
    let mut lines = vec![fit_text("sequenceDiagram", width)];
    lines.extend(sequence_participant_box_lines(
        &parsed.participants,
        &column_widths,
    ));
    lines.push(sequence_lifeline_line(&lifelines, grid_width));
    for row in &parsed.rows {
        match row {
            SequenceRow::Message(message) => lines.extend(sequence_message_lines(
                message,
                &parsed.participants,
                &lifelines,
                grid_width,
                width,
            )?),
            SequenceRow::Note(body) => {
                lines.push(fit_text(&format!("[note {body}]"), width));
                lines.push(sequence_lifeline_line(&lifelines, grid_width));
            }
            SequenceRow::Plain(text) => {
                lines.push(fit_text(text, width));
                lines.push(sequence_lifeline_line(&lifelines, grid_width));
            }
        }
    }
    check_output_cells(&lines, cell_budget)?;
    Ok(MermaidArt {
        chunks: vec![lines],
        diagram: sequence_to_flowchart(&parsed),
        kind: MermaidDiagramKind::Sequence,
    })
}

const SEQUENCE_COLUMN_GAP: usize = 3;
const SEQUENCE_MIN_COLUMN_WIDTH: usize = 3;

fn sequence_column_widths(
    participants: &[SequenceParticipant],
    width: usize,
) -> Result<Vec<usize>, MermaidDiagnostic> {
    let count = participants.len();
    let fixed = count
        .saturating_mul(2)
        .saturating_add(count.saturating_sub(1).saturating_mul(SEQUENCE_COLUMN_GAP));
    let available = width.saturating_sub(fixed);
    if available < count.saturating_mul(SEQUENCE_MIN_COLUMN_WIDTH) {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Sequence participants cannot fit the requested terminal width",
            None,
        ));
    }

    let mut widths = participants
        .iter()
        .map(|participant| display_width(&participant.label).max(SEQUENCE_MIN_COLUMN_WIDTH))
        .collect::<Vec<_>>();
    while widths.iter().sum::<usize>() > available {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > SEQUENCE_MIN_COLUMN_WIDTH)
            .max_by_key(|(_, value)| **value)
        else {
            break;
        };
        widths[index] -= 1;
    }
    let extra = available.saturating_sub(widths.iter().sum::<usize>());
    let share = extra / count;
    let remainder = extra % count;
    for (index, column_width) in widths.iter_mut().enumerate() {
        *column_width += share + usize::from(index < remainder);
    }
    Ok(widths)
}

fn sequence_grid_width(column_widths: &[usize]) -> usize {
    column_widths.iter().sum::<usize>()
        + column_widths.len().saturating_mul(2)
        + column_widths.len().saturating_sub(1) * SEQUENCE_COLUMN_GAP
}

fn sequence_lifeline_positions(column_widths: &[usize]) -> Vec<usize> {
    let mut positions = Vec::with_capacity(column_widths.len());
    let mut x = 0usize;
    for column_width in column_widths {
        positions.push(x + 1 + column_width / 2);
        x += column_width + 2 + SEQUENCE_COLUMN_GAP;
    }
    positions
}

fn sequence_participant_box_lines(
    participants: &[SequenceParticipant],
    column_widths: &[usize],
) -> Vec<String> {
    let mut top = Vec::with_capacity(participants.len());
    let mut middle = Vec::with_capacity(participants.len());
    let mut bottom = Vec::with_capacity(participants.len());
    for (participant, column_width) in participants.iter().zip(column_widths) {
        top.push(format!("┌{}┐", "─".repeat(*column_width)));
        middle.push(format!(
            "│{}│",
            pad_to_width(&fit_text(&participant.label, *column_width), *column_width)
        ));
        bottom.push(format!("└{}┘", "─".repeat(*column_width)));
    }
    let gap = " ".repeat(SEQUENCE_COLUMN_GAP);
    vec![top.join(&gap), middle.join(&gap), bottom.join(&gap)]
}

fn sequence_lifeline_line(lifelines: &[usize], width: usize) -> String {
    let mut cells = vec![' '; width];
    for position in lifelines {
        cells[*position] = '│';
    }
    cells.into_iter().collect()
}

fn sequence_message_lines(
    message: &SequenceMessage,
    participants: &[SequenceParticipant],
    lifelines: &[usize],
    grid_width: usize,
    width: usize,
) -> Result<Vec<String>, MermaidDiagnostic> {
    let from = participants
        .iter()
        .position(|participant| participant.id == message.from)
        .ok_or_else(|| {
            diagnostic(
                MermaidDiagnosticKind::InvalidSyntax,
                "Unknown sequence source",
                None,
            )
        })?;
    let to = participants
        .iter()
        .position(|participant| participant.id == message.to)
        .ok_or_else(|| {
            diagnostic(
                MermaidDiagnosticKind::InvalidSyntax,
                "Unknown sequence target",
                None,
            )
        })?;
    let label_rows = match &message.label {
        Some(label) => {
            split_label_rows(label).map_err(|()| unrepresentable_label(label, "message"))?
        }
        None => Vec::new(),
    };
    if from == to {
        return sequence_self_message_lines(
            message.arrow,
            &label_rows,
            lifelines,
            from,
            grid_width,
            width,
        );
    }

    let left = lifelines[from].min(lifelines[to]);
    let right = lifelines[from].max(lifelines[to]);
    let mut lines = label_rows
        .iter()
        .map(|label| sequence_label_line(label, left, right, width))
        .collect::<Vec<_>>();
    let mut arrow = sequence_lifeline_line(lifelines, grid_width)
        .chars()
        .collect::<Vec<_>>();
    let fill = sequence_arrow_fill(message.arrow);
    for cell in arrow.iter_mut().take(right).skip(left + 1) {
        *cell = fill;
    }
    if from < to {
        arrow[left] = '├';
        arrow[right] = sequence_arrow_head(message.arrow, true);
    } else {
        arrow[left] = sequence_arrow_head(message.arrow, false);
        arrow[right] = '┤';
    }
    lines.push(arrow.into_iter().collect());
    lines.push(sequence_lifeline_line(lifelines, grid_width));
    Ok(lines)
}

fn sequence_label_line(label: &str, left: usize, right: usize, width: usize) -> String {
    let span = right.saturating_sub(left).saturating_add(1);
    let label = fit_text(label, span.min(width));
    let offset = span.saturating_sub(display_width(&label)) / 2;
    fit_text(&format!("{}{}", " ".repeat(left + offset), label), width)
}

fn sequence_self_message_lines(
    arrow: SequenceArrow,
    labels: &[String],
    lifelines: &[usize],
    participant: usize,
    grid_width: usize,
    width: usize,
) -> Result<Vec<String>, MermaidDiagnostic> {
    let x = lifelines[participant];
    let route_right = grid_width.saturating_sub(x + 1) >= 4;
    let available = if route_right { grid_width - x - 1 } else { x };
    if available < 4 {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            "Sequence self-message cannot fit the requested terminal width",
            None,
        ));
    }
    let loop_width = available.min(12).max(4);
    let (left, right) = if route_right {
        (x, x + loop_width)
    } else {
        (x - loop_width, x)
    };
    let mut lines = labels
        .iter()
        .map(|label| sequence_label_line(label, left, right, width))
        .collect::<Vec<_>>();
    let mut top = sequence_lifeline_line(lifelines, grid_width)
        .chars()
        .collect::<Vec<_>>();
    let mut bottom = top.clone();
    for cell in top.iter_mut().take(right).skip(left + 1) {
        *cell = '─';
    }
    for cell in bottom.iter_mut().take(right).skip(left + 1) {
        *cell = sequence_arrow_fill(arrow);
    }
    if route_right {
        top[left] = '├';
        top[right] = '┐';
        bottom[left] = sequence_arrow_head(arrow, false);
        bottom[right] = '┘';
    } else {
        top[left] = '┌';
        top[right] = '┤';
        bottom[left] = '└';
        bottom[right] = sequence_arrow_head(arrow, true);
    }
    lines.push(top.into_iter().collect());
    lines.push(sequence_lifeline_line(lifelines, grid_width));
    lines.push(bottom.into_iter().collect());
    lines.push(sequence_lifeline_line(lifelines, grid_width));
    Ok(lines)
}

fn sequence_arrow_fill(arrow: SequenceArrow) -> char {
    match arrow {
        SequenceArrow::DottedClosed | SequenceArrow::DottedOpen => '·',
        SequenceArrow::SolidClosed | SequenceArrow::SolidOpen | SequenceArrow::SolidCross => '─',
    }
}

fn sequence_arrow_head(arrow: SequenceArrow, points_right: bool) -> char {
    match (arrow, points_right) {
        (SequenceArrow::SolidClosed | SequenceArrow::DottedClosed, true) => '▶',
        (SequenceArrow::SolidClosed | SequenceArrow::DottedClosed, false) => '◀',
        (SequenceArrow::SolidOpen, _) => '○',
        (SequenceArrow::DottedOpen, _) => '·',
        (SequenceArrow::SolidCross, _) => '✕',
    }
}

fn sanitize_mermaid_label(text: &str) -> String {
    sanitize_inline(text)
        .replace("<br/>", " · ")
        .replace("<br />", " · ")
        .replace("<br>", " · ")
}
/// Split a Mermaid node label into bounded rows at the recognized line-break
/// markers (`<br>`, `<br/>`, `<br />`, case-insensitive). Each segment is
/// sanitized inline and becomes one rendered row; empty segments are kept so
/// consecutive breaks produce blank rows, matching Mermaid's semantics.
///
/// Returns `Err` when the label contains a `<br` prefix that is not one of
/// the supported forms, so the caller can fail visibly to the source fallback
/// instead of emitting literal raw break markup in a "successful" diagram.
/// Literal `<` characters that are not part of a `<br` sequence are preserved
/// as text (e.g. comparison operators in labels).
fn split_label_rows(label: &str) -> Result<Vec<String>, ()> {
    let sanitized = sanitize_inline(label);
    let lower = sanitized.to_ascii_lowercase();
    const MARKERS: &[&str] = &["<br />", "<br/>", "<br>"];
    let mut rows: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut search = 0usize;
    while let Some(relative) = lower[search..].find("<br") {
        let open = search + relative;
        let lower_tail = &lower[open..];
        let matched = MARKERS.iter().find_map(|marker| {
            let lowered = marker.to_ascii_lowercase();
            lower_tail
                .strip_prefix(lowered.as_str())
                .map(|_| marker.len())
        });
        match matched {
            Some(len) => {
                rows.push(sanitized[start..open].to_owned());
                start = open + len;
                search = open + len;
            }
            None => return Err(()),
        }
    }
    rows.push(sanitized[start..].to_owned());
    Ok(rows)
}

/// Reduce a label that must occupy a single rendered row (an edge label or a
/// subgraph title) to its sanitized text, failing visibly when it contains
/// any break markup: those positions cannot represent multiple rows, so the
/// whole diagram falls back to the source instead of joining the rows and
/// silently dropping the `<br>` semantics.
fn single_row_label(label: &str, context: &str) -> Result<String, MermaidDiagnostic> {
    let rows = split_label_rows(label).map_err(|()| unrepresentable_label(label, context))?;
    if rows.len() > 1 {
        return Err(unrepresentable_label(label, context));
    }
    Ok(rows.into_iter().next().unwrap_or_default())
}

fn multi_row_label(label: &str, context: &str) -> Result<String, MermaidDiagnostic> {
    let rows = split_label_rows(label).map_err(|()| unrepresentable_label(label, context))?;
    Ok(rows.join("<br/>"))
}

/// Render rows of an edge label: `<br/>` markers become separate rows. Edge
/// labels previously required a single row; multi-row labels now render the
/// first row bracketed on the connector and the rest beneath it.
fn edge_label_rows(label: &str) -> Result<Vec<String>, MermaidDiagnostic> {
    split_label_rows(label).map_err(|()| unrepresentable_label(label, "edge"))
}

fn unrepresentable_label(label: &str, context: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!(
            "Mermaid {context} label {} contains <br> markup that cannot be rendered as rows",
            fit_text(label, 60)
        ),
        None,
    )
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

fn check_output_cells(lines: &[String], max_cells: usize) -> Result<(), MermaidDiagnostic> {
    let cells = rendered_cells(lines);
    if cells > max_cells {
        return Err(diagnostic(
            MermaidDiagnosticKind::OutputLimit,
            format!("Rendered Mermaid output needs {cells} cells; limit is {max_cells}"),
            None,
        ));
    }
    Ok(())
}

fn invalid_class_line(line: usize, statement: &str) -> MermaidDiagnostic {
    diagnostic(
        MermaidDiagnosticKind::InvalidSyntax,
        format!(
            "Unsupported classDiagram syntax: {}",
            fit_text(statement, 80)
        ),
        Some(line),
    )
}

fn append_node(
    output: &mut Vec<String>,
    node: &MermaidNode,
    width: usize,
) -> Result<(), MermaidDiagnostic> {
    // Normalize Mermaid-supported `<br>`/`<br/>`/`<br />` markers into bounded
    // node label rows. Unsupported `<br...>` markup (or breaks in shapes that
    // cannot render rows) fails visibly so the card falls back to the raw
    // source instead of reporting a malformed diagram as a success.
    let rows =
        split_label_rows(&node.label).map_err(|()| unrepresentable_label(&node.label, "node"))?;
    if width < 5 {
        if rows.len() > 1 {
            return Err(unrepresentable_label(&node.label, "node"));
        }
        output.push(fit_text(&format!("{}:{}", node.id, rows[0]), width));
        return Ok(());
    }
    let title = format!("{} · {}", node.id, rows[0]);
    let mut row_texts: Vec<String> = Vec::with_capacity(rows.len());
    row_texts.push(title);
    row_texts.extend(rows[1..].iter().cloned());
    let multi_row = row_texts.len() > 1;
    if multi_row
        && matches!(
            node.shape,
            MermaidNodeShape::Diamond | MermaidNodeShape::Circle | MermaidNodeShape::Plain
        )
    {
        return Err(unrepresentable_label(&node.label, "node"));
    }
    let inner_width = row_texts
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0)
        .min(width - 2)
        .max(1);
    let content_rows: Vec<String> = row_texts
        .iter()
        .map(|row| pad_to_width(&fit_text(row, inner_width), inner_width))
        .collect();
    match node.shape {
        MermaidNodeShape::Diamond => {
            output.push(fit_text(&format!("◇ {} ◇", content_rows[0]), width))
        }
        MermaidNodeShape::Circle => output.push(fit_text(&format!("({})", content_rows[0]), width)),
        MermaidNodeShape::Rounded => {
            output.push(format!("╭{}╮", "─".repeat(inner_width)));
            for row in &content_rows {
                output.push(format!("│{row}│"));
            }
            output.push(format!("╰{}╯", "─".repeat(inner_width)));
        }
        MermaidNodeShape::Rectangle | MermaidNodeShape::Plain => {
            output.push(format!("┌{}┐", "─".repeat(inner_width)));
            for row in &content_rows {
                output.push(format!("│{row}│"));
            }
            output.push(format!("└{}┘", "─".repeat(inner_width)));
        }
    }
    Ok(())
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
        let (arrow, dotted, label) = cursor
            .edge()
            .ok_or_else(|| invalid_line(line, statement))?;
        let targets = cursor
            .node_group()
            .ok_or_else(|| invalid_line(line, statement))?;
        for node in &targets {
            upsert_node(node.clone(), nodes, node_indices, limits, line)?;
        }
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
                    dotted,
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
        let closing_index = node_shape_closing_index(self.rest, body_start, &closing_text)?;
        let label = unquote_node_label(&self.rest[body_start..closing_index]);
        self.rest = &self.rest[closing_index + closing_text.len()..];
        Some(MermaidNode { id, label, shape })
    }

    /// Parses an `&`-joined list of one or more nodes, matching Mermaid
    /// multi-node statements such as `A & B --> C & D`. Slash-separated
    /// pseudo-groups remain invalid because Mermaid 11.16 rejects them.
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

    /// Parses one edge operator plus optional label. Supported operators:
    /// `-->` (arrow), `---` (open line), `-.->` (dotted), `==>` (thick), each
    /// in its INLINE-label form (`--text-->`, `-.text.->`, `==text==>`, text
    /// optionally quoted) and with a trailing pipe label (`-->|text|`). An
    /// empty inline/pipe label is treated as absent. Returns `None` for
    /// anything else so the caller fails closed.
    fn edge(&mut self) -> Option<(bool, bool, Option<String>)> {
        self.whitespace();
        let (arrow, dotted, label) = if let Some(rest) = self.rest.strip_prefix("-->") {
            (true, false, self.take_pipe_label(rest))
        } else if let Some(rest) = self.rest.strip_prefix("-.->") {
            (true, true, self.take_pipe_label(rest))
        } else if let Some(rest) = self.rest.strip_prefix("==>") {
            (true, false, self.take_pipe_label(rest))
        } else if let Some(rest) = self.rest.strip_prefix("---") {
            (false, false, self.take_pipe_label(rest))
        } else if let Some(rest) = self.rest.strip_prefix("-.") {
            let end = rest.find(".->")?;
            let label = rest[..end].trim();
            self.rest = &rest[end + 3..];
            (true, true, self.inline_label(label))
        } else if let Some(rest) = self.rest.strip_prefix("--") {
            let end = rest.find("-->")?;
            let label = rest[..end].trim();
            self.rest = &rest[end + 3..];
            (true, false, self.inline_label(label))
        } else if let Some(rest) = self.rest.strip_prefix("==") {
            let end = rest.find("==>")?;
            let label = rest[..end].trim();
            self.rest = &rest[end + 4..];
            (true, false, self.inline_label(label))
        } else {
            return None;
        };
        self.whitespace();
        Some((arrow, dotted, label))
    }

    /// Consumes an optional `|label|` tail after an edge operator.
    fn take_pipe_label(&mut self, rest: &'a str) -> Option<String> {
        self.rest = rest;
        self.whitespace();
        let Some(rest) = self.rest.strip_prefix('|') else {
            return None;
        };
        let end = rest.find('|')?;
        let label = rest[..end].trim();
        self.rest = &rest[end + 1..];
        self.inline_label(label)
    }

    /// Unquotes, sanitizes, and drops an empty edge label.
    fn inline_label(&self, label: &str) -> Option<String> {
        let unquoted = unquote_node_label(label);
        let sanitized = sanitize_inline(unquoted.trim());
        (!sanitized.is_empty()).then_some(sanitized)
    }
}
fn node_shape_closing_index(text: &str, body_start: usize, closing: &str) -> Option<usize> {
    let body = &text[body_start..];
    let trimmed = body.trim_start();
    let leading_space = body.len().saturating_sub(trimmed.len());
    let quote = trimmed.chars().next().filter(|ch| matches!(ch, '"' | '\''));
    let Some(quote) = quote else {
        return body.find(closing).map(|index| body_start + index);
    };

    let mut escaped = false;
    for (index, ch) in trimmed[quote.len_utf8()..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch != quote {
            continue;
        }
        let after_quote = quote.len_utf8() + index + ch.len_utf8();
        let trailing = &trimmed[after_quote..];
        let whitespace = trailing.len().saturating_sub(trailing.trim_start().len());
        if trailing[whitespace..].starts_with(closing) {
            return Some(body_start + leading_space + after_quote + whitespace);
        }
    }
    None
}

fn unquote_node_label(body: &str) -> String {
    let trimmed = body.trim();
    let Some(quote) = trimmed.chars().next().filter(|ch| matches!(ch, '"' | '\'')) else {
        return trimmed.to_owned();
    };
    trimmed
        .strip_prefix(quote)
        .and_then(|inner| inner.strip_suffix(quote))
        .unwrap_or(trimmed)
        .trim()
        .to_owned()
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
        (
            "CWD",
            "current working directory resolution and trust checks",
        ),
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

/// Architecture-style fixture shared by the mermaid.rs and render.rs
/// regression suites: mixed CJK/ASCII labels, multiline nodes exercising
/// every Mermaid break form (`<br>`, `<br/>`, `<br />`), and labeled/bare
/// edges crossing two subgraphs. The card-level path is tested against the
/// same input so a literal `<br>` leak is caught at either layer.
#[cfg(test)]
pub(crate) fn arch_flow_source() -> String {
    "\
flowchart TD
subgraph CORE[\"会话核心 · session core\"]
SESS[\"Session<br/>session.rs:3685 run()\"]
AGENT[\"pi-agent Agent<br/>agent.rs / loop_runtime.rs<br/>run_agent_loop\"]
SESS -->|run| AGENT
end
subgraph TOOLS[\"工具 · tool directory\"]
TOOL[\"Tool 目录<br/>tools.rs · execute()\"]
end
AGENT -->|执行 AgentTool| TOOL
PROV[\"pi-ai provider<br/>stream · 流式请求\"]
AGENT --> PROV
"
    .to_owned()
}

/// Exact user-reported sequence source: a 6-participant, 11-message pi-cli
/// pipeline lifeline diagram (see `<repo>/TASKS.md` Mermaid cell-budget
/// batch). Its projection needs 4145 cells at width 117 (2856 at 80, 3422 at
/// 96, ~4260 at 120), which fell back below the former flat 3200 default —
/// the "Rendered Mermaid output needs 4145 cells; limit is 3200" report.
/// Shared so render-level regressions can pin the same input.
#[cfg(test)]
pub(crate) const REPORTED_SEQUENCE_SOURCE: &str = "\
sequenceDiagram
    participant U as 用户(TUI/Web:8765)
    participant F as pi-cli 前端/listen WS
    participant S as Session(pi-coding)
    participant A as Agent循环(pi-agent)
    participant P as Provider(pi-ai)
    participant T as Tools(pi-coding)+沙箱(pi-sandbox)

    U->>F: prompt / WebSocket
    F->>S: 入队(Session queue/compaction检查)
    S->>A: prompt(消息树上下文+工具定义)
    A->>P: stream(ProviderResponse事件流)
    P-->>A: AssistantMessage(tool_calls)
    A->>T: before_tool_call→审批→execute
    T->>T: bash经bwrap/seccomp沙箱执行
    T-->>A: AgentToolResult→ToolResultMessage
    A->>P: 回传结果继续下一轮…
    A-->>S: StopReason::end_turn→Session持久化(JSONL)
    S-->>F: SessionEvent→TUI渲染/Web transcript推送";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{LineRole, MarkdownRenderOptions, RenderDiagnostic, render_markdown};

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
            parsed
                .classes
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
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
        let all = art.chunks.concat().join("\n");
        assert!(text.starts_with("flowchart LR"), "{text}");
        assert!(
            text.contains("subgraph records · SessionRecord types"),
            "{text}"
        );
        assert!(text.contains("end subgraph records"), "{text}");
        assert!(text.contains("A · Session"), "{text}");
        assert!(text.contains("B · Message"), "{text}");
        // width 48 overflows the full rank, so panel 0 holds only A,B; the C/X
        // nodes and the cross-seam edges land in `all` (concat of all panels).
        assert!(all.contains("C · ToolCall"), "{all}");
        assert!(all.contains("X · User"), "{all}");
        assert!(text.contains("▶B"), "{text}");
        assert!(all.contains("boundary: B → … C"), "{all}");
        assert!(all.contains("boundary: X → … A"), "{all}");
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
    fn flowchart_multi_node_renders_attached_branches() {
        let art = render_mermaid_unicode(
            "flowchart LR\nA & B --> C & D",
            48,
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        let text = art.chunks[0].join("\n");
        assert!(!text.lines().any(|line| line == "edges"), "{text}");
        assert_eq!(text.matches("▶C").count(), 1, "{text}");
        assert_eq!(text.matches("▶D").count(), 1, "{text}");
        let a = text.find("│A · A").expect("A node");
        let b = text.find("│B · B").expect("B node");
        let first_c = text.find("▶C").expect("A to C branch");
        let last_d = text.rfind("▶D").expect("B to D branch");
        assert!(
            a < b && b < first_c && first_c < last_d,
            "branches must stay attached: {text}"
        );
    }

    #[test]
    fn small_flowchart_stays_a_connected_single_chunk() {
        let art =
            render_mermaid_unicode("flowchart LR\nA --> B", 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        assert_eq!(art.chunks.len(), 1, "small diagrams must not split");
        assert_eq!(art.chunks[0][0], "flowchart LR");
        let text = art.chunks[0].join("\n");
        assert!(text.contains("│A · A"), "{text}");
        assert!(text.contains("▶B"), "{text}");
        assert!(!text.lines().any(|line| line == "edges"), "{text}");
    }

    #[test]
    fn over_budget_flowchart_splits_into_bounded_connected_chunks() {
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
        assert!(art.chunks.len() >= 2, "{}", art.chunks.len());
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(cells <= limits.max_output_cells, "chunk {index}: {cells}");
            assert!(
                !chunk.iter().any(|line| line == "edges"),
                "chunk {index}: {chunk:?}"
            );
        }
        let text = art.chunks.concat().join("\n");
        for i in 1..=112 {
            assert!(text.contains(&format!("A{i}")), "node A{i} missing: {text}");
        }
        assert_eq!(
            text.matches('▼').count(),
            109,
            "every edge must stay attached: {text}"
        );
        assert_eq!(text.matches("boundary:").count(), 2, "{text}");
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
                ("CWD", 0),
                ("TR", 0),
                ("RM", 0),
                ("AD", 0),
                ("PJ", 0),
                ("SNAP", 0),
                ("SESS", 0),
                ("EXT", 0),
                ("ORCH", 0),
                ("SBX", 0),
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
    fn over_budget_flowchart_many_incoming_edges_keeps_attached_edges_bounded() {
        let mut source = String::from("flowchart TD\n");
        for i in 1..=90 {
            source.push_str(&format!("S{i} --> T\n"));
        }
        let limits = MermaidLimits {
            max_nodes: 128,
            max_edges: 128,
            max_output_cells: 500,
            ..MermaidLimits::default()
        };
        let art = render_mermaid_unicode(&source, 48, limits).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        assert!(art.chunks.len() >= 2, "{}", art.chunks.len());
        for (index, chunk) in art.chunks.iter().enumerate() {
            let cells = rendered_cells(chunk);
            assert!(cells <= limits.max_output_cells, "chunk {index}: {cells}");
            assert!(
                !chunk.iter().any(|line| line == "edges"),
                "chunk {index}: {chunk:?}"
            );
        }
        let text = art.chunks.concat().join("\n");
        for i in 1..=90 {
            assert!(
                text.contains(&format!("S{i}")),
                "source S{i} missing: {text}"
            );
        }
        assert!(text.contains("T · T"), "target T missing: {text}");
        assert_eq!(text.matches("boundary:").count(), 86, "{text}");
        assert!(
            text.contains('▼'),
            "drawn fan-in trunk must use a vertical arrow: {text}"
        );
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
        assert!(
            art.chunks[0]
                .first()
                .is_some_and(|line| line.starts_with("classDiagram"))
        );
        let text = art.chunks.concat().join("\n");
        for i in 1..=190 {
            assert!(
                text.contains(&format!("C{i}")),
                "class C{i} missing: {text}"
            );
        }
        assert!(
            text.contains("──▶"),
            "in-chunk relations must stay full: {text}"
        );
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
            assert!(
                text.contains(&format!("C{i}")),
                "class C{i} missing: {text}"
            );
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
    fn sequence_diagram_renders_boxes_lifelines_and_all_arrows() {
        let art = render_mermaid_unicode(SEQUENCE_SOURCE, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Sequence);
        let text = art.chunks[0].join("\n");
        assert!(text.contains("│Alice"), "{text}");
        assert!(text.contains("Bob"), "{text}");
        for label in ["hello", "ack", "ping", "pong", "quit"] {
            assert!(text.contains(label), "{label}: {text}");
        }
        for head in ['▶', '◀', '○', '·', '✕'] {
            assert!(text.contains(head), "{head}: {text}");
        }
        let lifeline_rows = art.chunks[0]
            .iter()
            .filter(|line| line.chars().filter(|ch| *ch == '│').count() == 2)
            .count();
        assert!(
            lifeline_rows >= 6,
            "lifelines must persist through messages: {text}"
        );
        assert!(
            !text.contains("│ Alice │──▶│"),
            "legacy projection leaked: {text}"
        );
        assert_eq!(art.diagram.nodes.len(), 2);
        assert_eq!(art.diagram.edges.len(), 5);
        assert_eq!(art.diagram.edges[0].label.as_deref(), Some("hello"));
        assert!(art.chunks[0].iter().all(|line| display_width(line) <= 48));
    }

    #[test]
    fn sequence_diagram_auto_creates_undeclared_participants() {
        let parsed =
            parse_sequence_diagram("sequenceDiagram\nX ->> Y: hi", MermaidLimits::default())
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
        assert!(
            matches!(rows[0], SequenceRow::Message(message) if message.arrow == SequenceArrow::SolidClosed)
        );
        assert!(
            matches!(rows[1], SequenceRow::Message(message) if message.arrow == SequenceArrow::DottedClosed)
        );
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks[0].join("\n");
        assert!(text.contains("execute tools"), "{text}");
        assert!(text.contains("ToolResult"), "{text}");
        assert!(text.lines().any(|line| line.contains('▶')), "{text}");
        assert!(text.lines().any(|line| line.contains('◀')), "{text}");
        assert!(
            !text.contains("│ L │──▶│"),
            "legacy projection leaked: {text}"
        );
    }
    #[test]
    fn sequence_cjk_lifelines_and_multiline_messages_render_at_80_and_120() {
        let source = "sequenceDiagram\nparticipant U as 用户\nparticipant A as Application\nU->>A: 提交提示<br/>包含 CJK\nA-->>U: 渲染结果";
        for width in [80usize, 120] {
            let art = render_mermaid_unicode(source, width, MermaidLimits::default()).unwrap();
            let text = art.chunks[0].join("\n");
            for value in ["用户", "Application", "提交提示", "包含 CJK", "渲染结果"] {
                assert!(text.contains(value), "width {width}: {value}: {text}");
            }
            assert!(
                text.lines().any(|line| line.contains('▶')),
                "width {width}: {text}"
            );
            assert!(
                text.lines().any(|line| line.contains('◀')),
                "width {width}: {text}"
            );
            assert!(!text.contains("<br"), "width {width}: {text}");
            assert!(!text.contains("│ 用户 │──▶│"), "width {width}: {text}");
            assert!(
                art.chunks[0]
                    .iter()
                    .all(|line| display_width(line) <= width)
            );
        }
    }

    #[test]
    fn sequence_self_message_renders_loop() {
        let source = "sequenceDiagram\nparticipant A\nA->>A: retry";
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks[0].join("\n");
        assert!(text.contains("retry"), "{text}");
        assert!(text.contains('┐') && text.contains('┘'), "{text}");
        assert!(
            text.contains('◀'),
            "self-message must return to its lifeline: {text}"
        );
    }

    #[test]
    fn sequence_message_malformed_break_fails_closed() {
        let source = "sequenceDiagram\nA->>B: line1<brx>line2";
        let error = render_mermaid_unicode(source, 80, MermaidLimits::default()).unwrap_err();
        assert_eq!(
            error.kind,
            MermaidDiagnosticKind::InvalidSyntax,
            "{error:?}"
        );
        assert!(error.message.contains("message"), "{error:?}");
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
        assert!(art.chunks[0][2].contains("Alice"), "{:?}", art.chunks[0]);
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
    fn user_reported_sequence_renders_sequence_card_across_widths() {
        // The reported 6-participant/11-message source needs 4145 cells at
        // width 117 (plus 2856/3422/~4260 at 80/96/120) — it must now render
        // as one bounded sequenceDiagram card at every ordinary TUI width
        // instead of falling back under the former flat 3200 default.
        for width in [80usize, 96, 117, 120] {
            let art = render_mermaid_unicode(REPORTED_SEQUENCE_SOURCE, width, MermaidLimits::default())
                .unwrap();
            assert_eq!(art.kind, MermaidDiagramKind::Sequence, "width {width}");
            assert_eq!(art.chunks.len(), 1, "width {width}: sequence cards do not split");
            let text = art.chunks[0].join("\n");
            assert!(text.contains("sequenceDiagram"), "width {width}: {text}");
            assert!(text.contains("│"), "width {width}: missing lifeline boxes");
            assert!(text.contains('├') || text.contains('┤'), "width {width}: missing arrows");
            assert!(
                art.chunks
                    .iter()
                    .flatten()
                    .all(|line| display_width(line) <= width),
                "width {width}: unbounded row: {text}"
            );
        }
    }

    #[test]
    fn user_reported_sequence_needs_exactly_4145_cells_at_width_117() {
        // Pins the exact projection cost behind the "needs 4145 cells"
        // report: a ceiling of 4144 must reject the card with that message,
        // and 4145 must accept it exactly.
        let err = render_mermaid_unicode(
            REPORTED_SEQUENCE_SOURCE,
            117,
            MermaidLimits {
                max_sequence_output_cells: 4_144,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OutputLimit);
        assert!(
            err.message.contains("needs 4145 cells; limit is 4144"),
            "{}",
            err.message
        );

        let art = render_mermaid_unicode(
            REPORTED_SEQUENCE_SOURCE,
            117,
            MermaidLimits {
                max_sequence_output_cells: 4_145,
                ..MermaidLimits::default()
            },
        )
        .unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::Sequence);
    }

    #[test]
    fn sequence_budget_slices_grow_with_rows_and_stay_hard_capped() {
        // Slices derive from parsed rows/participants and scale from an
        // explicitly tightened budget, so small budgets keep rejecting small
        // sequence cards (the render-level "sequence output budget" case).
        let limits = MermaidLimits::default();
        assert_eq!(limits.sequence_output_cells(7, 6), 3_200);
        assert_eq!(limits.sequence_output_cells(11, 6), 6_144);
        assert_eq!(limits.sequence_output_cells(1_000, 2), 6_144);
        let tight = MermaidLimits {
            max_output_cells: 5,
            ..MermaidLimits::default()
        };
        assert_eq!(tight.sequence_output_cells(3, 2), 5);
        let err = render_mermaid_unicode(
            SEQUENCE_SOURCE,
            48,
            MermaidLimits {
                max_sequence_output_cells: 4,
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
        assert!(text.contains("prompt(text)"), "{text}");
        assert!(text.contains("run(prompt)"), "{text}");
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

    #[test]
    fn flowchart_quoted_label_with_nested_closer_renders_at_80_and_120() {
        let source = "flowchart LR\nM[\"message<br/>id, parentId, timestamp,<br/>message{role, content[{type:text,text}]}\" ] --> N[下一步]";
        for width in [80usize, 120] {
            let chart = parse_mermaid(source, MermaidLimits::default()).unwrap();
            let message = chart.nodes.iter().find(|node| node.id == "M").unwrap();
            assert_eq!(
                message.label,
                "message<br/>id, parentId, timestamp,<br/>message{role, content[{type:text,text}]}"
            );
            let art = render_mermaid_unicode(source, width, MermaidLimits::default()).unwrap();
            let text = art.chunks.concat().join("\n");
            for row in [
                "M · message",
                "id, parentId, timestamp,",
                "message{role, content[{type:text,text}]}",
                "▶N",
                "下一步",
            ] {
                assert!(text.contains(row), "width {width}: {row}: {text}");
            }
            assert!(!text.contains("<br"), "width {width}: {text}");
            assert!(
                !text.lines().any(|line| line == "edges"),
                "width {width}: {text}"
            );
            assert!(
                art.chunks
                    .iter()
                    .flatten()
                    .all(|line| display_width(line) <= width)
            );
        }
    }

    #[test]
    fn connected_cjk_flowchart_renders_at_80_and_120() {
        let source = "flowchart LR\nA[启动任务] --> B{检查状态}\nB -->|完成| C[结束]\nB -->|重试| D[继续处理]";
        for width in [80usize, 120] {
            let art = render_mermaid_unicode(source, width, MermaidLimits::default()).unwrap();
            let text = art.chunks.concat().join("\n");
            assert!(text.contains("A · 启动任务"), "width {width}: {text}");
            assert!(text.contains("▶ B"), "width {width}: {text}");
            assert!(
                text.contains("[完成]") && text.contains("▶C"),
                "width {width}: {text}"
            );
            assert!(
                text.contains("[重试]") && text.contains("▶D"),
                "width {width}: {text}"
            );
            assert!(text.contains("继续处理"), "width {width}: {text}");
            assert!(
                !text.lines().any(|line| line == "edges"),
                "width {width}: {text}"
            );
            assert!(
                art.chunks
                    .iter()
                    .flatten()
                    .all(|line| display_width(line) <= width)
            );
        }
    }

    #[test]
    fn architecture_multiline_flowchart_renders_rows_without_literal_breaks() {
        let source = arch_flow_source();
        let art = render_mermaid_unicode(&source, 64, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("architecture fixture must render: {err:?}"));
        assert_eq!(art.kind, MermaidDiagramKind::Flowchart);
        let text = art.chunks.concat().join("\n");
        // No literal raw break markup leaks into a successful render.
        assert!(!text.contains("<br"), "literal <br> markup leaked: {text}");
        assert!(!text.contains("source fallback"), "{text}");
        // Subgraph markers survive (no breaks in titles).
        assert!(
            text.contains("subgraph CORE · 会话核心 · session core"),
            "{text}"
        );
        assert!(text.contains("end subgraph CORE"), "{text}");
        assert!(
            text.contains("subgraph TOOLS · 工具 · tool directory"),
            "{text}"
        );
        assert!(text.contains("end subgraph TOOLS"), "{text}");
        // Multi-row node labels land on separate box rows.
        let lines: Vec<&str> = text.lines().collect();
        let sess_header = lines
            .iter()
            .find(|line| line.contains("SESS · Session"))
            .unwrap_or_else(|| panic!("SESS header row missing: {text}"));
        let sess_second = lines
            .iter()
            .find(|line| line.contains("session.rs:3685 run()"))
            .unwrap_or_else(|| panic!("SESS second row missing: {text}"));
        assert_ne!(
            sess_header, sess_second,
            "multiline label must occupy separate rows: {text}"
        );
        assert!(
            sess_header.contains('│') && sess_second.contains('│'),
            "{text}"
        );
        let agent_rows = [
            "AGENT · pi-agent Agent",
            "agent.rs / loop_runtime.rs",
            "run_agent_loop",
        ];
        let mut seen = Vec::new();
        for expected in agent_rows {
            let row = lines
                .iter()
                .find(|line| line.contains(expected))
                .unwrap_or_else(|| panic!("AGENT row {expected:?} missing: {text}"));
            seen.push(row);
        }
        assert_eq!(
            seen.len(),
            3,
            "three-row AGENT label must each be on its own line: {text}"
        );
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "AGENT rows must be distinct lines: {text}"
        );
        // CJK row content preserved.
        assert!(text.contains("TOOL · Tool 目录"), "{text}");
        assert!(text.contains("tools.rs · execute()"), "{text}");
        assert!(text.contains("PROV · pi-ai provider"), "{text}");
        assert!(text.contains("stream · 流式请求"), "{text}");
        // Labeled and bare edges render with the connector form: labels are
        // side-placed on connector rows and the routed arrows are vertical (▼).
        assert!(text.contains("[run]"), "{text}");
        assert!(text.contains("[执行 AgentTool]"), "{text}");
        assert!(
            text.contains('▼'),
            "vertical architecture edges draw ▼ arrows: {text}"
        );
    }

    #[test]
    fn flowchart_node_label_break_forms_render_as_rows() {
        // Every supported break form (`<br>`, `<br/>`, `<br />`) and a
        // case-insensitive variant splits a rectangle node into bounded rows;
        // consecutive breaks produce a blank row, matching Mermaid semantics.
        let source = "flowchart TD\nA[\"alpha<br/>beta<br>gamma<br />delta<BR>ε\"]";
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks.concat().join("\n");
        assert!(!text.contains("<br"), "no literal break markup: {text}");
        assert!(text.contains("A · alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("gamma"), "{text}");
        assert!(text.contains("delta"), "{text}");
        assert!(text.contains("ε"), "{text}");
        // Each row segment lives on its own box line.
        let lines: Vec<&str> = text.lines().collect();
        for (a, b) in [("alpha", "beta"), ("beta", "gamma"), ("gamma", "delta")] {
            let la = lines.iter().find(|l| l.contains(a)).unwrap();
            let lb = lines.iter().find(|l| l.contains(b)).unwrap();
            assert_ne!(la, lb, "{a}/{b} must be on separate rows: {text}");
        }

        // Consecutive breaks produce a blank padded row.
        let source = "flowchart TD\nB[\"x<br><br>y\"]";
        let art = render_mermaid_unicode(source, 24, MermaidLimits::default()).unwrap();
        let text = art.chunks.concat().join("\n");
        assert!(!text.contains("<br"), "{text}");
        // The blank row is the box content line between the x and y rows:
        // `│B · x │`, `│      │`, `│y     │`.
        let blank = text
            .lines()
            .filter(|line| line.starts_with("│") && line.ends_with("│"))
            .find(|line| {
                let inner = line
                    .strip_prefix('│')
                    .and_then(|rest| rest.strip_suffix('│'));
                inner.is_some_and(|rest| rest.trim().is_empty())
            });
        assert!(
            blank.is_some(),
            "expected a blank padded row between breaks: {text}"
        );
    }

    #[test]
    fn flowchart_rounded_node_renders_multiline_rows() {
        let source = "flowchart TD\nA(\"one<br/>two\")";
        let art = render_mermaid_unicode(source, 32, MermaidLimits::default()).unwrap();
        let text = art.chunks.concat().join("\n");
        assert!(!text.contains("<br"), "{text}");
        assert!(text.contains("A · one"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("╭"), "{text}");
        assert!(text.contains("╰"), "{text}");
    }

    #[test]
    fn flowchart_break_in_unsupported_shape_falls_back_to_source() {
        // Circle and diamond shapes have no row separators, so a `<br>` label
        // cannot be represented as rows — fail visibly to the source fallback
        // instead of joining or emitting literal markup.
        for source in [
            "flowchart TD\nA((\"one<br/>two\"))",
            "flowchart TD\nA{\"one<br/>two\"}",
        ] {
            let err = render_mermaid_unicode(source, 48, MermaidLimits::default())
                .expect_err(&format!("expected visible failure: {source}"));
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{source}: {err:?}"
            );
            assert!(err.message.contains("<br>"), "{source}: {err:?}");
        }
    }

    #[test]
    fn flowchart_unsupported_break_markup_falls_back_to_source() {
        // `<br` prefixes that are not one of the three supported forms cannot
        // be normalized into rows, so the whole diagram fails visibly rather
        // than emit literal raw markup as a "successful" render.
        for source in [
            "flowchart TD\nA[\"x<br foo>y\"]",
            "flowchart TD\nA[\"x<br\">y\"]",
            "flowchart TD\nA[\"x<br",
        ] {
            let err = render_mermaid_unicode(source, 48, MermaidLimits::default())
                .expect_err(&format!("expected visible failure: {source}"));
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{source}: {err:?}"
            );
        }
    }

    #[test]
    fn flowchart_break_in_edge_label_renders_rows() {
        // Edge labels support multi-row rendering: a `<br>` in an edge label
        // becomes separate label rows instead of failing the whole diagram.
        // (Formerly this failed closed; the renderer now draws bracketed
        // rows, so only genuinely unrenderable markup still fails.)
        let source = "flowchart TD\nA -->|\"line1<br/>line2\"| B";
        let art = render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("edge label break must render: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("[line1]"), "{text}");
        assert!(text.contains("line2"), "{text}");
        assert!(!text.contains("<br"), "{text}");
    }

    #[test]
    fn flowchart_break_in_subgraph_title_falls_back_to_source() {
        let source = "flowchart TD\nsubgraph S[\"title<br/>line2\"]\nA[\"x\"]\nend";
        let err = render_mermaid_unicode(source, 48, MermaidLimits::default())
            .expect_err("subgraph title break must fail visibly");
        assert_eq!(err.kind, MermaidDiagnosticKind::InvalidSyntax, "{err:?}");
        assert!(err.message.contains("subgraph"), "{err:?}");
    }

    #[test]
    fn flowchart_literal_angle_bracket_is_preserved() {
        // A literal `<` that is not part of a `<br` sequence (e.g. a
        // comparison operator) is preserved as text and does not trip the
        // break-markup guard.
        let source = "flowchart TD\nA[\"if x < 5 then ok\"]";
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("if x < 5 then ok"), "{text}");
        assert!(!text.contains("<br"), "{text}");
    }

    #[test]
    fn flowchart_multiline_label_honors_cjk_width_bounding() {
        // CJK rows are 2 cells per grapheme; the box inner width is the max
        // row width, and every row is padded to it so the box borders align
        // even when ASCII and CJK rows are mixed.
        let source = "flowchart TD\nA[\"短<br/>longer ascii row\"]";
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks.concat().join("\n");
        let rows: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("│") && line.ends_with("│"))
            .collect();
        assert!(
            rows.len() >= 2,
            "expected at least two content rows: {text}"
        );
        // All content rows share the same cell width (borders align).
        let widths: Vec<usize> = rows.iter().map(|line| display_width(line)).collect();
        let first = widths[0];
        assert!(
            widths.iter().all(|w| *w == first),
            "box content rows must align: {widths:?}\n{text}"
        );
        // The CJK row is present and not corrupted.
        assert!(text.contains("A · 短"), "{text}");
        assert!(text.contains("longer ascii row"), "{text}");
    }

    #[test]
    fn architecture_fixture_stays_bounded_at_narrow_widths() {
        for width in [20usize, 28, 40, 64, 96] {
            let art = render_mermaid_unicode(&arch_flow_source(), width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must render: {err:?}"));
            assert_eq!(art.kind, MermaidDiagramKind::Flowchart, "width {width}");
            for (index, chunk) in art.chunks.iter().enumerate() {
                let cells = rendered_cells(chunk);
                assert!(
                    cells <= MermaidLimits::default().max_output_cells,
                    "width {width}: chunk {index} exceeds budget: {cells} cells"
                );
                assert!(
                    chunk.iter().all(|line| display_width(line) <= width),
                    "width {width}: {chunk:?}"
                );
            }
            let text = art.chunks.concat().join("\n");
            assert!(!text.contains("<br"), "width {width}: literal break markup: {text}");
            for label in ["Session", "Tool", "pi-ai"] {
                assert!(text.contains(label), "width {width}: label {label} missing: {text}");
            }
            assert!(
                text.contains("pi-agent") || text.contains("pi-age"),
                "width {width}: agent label missing: {text}"
            );
        }
    }

    const STATE_SOURCE: &str = "\
stateDiagram-v2
direction LR
[*] --> Idle: 初始化完成
state \"长描述\" as LongName
Idle --> Running: start
Running --> Done: finish
Done --> [*]
state Choice <<choice>>
Idle --> Choice: branch
Choice --> Running: retry
Choice --> Done: abort
";

    #[test]
    fn state_diagram_parses_direction_endpoints_transitions_and_choice() {
        let chart = parse_mermaid(STATE_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(chart.direction, FlowDirection::LeftRight);
        let ids: Vec<&str> = chart.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"[*]start"), "{ids:?}");
        assert!(ids.contains(&"[*]end"), "{ids:?}");
        assert!(ids.contains(&"Idle"), "{ids:?}");
        assert!(ids.contains(&"Running"), "{ids:?}");
        assert!(ids.contains(&"Done"), "{ids:?}");
        assert!(ids.contains(&"Choice"), "{ids:?}");
        assert!(ids.contains(&"LongName"), "{ids:?}");
        let star = chart.nodes.iter().find(|n| n.id == "[*]start").unwrap();
        assert_eq!(star.shape, MermaidNodeShape::Circle);
        assert_eq!(star.label, "●");
        let choice = chart.nodes.iter().find(|n| n.id == "Choice").unwrap();
        assert_eq!(choice.shape, MermaidNodeShape::Diamond);
        let long_name = chart.nodes.iter().find(|n| n.id == "LongName").unwrap();
        assert_eq!(long_name.label, "长描述");
        assert!(
            chart
                .edges
                .iter()
                .any(|e| e.from == "[*]start" && e.to == "Idle"),
            "{:?}",
            chart.edges
        );
        assert!(
            chart
                .edges
                .iter()
                .any(|e| e.from == "Done" && e.to == "[*]end"),
            "{:?}",
            chart.edges
        );
        let start = chart
            .edges
            .iter()
            .find(|e| e.from == "Idle" && e.to == "Running")
            .unwrap();
        assert_eq!(start.label.as_deref(), Some("start"));
        assert!(
            chart
                .edges
                .iter()
                .any(|e| e.from == "Choice" && e.to == "Done"),
            "{:?}",
            chart.edges
        );
    }

    #[test]
    fn state_diagram_renders_cjk_labels_and_endpoints() {
        let art = render_mermaid_unicode(STATE_SOURCE, 80, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::StateDiagram);
        assert_eq!(art.chunks.len(), 1);
        let text = art.chunks[0].join("\n");
        assert!(text.starts_with("stateDiagram LR"), "{text}");
        for value in ["初始化完成", "Idle", "Running", "Done", "长描述"] {
            assert!(text.contains(value), "missing {value:?}: {text}");
        }
        assert!(text.contains('●'), "compact endpoint missing: {text}");
        assert!(text.contains('▼'), "connected transition missing: {text}");
        assert!(!text.lines().any(|line| line == "edges"), "{text}");
        assert!(!text.contains("<br"), "{text}");
    }

    #[test]
    fn state_diagram_reported_inline_labeled_target_renders() {
        let source =
            "stateDiagram\nstate Overlap\nOverlap --> Err[\"is_error=true<br/>返回错误文案\"]";
        let chart = parse_mermaid(source, MermaidLimits::default()).unwrap();
        let error = chart.nodes.iter().find(|node| node.id == "Err").unwrap();
        assert_eq!(error.label, "is_error=true<br/>返回错误文案");
        assert_eq!(error.shape, MermaidNodeShape::Rectangle);
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::StateDiagram);
        let text = art.chunks[0].join("\n");
        assert!(text.contains("Err · is_error=true"), "{text}");
        assert!(text.contains("返回错误文案"), "{text}");
        assert!(text.contains('▼'), "connected transition missing: {text}");
        assert!(!text.lines().any(|line| line == "edges"), "{text}");
        assert!(!text.contains("<br"), "{text}");
        assert!(art.chunks[0].iter().all(|line| display_width(line) <= 48));

        let markdown = format!("```mermaid\n{source}\n```");
        let output = render_markdown(
            &markdown,
            &MarkdownRenderOptions {
                width: 52,
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(!output.plain_text().contains("source fallback"));
    }

    #[test]
    fn state_diagram_chained_transitions_parse() {
        let chart = parse_mermaid(
            "stateDiagram\nA --> B --> C --> D",
            MermaidLimits::default(),
        )
        .unwrap();
        let pairs: Vec<(&str, &str)> = chart
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(pairs, vec![("A", "B"), ("B", "C"), ("C", "D")]);
    }

    #[test]
    fn state_diagram_rejects_endpoint_in_middle_of_chain() {
        let block = "stateDiagram\nA --> [*] --> B";
        let error = parse_mermaid(block, MermaidLimits::default()).unwrap_err();
        assert_eq!(
            error.kind,
            MermaidDiagnosticKind::InvalidSyntax,
            "{error:?}"
        );

        let source = format!("```mermaid\n{block}\n```");
        let output = render_markdown(
            &source,
            &MarkdownRenderOptions {
                width: 48,
                ..MarkdownRenderOptions::default()
            },
        );
        let text = output.plain_text();
        assert_eq!(
            text.matches("mermaid · source fallback").count(),
            1,
            "{text}"
        );
        assert_eq!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::MermaidBorder && line.text.starts_with("└─"))
                .count(),
            0,
            "{text}"
        );
        assert!(text.contains("mermaid · source fallback"), "{text}");
        assert!(text.contains("A --> [*] --> B"), "{text}");
        assert!(text.contains("! mermaid:"), "{text}");
        assert!(
            output.lines.iter().any(|line| {
                line.role == LineRole::Code && !line.text.trim_matches(['│', ' ']).is_empty()
            }),
            "{text}"
        );
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid { diagnostic, .. }]
                if diagnostic.kind == MermaidDiagnosticKind::InvalidSyntax
        ));
    }

    #[test]
    fn state_diagram_rejects_transition_label_before_chain_end() {
        let error = parse_mermaid(
            "stateDiagram\nA --> B: stop --> C",
            MermaidLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            MermaidDiagnosticKind::InvalidSyntax,
            "{error:?}"
        );
    }

    #[test]
    fn state_diagram_composite_block_is_flattened() {
        let source = "stateDiagram\nstate Group {\nA --> B\nB --> C\n}\nC --> [*]";
        let chart = parse_mermaid(source, MermaidLimits::default()).unwrap();
        let ids: Vec<&str> = chart.nodes.iter().map(|n| n.id.as_str()).collect();
        // The composite state and its inner states are all top-level nodes.
        assert!(ids.contains(&"Group"), "{ids:?}");
        assert!(ids.contains(&"A"), "{ids:?}");
        assert!(ids.contains(&"B"), "{ids:?}");
        assert!(ids.contains(&"C"), "{ids:?}");
        assert!(ids.contains(&"[*]end"), "{ids:?}");
        let pairs: Vec<(&str, &str)> = chart
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(pairs, vec![("A", "B"), ("B", "C"), ("C", "[*]end")]);
    }

    #[test]
    fn state_diagram_rejects_invalid_statements() {
        for source in [
            "stateDiagram\nnote left of A: hi",
            "stateDiagram\nIdle -> Running",
            "stateDiagram\nstate \"no close",
            "stateDiagram\nstate Bad <<unknown>>",
            "stateDiagram\n}",
            "stateDiagram\nstate Group {\nA --> B",
        ] {
            let err = parse_mermaid(source, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{source}: {err:?}"
            );
        }
    }

    #[test]
    fn state_diagram_transition_label_break_renders_at_80_and_120() {
        let source = "stateDiagram-v2\n[*] --> Execute\nExecute --> AppendResult: toolResultMessage<br/>(含 is_error)\nAppendResult --> [*]";
        for width in [80usize, 120] {
            let art = render_mermaid_unicode(source, width, MermaidLimits::default()).unwrap();
            assert_eq!(art.kind, MermaidDiagramKind::StateDiagram);
            let text = art.chunks[0].join("\n");
            for row in ["toolResultMessage", "(含 is_error)"] {
                assert!(text.contains(row), "width {width}: {text}");
            }
            assert!(text.contains('▼'), "width {width}: {text}");
            assert!(!text.lines().any(|line| line == "edges"), "width {width}: {text}");
            assert!(!text.contains("<br"), "width {width}: {text}");
            assert!(art.chunks[0].iter().all(|line| display_width(line) <= width));
        }
    }

    #[test]
    fn state_diagram_cycle_uses_layered_two_dimensional_layout() {
        let source = "stateDiagram-v2\n[*] --> Idle\nIdle --> Running: start\nRunning --> Paused: pause\nPaused --> Running: resume\nRunning --> Done: finish\nDone --> [*]";
        let art = render_mermaid_unicode(source, 80, MermaidLimits::default()).unwrap();
        let text = art.chunks[0].join("\n");
        assert!(!text.lines().any(|line| line == "edges"), "{text}");
        assert!(text.contains('▼'), "forward transitions must connect ranks: {text}");
        assert!(text.contains('▲'), "cycle back-edge must route upward: {text}");
        assert!(
            text.lines().any(|line| line.contains("Running") && line.contains("Paused")),
            "cycle states must share a rank instead of a vertical stack: {text}"
        );
        for label in ["start", "pause", "resume"] {
            assert!(text.contains(label), "missing {label:?}: {text}");
        }
    }
    #[test]
    fn state_diagram_transition_malformed_break_fails_closed() {
        let source = "stateDiagram\nA --> B: line1<brx>line2";
        let error = render_mermaid_unicode(source, 80, MermaidLimits::default()).unwrap_err();
        assert_eq!(
            error.kind,
            MermaidDiagnosticKind::InvalidSyntax,
            "{error:?}"
        );
        assert!(error.message.contains("transition"), "{error:?}");
    }

    #[test]
    fn state_diagram_enforces_node_and_edge_limits() {
        let err = parse_mermaid(
            "stateDiagram\nA --> B\nB --> C\nC --> D",
            MermaidLimits {
                max_nodes: 2,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("node limit"), "{}", err.message);

        let err = parse_mermaid(
            "stateDiagram\nA --> B\nC --> D\nE --> F",
            MermaidLimits {
                max_edges: 2,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("edge limit"), "{}", err.message);
    }

    #[test]
    fn state_diagram_over_budget_returns_output_limit_without_split() {
        // A large state graph must NOT split into panels; it keeps the
        // OutputLimit diagnostic so the card falls back to the bordered source.
        let mut source = String::from("stateDiagram\n");
        for i in 1..400 {
            source.push_str(&format!("S{i} --> S{}\n", i + 1));
        }
        let limits = MermaidLimits {
            max_nodes: 1024,
            max_edges: 1024,
            ..MermaidLimits::default()
        };
        let err = render_mermaid_unicode(&source, 48, limits).unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OutputLimit);
        assert!(err.message.contains("needs"), "{}", err.message);
    }

    #[test]
    fn state_diagram_width_sweep_stays_bounded_and_break_free() {
        for width in [12usize, 24, 48, 80, 120] {
            let art = render_mermaid_unicode(STATE_SOURCE, width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must render: {err:?}"));
            assert_eq!(art.kind, MermaidDiagramKind::StateDiagram, "width {width}");
            assert_eq!(art.chunks.len(), 1, "state diagrams never split");
            let cells = rendered_cells(&art.chunks[0]);
            assert!(
                cells <= MermaidLimits::default().max_output_cells,
                "width {width}: {cells} cells over budget"
            );
            let text = art.chunks[0].join("\n");
            assert!(
                !text.contains("<br"),
                "width {width}: break markup leaked: {text}"
            );
            assert!(text.contains("Idle"), "width {width}: Idle missing");
            if width >= 24 {
                assert!(
                    text.contains("初始化完成"),
                    "width {width}: CJK label missing"
                );
            }
        }
    }

    const ER_SOURCE: &str = "\
erDiagram
CUSTOMER ||--o{ ORDER : 下单
CUSTOMER {
string name 名称
int id 标识
}
ORDER {
int id 订单标识
string total
}
ORDER ||--|| LINEITEM : contains
LINEITEM {
int qty 数量
}
";

    #[test]
    fn er_diagram_parses_entities_attributes_and_relations() {
        let chart = parse_mermaid(ER_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(chart.direction, FlowDirection::LeftRight);
        let ids: Vec<&str> = chart.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["CUSTOMER", "ORDER", "LINEITEM"]);
        assert_eq!(chart.edges.len(), 2);
        assert_eq!(chart.edges[0].from, "CUSTOMER");
        assert_eq!(chart.edges[0].to, "ORDER");
        assert_eq!(chart.edges[0].label.as_deref(), Some("下单"));
        assert_eq!(chart.edges[1].from, "ORDER");
        assert_eq!(chart.edges[1].to, "LINEITEM");
        assert_eq!(chart.edges[1].label.as_deref(), Some("contains"));
        // Cardinality operators are captured in the projection via the parsed
        // relations (parsed separately, since MermaidFlowchart edges don't carry
        // the operator).
        let parsed = parse_er_diagram(ER_SOURCE, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.relations[0].cardinality.as_deref(), Some("||--o{"));
        assert!(!parsed.relations[0].dotted);
        assert_eq!(parsed.relations[1].cardinality.as_deref(), Some("||--||"));
        assert!(!parsed.relations[1].dotted);
        let customer = parsed
            .classes
            .iter()
            .find(|c| c.name == "CUSTOMER")
            .unwrap();
        assert_eq!(customer.members.len(), 2);
        assert!(
            customer.members[0].contains("名称"),
            "{:?}",
            customer.members
        );
        assert!(
            customer.members[1].contains("标识"),
            "{:?}",
            customer.members
        );
    }

    #[test]
    fn er_diagram_renders_cjk_attributes_and_cardinalities() {
        let art = render_mermaid_unicode(ER_SOURCE, 48, MermaidLimits::default()).unwrap();
        assert_eq!(art.kind, MermaidDiagramKind::ErDiagram);
        assert_eq!(art.chunks.len(), 1);
        let text = art.chunks[0].join("\n");
        assert!(text.starts_with("erDiagram"), "{text}");
        // CJK attribute comments and relation labels survive verbatim.
        assert!(text.contains("名称"), "{text}");
        assert!(text.contains("标识"), "{text}");
        assert!(text.contains("下单"), "{text}");
        assert!(text.contains("数量"), "{text}");
        // Cardinality operators render in the relation line.
        assert!(text.contains("CUSTOMER ||--o{ ORDER"), "{text}");
        assert!(text.contains("ORDER ||--|| LINEITEM"), "{text}");
        // No literal break markup.
        assert!(!text.contains("<br"), "{text}");
    }

    #[test]
    fn er_diagram_dotted_cardinality_and_aliases() {
        let source = "erDiagram\nA }o..o{ B : optional\nB ||..|| C";
        let parsed = parse_er_diagram(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.relations[0].cardinality.as_deref(), Some("}o..o{"));
        assert!(parsed.relations[0].dotted);
        assert_eq!(parsed.relations[0].label.as_deref(), Some("optional"));
        assert_eq!(parsed.relations[1].cardinality.as_deref(), Some("||..||"));
        assert!(parsed.relations[1].dotted);
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks[0].join("\n");
        assert!(text.contains("A }o..o{ B · optional"), "{text}");
        assert!(text.contains("B ||..|| C"), "{text}");
    }

    #[test]
    fn er_diagram_bare_and_implicitly_declared_entities() {
        // A relation implicitly declares both entities; a bare entity
        // declaration adds an empty box.
        let source = "erDiagram\nA ||--o{ B : has\nEXTRA";
        let parsed = parse_er_diagram(source, MermaidLimits::default()).unwrap();
        let ids: Vec<&str> = parsed.diagram.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["A", "B", "EXTRA"]);
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap();
        let text = art.chunks[0].join("\n");
        assert!(text.contains("A ||--o{ B · has"), "{text}");
        assert!(text.contains("EXTRA"), "{text}");
    }

    #[test]
    fn er_diagram_rejects_invalid_statements() {
        for source in [
            "erDiagram\nA ||-- B : bad",
            "erDiagram\nA ||--o{",
            "erDiagram\nCUSTOMER {\nname\n}",
            "erDiagram\nCUSTOMER {\nint id\n",
            "erDiagram\n}",
        ] {
            let err = parse_mermaid(source, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{source}: {err:?}"
            );
        }
    }

    #[test]
    fn er_diagram_relation_label_break_falls_back() {
        let source = "erDiagram\nA ||--o{ B : line1<br/>line2";
        let err = render_mermaid_unicode(source, 48, MermaidLimits::default()).unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::InvalidSyntax, "{err:?}");
        assert!(err.message.contains("relation"), "{err:?}");
    }

    #[test]
    fn er_diagram_enforces_edge_and_attribute_limits() {
        let err = parse_mermaid(
            "erDiagram\nA ||--o{ B\nC ||--o{ D",
            MermaidLimits {
                max_edges: 1,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("edge limit"), "{}", err.message);

        let mut block = String::from("erDiagram\nE {\n");
        for i in 0..70 {
            block.push_str(&format!("int a{i}\n"));
        }
        block.push_str("}\n");
        let err = parse_mermaid(
            &block,
            MermaidLimits {
                max_nodes: 64,
                ..MermaidLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OversizeGraph);
        assert!(err.message.contains("attribute limit"), "{}", err.message);
    }

    #[test]
    fn er_diagram_over_budget_returns_output_limit_without_split() {
        let mut source = String::from("erDiagram\n");
        for i in 1..400 {
            source.push_str(&format!("E{i} {{\nint a\n}}\n"));
            if i < 399 {
                source.push_str(&format!("E{i} ||--o{{ E{}\n", i + 1));
            }
        }
        let limits = MermaidLimits {
            max_nodes: 1024,
            max_edges: 1024,
            ..MermaidLimits::default()
        };
        let err = render_mermaid_unicode(&source, 48, limits).unwrap_err();
        assert_eq!(err.kind, MermaidDiagnosticKind::OutputLimit);
        assert!(err.message.contains("needs"), "{}", err.message);
    }

    #[test]
    fn er_diagram_width_sweep_stays_bounded_and_break_free() {
        for width in [16usize, 24, 48, 80, 120] {
            let art = render_mermaid_unicode(ER_SOURCE, width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must render: {err:?}"));
            assert_eq!(art.kind, MermaidDiagramKind::ErDiagram, "width {width}");
            assert_eq!(art.chunks.len(), 1, "ER diagrams never split");
            let cells = rendered_cells(&art.chunks[0]);
            assert!(
                cells <= MermaidLimits::default().max_output_cells,
                "width {width}: {cells} cells over budget"
            );
            let text = art.chunks[0].join("\n");
            assert!(
                !text.contains("<br"),
                "width {width}: break markup leaked: {text}"
            );
            assert!(text.contains("CUSTOMER"), "width {width}");
            if width >= 48 {
                assert!(
                    text.contains("下单"),
                    "width {width}: CJK relation label missing"
                );
                assert!(
                    text.contains("名称"),
                    "width {width}: CJK attribute missing"
                );
            }
        }
    }

    /// Exact user-reported edge: a pipe label carrying `<br/>` must render as
    /// separate label rows, not fall back to the source card.
    #[test]
    fn edge_pipe_label_br_renders_as_rows() {
        let source =
            "flowchart LR\nCAT -->|\"list/latest/prune<br/>session_store.rs:1963-2153\"| UI";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.edges.len(), 1);
        assert_eq!(parsed.edges[0].from, "CAT");
        assert_eq!(parsed.edges[0].to, "UI");
        let art = render_mermaid_unicode(source, 80, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render without fallback: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("list/latest/prune"), "{text}");
        assert!(text.contains("session_store.rs:1963-2153"), "{text}");
        assert!(!text.contains("<br"), "break markup leaked: {text}");
    }

    /// Exact user-reported dotted arrows with INLINE unquoted labels.
    #[test]
    fn dotted_arrow_inline_labels_parse_and_render() {
        let source = "flowchart TD\nT -.tentative.-> H\nE -.仅 Ask 升级.-> T";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.edges.len(), 2, "{parsed:?}");
        assert_eq!(
            (parsed.edges[0].from.as_str(), parsed.edges[0].to.as_str()),
            ("T", "H")
        );
        assert_eq!(parsed.edges[0].label.as_deref(), Some("tentative"));
        assert_eq!(
            (parsed.edges[1].from.as_str(), parsed.edges[1].to.as_str()),
            ("E", "T")
        );
        assert_eq!(parsed.edges[1].label.as_deref(), Some("仅 Ask 升级"));
        let art = render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render without fallback: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("tentative"), "{text}");
        assert!(text.contains("仅 Ask 升级"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
    }

    /// TASKS.md reopen: quoted dotted label and compact dotted arrow.
    #[test]
    fn dotted_arrow_quoted_and_compact_forms_parse() {
        let source = "flowchart LR\nA -. \"quoted label\" .-> B\nB -.-> C";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.edges.len(), 2, "{parsed:?}");
        assert_eq!(parsed.edges[0].label.as_deref(), Some("quoted label"));
        assert_eq!(parsed.edges[1].label, None);
        render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render without fallback: {err:?}"));
    }

    /// Mermaid.js 11.16.1 rejects slash-separated pseudo-groups: the exact
    /// user line fails at the first slash rather than defining three targets.
    /// The TUI therefore preserves the bounded visible source fallback instead
    /// of inventing graph semantics that disagree with the Web renderer.
    #[test]
    fn slash_separated_pseudo_groups_fail_closed() {
        for diagram in [
            "flowchart LR\nbash --> brush / ProcessManager / sandbox",
            "flowchart LR\nclient / worker --> queue",
        ] {
            let error = parse_mermaid(diagram, MermaidLimits::default()).unwrap_err();
            assert_eq!(error.kind, MermaidDiagnosticKind::InvalidSyntax, "{error:?}");
            let output = render_markdown(
                &format!("```mermaid\n{diagram}\n```"),
                &MarkdownRenderOptions {
                    width: 80,
                    ..MarkdownRenderOptions::default()
                },
            );
            let text = output.plain_text();
            assert!(text.contains("mermaid · source fallback"), "{text}");
            assert!(text.contains("Unsupported flowchart syntax"), "{text}");
        }
    }

    /// A ` / ` inside a quoted edge label or node label is display text and
    /// must NOT split into extra edges/nodes.
    #[test]
    fn quoted_slash_labels_are_never_split() {
        let parsed = parse_mermaid(
            "flowchart LR\nA -->|\"one / two\"| B\nC[\"x / y\"]",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(parsed.edges.len(), 1, "{parsed:?}");
        assert_eq!(parsed.edges[0].label.as_deref(), Some("one / two"));
        assert_eq!(parsed.nodes.len(), 3, "{parsed:?}");
        let c = parsed.nodes.iter().find(|node| node.id == "C").unwrap();
        assert_eq!(c.label, "x / y");
    }

    /// TASKS.md reopen: valid `style` directives are accepted (and dropped —
    /// the TUI projection carries no per-node colors) instead of failing.
    #[test]
    fn style_directives_render_without_fallback() {
        let source = "flowchart TD\nRoot[Root]\nRoot --> Leaf\nstyle Root fill:#6b2432,stroke:#ff6b7a,color:#fff";
        let art = render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render without fallback: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("Root · Root"), "{text}");
        assert!(!text.contains("fill:"), "{text}");
    }

    /// Fail-closed is preserved for malformed style directives.
    #[test]
    fn malformed_style_directives_still_fail_closed() {
        for bad in [
            "flowchart TD\nA\nstyle A",
            "flowchart TD\nA\nstyle A fill",
            "flowchart TD\nA\nstyle A fill:",
            "flowchart TD\nA\nstyle \"A B\" fill:#fff",
        ] {
            let err = parse_mermaid(bad, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{bad}: {err:?}"
            );
        }
    }

    /// TASKS.md reopen: aliased composite state header.
    #[test]
    fn aliased_composite_state_parses_and_renders() {
        let source =
            "stateDiagram-v2\nstate \"Config 层\" as Config {\n    A --> B\n}\nConfig --> Done";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        let config = parsed
            .nodes
            .iter()
            .find(|node| node.id == "Config")
            .unwrap();
        assert_eq!(config.label, "Config 层");
        let art = render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render without fallback: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("Config · Config 层"), "{text}");
    }

    /// TASKS.md reopen: `stateDiagram-v2` recognition matches `stateDiagram`.
    #[test]
    fn state_diagram_v2_header_is_recognized() {
        for header in ["stateDiagram-v2", "stateDiagram"] {
            let source = format!("{header}\n[*] --> A\nA --> [*]");
            let art = render_mermaid_unicode(&source, 48, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("{header} must render: {err:?}"));
            assert_eq!(art.kind, MermaidDiagramKind::StateDiagram, "{header}");
            let text = art.chunks.concat().join("\n");
            assert!(!text.contains("source fallback"), "{text}");
        }
    }

    /// Every supported edge operator parses with the right arrow/label shape.
    #[test]
    fn every_arrow_form_parses() {
        let source = "\
flowchart TD
A --> B
B --- C
C -.-> D
D ==> E
F --via--> G
H -.hop.-> I
J ==fast==> K";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        let edges: Vec<(String, String, Option<String>, bool)> = parsed
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.from.clone(),
                    edge.to.clone(),
                    edge.label.clone(),
                    edge.arrow,
                )
            })
            .collect();
        assert_eq!(
            edges,
            vec![
                ("A".to_owned(), "B".to_owned(), None, true),
                ("B".to_owned(), "C".to_owned(), None, false),
                ("C".to_owned(), "D".to_owned(), None, true),
                ("D".to_owned(), "E".to_owned(), None, true),
                ("F".to_owned(), "G".to_owned(), Some("via".to_owned()), true),
                ("H".to_owned(), "I".to_owned(), Some("hop".to_owned()), true),
                (
                    "J".to_owned(),
                    "K".to_owned(),
                    Some("fast".to_owned()),
                    true
                ),
            ]
        );
    }

    /// Every arrow form renders through the canvas without fallback; the
    /// open link (`---`) must not draw an arrowhead glyph.
    #[test]
    fn every_arrow_form_renders_without_fallback() {
        let source = "\
flowchart TD
A --> B
B --- C
C -.-> D
D ==> E
F --via--> G
H -.hop.-> I
J ==fast==> K";
        for width in [40usize, 80] {
            let art = render_mermaid_unicode(source, width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must render: {err:?}"));
            let text = art.chunks.concat().join("\n");
            for needle in ["A", "B", "C", "D", "E", "G", "I", "K"] {
                assert!(text.contains(needle), "width {width}: {text}");
            }
            for label in ["via", "hop", "fast"] {
                assert!(text.contains(label), "width {width}: {text}");
            }
        }
    }

    /// Quoted/pipe label variants across operators; empty labels are absent.
    #[test]
    fn pipe_label_variants_parse_and_empty_labels_are_dropped() {
        let source = "\
flowchart LR
A -->|\"quoted\"| B
B -->|plain| C
C -->|| D
D -->| | E
E -->|\"\"| F
G -.\"dotted quoted\".-> H";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.edges.len(), 6, "{parsed:?}");
        assert_eq!(parsed.edges[0].label.as_deref(), Some("quoted"));
        assert_eq!(parsed.edges[1].label.as_deref(), Some("plain"));
        for edge in &parsed.edges[2..5] {
            assert_eq!(edge.label, None, "{edge:?}");
        }
        assert_eq!(parsed.edges[5].label.as_deref(), Some("dotted quoted"));
    }

    /// `<br/>` in a NODE label renders as box rows (explicit matrix entry).
    #[test]
    fn node_label_br_renders_as_rows() {
        let source = "flowchart TD\nA[\"第一行<br/>第二行\"] --> B[Done]";
        let art = render_mermaid_unicode(source, 48, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render: {err:?}"));
        let text = art.chunks.concat().join("\n");
        assert!(text.contains("A · 第一行"), "{text}");
        assert!(text.contains("第二行"), "{text}");
        assert!(!text.contains("<br"), "{text}");
    }

    /// Nested subgraphs (depth 2) parse and render: the outer title precedes
    /// the inner title, and both end markers close in reverse order.
    #[test]
    fn nested_subgraphs_depth_two_parse_and_render() {
        let source = "\
flowchart TD
subgraph Outer[\"外层子图\"]
    subgraph Inner[\"内层子图\"]
        A[A] --> B[B]
    end
    B --> C[C]
end
C --> D[D]";
        let parsed_statements = statements(source);
        let parsed = parse_flowchart(parsed_statements, 1, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.subgraphs.len(), 2, "{parsed:?}");
        let inner = parsed.subgraphs.iter().find(|s| s.id == "Inner").unwrap();
        let outer = parsed.subgraphs.iter().find(|s| s.id == "Outer").unwrap();
        assert_eq!(inner.title, "内层子图");
        assert_eq!(outer.title, "外层子图");
        // The inner node range nests inside the outer range.
        assert!(inner.start_node >= outer.start_node && inner.end_node <= outer.end_node);
        let art = render_mermaid_unicode(source, 60, MermaidLimits::default())
            .unwrap_or_else(|err| panic!("must render: {err:?}"));
        let text = art.chunks.concat().join("\n");
        let outer_title = text.find("subgraph Outer · 外层子图").expect("outer title");
        let inner_title = text.find("subgraph Inner · 内层子图").expect("inner title");
        assert!(
            outer_title < inner_title,
            "outer title must precede inner: {text}"
        );
        assert!(text.contains("end subgraph Inner"), "{text}");
        assert!(text.contains("end subgraph Outer"), "{text}");
    }

    /// CJK node and edge labels survive the canvas at CJK widths, bounded.
    #[test]
    fn cjk_labels_render_bounded() {
        // Node ids stay ASCII (the projection's identifier grammar); CJK
        // text belongs in node/edge labels, which must render bounded.
        let source = "flowchart TD\nU[用户会话] -->|\"校验<br/>通过\"| G[API 网关]";
        for width in [24usize, 40, 80] {
            let art = render_mermaid_unicode(source, width, MermaidLimits::default())
                .unwrap_or_else(|err| panic!("width {width} must render: {err:?}"));
            for line in &art.chunks[0] {
                assert!(display_width(line) <= width, "width {width}: {line:?}");
            }
            let text = art.chunks.concat().join("\n");
            assert!(text.contains("用户会话"), "width {width}: {text}");
            assert!(text.contains("校验"), "width {width}: {text}");
            assert!(text.contains("通过"), "width {width}: {text}");
        }
    }

    /// Empty and whitespace-only inputs fail closed with a visible error.
    #[test]
    fn empty_and_whitespace_inputs_fail_closed() {
        for source in ["", "   ", "\n\n", "%% only a comment"] {
            let err = parse_mermaid(source, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{source:?}: {err:?}"
            );
            assert!(err.message.contains("empty"), "{source:?}: {err:?}");
        }
    }

    /// Genuinely malformed flowchart/state syntax remains fail-closed.
    #[test]
    fn malformed_statements_remain_fail_closed() {
        for bad in [
            "flowchart TD\nA -->",
            "flowchart TD\nA -->|unclosed B",
            "flowchart TD\n--> B",
            "flowchart TD\nA --> B &",
            "flowchart TD\nsubgraph S[\"no end\"]\nA[A]",
            "stateDiagram-v2\nA foo B",
            "stateDiagram-v2\nstate \"unterminated as A",
            "stateDiagram-v2\nstate A <<circle>>",
            "flowchart TD\nA -.broken. B",
            "flowchart TD\nA ==> broken ==> ",
        ] {
            let err = parse_mermaid(bad, MermaidLimits::default()).unwrap_err();
            assert_eq!(
                err.kind,
                MermaidDiagnosticKind::InvalidSyntax,
                "{bad:?}: {err:?}"
            );
        }
    }

    /// Whitespace-tolerant happy path: padded operators and labels still
    /// parse to the same structure.
    #[test]
    fn whitespace_padded_operators_parse() {
        let source = "flowchart LR\n  A   -->   B  \n  B  -.  loose  .->   C";
        let parsed = parse_mermaid(source, MermaidLimits::default()).unwrap();
        assert_eq!(parsed.edges.len(), 2, "{parsed:?}");
        assert_eq!(parsed.edges[0].label, None);
        assert_eq!(parsed.edges[1].label.as_deref(), Some("loose"));
    }
}
