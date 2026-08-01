use super::AnalysisMode;
use super::inline::{InlineStyleRange, StyledText, append_styled, parse_inline, shifted_styles, wrap_inline};
use super::text::display_width;

const MAX_TABLE_COLUMNS: usize = 32;
const MAX_LAYOUT_WIDTH: usize = 1_000;
const MAX_TABLE_ROWS: usize = 2048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAlignment {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableBlock {
    pub headers: Vec<String>,
    pub alignments: Vec<TableAlignment>,
    pub rows: Vec<Vec<String>>,
    pub source_lines: Vec<String>,
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableLayout {
    pub lines: Vec<String>,
    pub inline_styles: Vec<Vec<InlineStyleRange>>,
    pub column_widths: Vec<usize>,
    pub width: usize,
}

pub fn layout_table(table: &TableBlock, available_width: usize) -> Option<TableLayout> {
    let columns = table.headers.len();
    if columns == 0 || columns > MAX_TABLE_COLUMNS {
        return None;
    }
    let available_width = available_width.min(MAX_LAYOUT_WIDTH);
    if available_width < columns * 3 + 1 {
        return None;
    }
    let widths = allocate_widths(table, available_width)?;
    let mut lines = Vec::new();
    let mut inline_styles = Vec::new();
    lines.push(border('┌', '┬', '┐', &widths));
    inline_styles.push(Vec::new());
    append_row(&mut lines, &mut inline_styles, &table.headers, &widths, &table.alignments);
    lines.push(border('├', '┼', '┤', &widths));
    inline_styles.push(Vec::new());
    for row in &table.rows {
        append_row(&mut lines, &mut inline_styles, row, &widths, &table.alignments);
    }
    lines.push(border('└', '┴', '┘', &widths));
    inline_styles.push(Vec::new());
    let width = display_width(lines.first()?);
    Some(TableLayout {
        lines,
        inline_styles,
        column_widths: widths,
        width,
    })
}

pub(crate) fn parse_table_at(
    lines: &[&str],
    start: usize,
    mode: AnalysisMode,
) -> Option<(TableBlock, usize)> {
    if start + 1 >= lines.len() || !contains_unescaped_pipe(lines[start]) {
        return None;
    }
    let header = parse_pipe_row(lines[start])?;
    if header.is_empty() || header.len() > MAX_TABLE_COLUMNS {
        return None;
    }
    let separator = parse_separator_row(lines[start + 1], header.len())?;
    if matches!(mode, AnalysisMode::Streaming)
        && lines[start + 2..].iter().all(|line| line.trim().is_empty())
    {
        return None;
    }

    let mut rows = Vec::new();
    let mut source_lines = vec![lines[start].to_owned(), lines[start + 1].to_owned()];
    let mut index = start + 2;
    while index < lines.len() && rows.len() < MAX_TABLE_ROWS {
        let line = lines[index];
        if line.trim().is_empty() || !contains_unescaped_pipe(line) {
            break;
        }
        let Some(mut cells) = parse_pipe_row(line) else {
            break;
        };
        normalize_cells(&mut cells, header.len());
        rows.push(cells);
        source_lines.push(line.to_owned());
        index += 1;
    }

    Some((
        TableBlock {
            headers: header,
            alignments: separator,
            rows,
            source_lines,
            line: start + 1,
        },
        index,
    ))
}

fn allocate_widths(table: &TableBlock, available_width: usize) -> Option<Vec<usize>> {
    let columns = table.headers.len();
    let borders_and_padding = columns * 3 + 1;
    let content_budget = available_width.checked_sub(borders_and_padding)?;
    let minimum_width = 1;
    if content_budget < columns {
        return None;
    }

    let mut preferred = vec![1; columns];
    for (column, header) in table.headers.iter().enumerate() {
        preferred[column] = preferred[column].max(display_width(&parse_inline(header).text));
    }
    for row in &table.rows {
        for (column, cell) in row.iter().enumerate().take(columns) {
            preferred[column] = preferred[column].max(display_width(&parse_inline(cell).text));
        }
    }
    let mut widths = vec![minimum_width; columns];
    let mut remaining = content_budget - columns * minimum_width;
    while remaining > 0 {
        let mut candidate = None;
        for (column, (&width, &want)) in widths.iter().zip(&preferred).enumerate() {
            if width >= want {
                continue;
            }
            let score = (width, want.saturating_sub(width), usize::MAX - column);
            if candidate.is_none_or(|(_, current)| score < current) {
                candidate = Some((column, score));
            }
        }
        let Some((column, _)) = candidate else {
            break;
        };
        widths[column] += 1;
        remaining -= 1;
    }
    while remaining > 0 {
        for width in &mut widths {
            if remaining == 0 {
                break;
            }
            *width += 1;
            remaining -= 1;
        }
    }
    Some(widths)
}

fn append_row(
    output: &mut Vec<String>,
    output_styles: &mut Vec<Vec<InlineStyleRange>>,
    cells: &[String],
    widths: &[usize],
    alignments: &[TableAlignment],
) {
    let parsed = widths
        .iter()
        .enumerate()
        .map(|(column, width)| {
            wrap_inline(
                cells.get(column).map(String::as_str).unwrap_or_default(),
                *width,
            )
        })
        .collect::<Vec<_>>();
    let height = parsed.iter().map(Vec::len).max().unwrap_or(1);
    for line in 0..height {
        let mut rendered = String::from("│");
        let mut styles = Vec::new();
        for (column, width) in widths.iter().enumerate() {
            let text = parsed[column].get(line).cloned().unwrap_or_default();
            let aligned = align_styled(text, *width, alignments.get(column).copied().unwrap_or_default());
            rendered.push(' ');
            styles.extend(shifted_styles(&aligned.styles, rendered.len()));
            rendered.push_str(&aligned.text);
            rendered.push(' ');
            rendered.push('│');
        }
        output.push(rendered);
        output_styles.push(styles);
    }
}

fn align_styled(text: StyledText, width: usize, alignment: TableAlignment) -> StyledText {
    let actual = display_width(&text.text).min(width);
    let remaining = width.saturating_sub(actual);
    let (left, right) = match alignment {
        TableAlignment::Left => (0, remaining),
        TableAlignment::Right => (remaining, 0),
        TableAlignment::Center => (remaining / 2, remaining - remaining / 2),
    };
    let mut aligned = StyledText {
        text: " ".repeat(left),
        styles: Vec::new(),
    };
    append_styled(&mut aligned, &text, 0..text.text.len());
    aligned.text.push_str(&" ".repeat(right));
    aligned
}

fn border(left: char, joint: char, right: char, widths: &[usize]) -> String {
    let mut rendered = String::new();
    rendered.push(left);
    for (index, width) in widths.iter().enumerate() {
        rendered.extend(std::iter::repeat_n('─', width + 2));
        rendered.push(if index + 1 == widths.len() { right } else { joint });
    }
    rendered
}

fn parse_separator_row(line: &str, columns: usize) -> Option<Vec<TableAlignment>> {
    let cells = parse_pipe_row(line)?;
    if cells.len() != columns {
        return None;
    }
    cells
        .into_iter()
        .map(|cell| {
            let trimmed = cell.trim();
            let left = trimmed.starts_with(':');
            let right = trimmed.ends_with(':');
            let dashes = trimmed.trim_matches(':');
            if dashes.len() < 3 || !dashes.chars().all(|ch| ch == '-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => TableAlignment::Center,
                (false, true) => TableAlignment::Right,
                _ => TableAlignment::Left,
            })
        })
        .collect()
}

fn parse_pipe_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !contains_unescaped_pipe(trimmed) {
        return None;
    }

    // Drop a leading unescaped `|` and a trailing unescaped `|` when present.
    // Both ends are byte indices into `trimmed`; never form a body range with
    // start > end (e.g. a lone `|` would otherwise become 1..0 and panic).
    let start = if trimmed.starts_with('|') { '|'.len_utf8() } else { 0 };
    let end = match trimmed.char_indices().next_back() {
        Some((index, '|')) if index >= start && !is_escaped(trimmed, index) => index,
        _ => trimmed.len(),
    };
    // Invariant: start <= end <= trimmed.len(), both on char boundaries.
    debug_assert!(start <= end && end <= trimmed.len());
    debug_assert!(trimmed.is_char_boundary(start) && trimmed.is_char_boundary(end));
    let body = trimmed.get(start..end).unwrap_or("");

    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    let mut code_ticks = 0usize;
    let chars = body.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            cell.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '`' {
            let run_start = index;
            while index < chars.len() && chars[index] == '`' {
                cell.push('`');
                index += 1;
            }
            let run = index - run_start;
            if code_ticks == 0 {
                code_ticks = run;
            } else if code_ticks == run {
                code_ticks = 0;
            }
            continue;
        }
        if ch == '|' && code_ticks == 0 {
            cells.push(cell.trim().to_owned());
            cell.clear();
            index += 1;
            continue;
        }
        cell.push(ch);
        index += 1;
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell.trim().to_owned());
    Some(cells)
}

fn normalize_cells(cells: &mut Vec<String>, columns: usize) {
    cells.resize(columns, String::new());
    cells.truncate(columns);
}

fn contains_unescaped_pipe(line: &str) -> bool {
    line.char_indices()
        .any(|(index, ch)| ch == '|' && !is_escaped(line, index))
}

fn is_escaped(line: &str, index: usize) -> bool {
    let preceding = line[..index]
        .chars()
        .rev()
        .take_while(|ch| *ch == '\\')
        .count();
    preceding % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::AnalysisMode;

    #[test]
    fn parse_pipe_row_is_total_on_partial_and_edge_rows() {
        // Contract: every partial/incomplete pipe row must return without
        // panicking on invalid slices (lone `|` used to form 1..0).
        let cases = [
            "|",
            "||",
            "|||",
            "|a|",
            "|a",
            "a|",
            "|a||b|",
            "|  |",
            "| |",
            "  |x|  ",
            "|Language|Paradigm|",
            "|Language|Paradigm",
            "Language|Paradigm|",
            "|Ελληνικά|Paradigm|",
            "|a\\|b|",
            "\\|a|",
            "| `a|b` | c |",
            "|a|b|c|",
            "| | |",
            "|\u{3000}|", // ideographic space cell
        ];
        for line in cases {
            let cells = parse_pipe_row(line);
            assert!(
                cells.is_some(),
                "expected Some(...) for pipe row {line:?}, got None"
            );
        }

        assert_eq!(parse_pipe_row("|"), Some(vec![String::new()]));
        assert_eq!(parse_pipe_row("||"), Some(vec![String::new()]));
        assert_eq!(parse_pipe_row("|a|"), Some(vec!["a".to_owned()]));
        assert_eq!(
            parse_pipe_row("|Language|Paradigm|"),
            Some(vec!["Language".to_owned(), "Paradigm".to_owned()])
        );
        assert_eq!(
            parse_pipe_row("|Ελληνικά|Paradigm|"),
            Some(vec!["Ελληνικά".to_owned(), "Paradigm".to_owned()])
        );
        assert_eq!(
            parse_pipe_row("|a||b|"),
            Some(vec!["a".to_owned(), String::new(), "b".to_owned()])
        );
        assert_eq!(parse_pipe_row("no pipes here"), None);
    }

    #[test]
    fn separator_candidates_from_partial_headers_never_panic() {
        // Reproduce the streamed header path: header arrives first, then a
        // separator candidate is classified (or rejected) without slicing panic.
        let lines = ["|Language|Paradigm|", "|"];
        assert!(parse_table_at(&lines, 0, AnalysisMode::Complete).is_none());
        assert!(parse_table_at(&lines, 0, AnalysisMode::Streaming).is_none());

        let partial_only = ["|Language|Paradigm|"];
        assert!(parse_table_at(&partial_only, 0, AnalysisMode::Streaming).is_none());

        for separator in ["|", "||", "|---|", "| --- | --- |", "|:---|---:|"] {
            let rows = ["|Language|Paradigm|", separator];
            let _ = parse_table_at(&rows, 0, AnalysisMode::Complete);
            let _ = parse_table_at(&rows, 0, AnalysisMode::Streaming);
        }
    }

    #[test]
    fn valid_table_still_parses_with_alignments() {
        let lines = [
            "| left | mid | right |",
            "| :--- | :---: | ---: |",
            "| a | b | c |",
        ];
        let (table, next) =
            parse_table_at(&lines, 0, AnalysisMode::Complete).expect("valid table");
        assert_eq!(next, 3);
        assert_eq!(table.headers, vec!["left", "mid", "right"]);
        assert_eq!(
            table.alignments,
            vec![
                TableAlignment::Left,
                TableAlignment::Center,
                TableAlignment::Right
            ]
        );
        assert_eq!(table.rows, vec![vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]]);
    }
}
