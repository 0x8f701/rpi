use super::table::{TableBlock, parse_table_at};
use super::text::safe_prefix;

pub const DEFAULT_MAX_MARKDOWN_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnalysisMode {
    #[default]
    Complete,
    Streaming,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkdownBlock {
    Blank,
    Heading {
        level: u8,
        text: String,
        line: usize,
    },
    List {
        items: Vec<ListItem>,
        line: usize,
    },
    FencedCode {
        info: String,
        source: String,
        marker: char,
        marker_len: usize,
        closed: bool,
        line: usize,
    },
    Table(TableBlock),
    BlockQuote {
        lines: Vec<String>,
        line: usize,
    },
    ThematicBreak {
        line: usize,
    },
    Paragraph {
        lines: Vec<String>,
        line: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    pub depth: usize,
    pub marker: ListMarker,
    pub checked: Option<bool>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListMarker {
    Bullet(char),
    Ordered { number: u64, delimiter: char },
}

pub fn analyze_markdown(source: &str) -> MarkdownDocument {
    analyze_markdown_with_mode(source, AnalysisMode::Complete)
}

pub fn analyze_markdown_with_mode(source: &str, mode: AnalysisMode) -> MarkdownDocument {
    analyze_with_limit(source, mode, DEFAULT_MAX_MARKDOWN_BYTES)
}

pub(crate) fn analyze_with_limit(
    source: &str,
    mode: AnalysisMode,
    max_bytes: usize,
) -> MarkdownDocument {
    let (source, truncated) = safe_prefix(source, max_bytes);
    let lines = source
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() {
            if !matches!(blocks.last(), Some(MarkdownBlock::Blank)) {
                blocks.push(MarkdownBlock::Blank);
            }
            index += 1;
            continue;
        }

        if let Some(opening) = parse_fence_opening(line) {
            let start = index;
            index += 1;
            let mut body = Vec::new();
            let mut closed = false;
            while index < lines.len() {
                if is_fence_closing(lines[index], opening.marker, opening.marker_len) {
                    closed = true;
                    index += 1;
                    break;
                }
                body.push(lines[index]);
                index += 1;
            }
            blocks.push(MarkdownBlock::FencedCode {
                info: opening.info,
                source: body.join("\n"),
                marker: opening.marker,
                marker_len: opening.marker_len,
                closed,
                line: start + 1,
            });
            continue;
        }

        if let Some((level, text)) = parse_heading(line) {
            blocks.push(MarkdownBlock::Heading {
                level,
                text: text.to_owned(),
                line: index + 1,
            });
            index += 1;
            continue;
        }

        if is_thematic_break(line) {
            blocks.push(MarkdownBlock::ThematicBreak { line: index + 1 });
            index += 1;
            continue;
        }

        if let Some((table, next)) = parse_table_at(&lines, index, mode) {
            blocks.push(MarkdownBlock::Table(table));
            index = next;
            continue;
        }

        if parse_list_item(line).is_some() {
            let start = index;
            let mut items = Vec::new();
            while index < lines.len() {
                let Some(item) = parse_list_item(lines[index]) else {
                    break;
                };
                items.push(item);
                index += 1;
            }
            blocks.push(MarkdownBlock::List {
                items,
                line: start + 1,
            });
            continue;
        }

        if parse_quote_line(line).is_some() {
            let start = index;
            let mut quoted = Vec::new();
            while index < lines.len() {
                let Some(quote) = parse_quote_line(lines[index]) else {
                    break;
                };
                quoted.push(quote.to_owned());
                index += 1;
            }
            blocks.push(MarkdownBlock::BlockQuote {
                lines: quoted,
                line: start + 1,
            });
            continue;
        }

        let start = index;
        let mut paragraph = Vec::new();
        while index < lines.len() {
            let candidate = lines[index];
            if candidate.trim().is_empty()
                || parse_fence_opening(candidate).is_some()
                || parse_heading(candidate).is_some()
                || is_thematic_break(candidate)
                || parse_list_item(candidate).is_some()
                || parse_quote_line(candidate).is_some()
                || parse_table_at(&lines, index, mode).is_some()
            {
                break;
            }
            paragraph.push(candidate.to_owned());
            index += 1;
        }
        if paragraph.is_empty() {
            paragraph.push(line.to_owned());
            index += 1;
        }
        blocks.push(MarkdownBlock::Paragraph {
            lines: paragraph,
            line: start + 1,
        });
    }

    if matches!(blocks.last(), Some(MarkdownBlock::Blank)) {
        blocks.pop();
    }
    MarkdownDocument { blocks, truncated }
}

pub(crate) fn streaming_tail_start(source: &str) -> usize {
    let mut fence = None;
    let mut latest_boundary = 0usize;
    let mut offset = 0usize;

    for raw_line in source.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((marker, marker_len)) = fence {
            if is_fence_closing(line, marker, marker_len) {
                fence = None;
            }
        } else if let Some(opening) = parse_fence_opening(line) {
            fence = Some((opening.marker, opening.marker_len));
        } else if line.trim().is_empty() {
            // Freeze the first newline immediately before a blank separator.
            // The separator newline stays in the mutable tail, preserving one
            // rendered blank line when frozen and mutable output are joined.
            latest_boundary = offset;
        }
        offset += raw_line.len();
    }

    latest_boundary
}

struct FenceOpening {
    marker: char,
    marker_len: usize,
    info: String,
}

fn parse_fence_opening(line: &str) -> Option<FenceOpening> {
    let indent = line.chars().take_while(|ch| *ch == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let marker_len = rest.chars().take_while(|ch| *ch == marker).count();
    if marker_len < 3 {
        return None;
    }
    let info = rest[marker.len_utf8() * marker_len..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some(FenceOpening {
        marker,
        marker_len,
        info: info.to_owned(),
    })
}

fn is_fence_closing(line: &str, marker: char, minimum: usize) -> bool {
    let indent = line.chars().take_while(|ch| *ch == ' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let count = rest.chars().take_while(|ch| *ch == marker).count();
    count >= minimum && rest[marker.len_utf8() * count..].trim().is_empty()
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let indent = line.chars().take_while(|ch| *ch == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let count = rest.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&count) {
        return None;
    }
    let after = &rest[count..];
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    let text = after.trim();
    let text = text
        .rfind(char::is_whitespace)
        .and_then(|separator| {
            let suffix = text[separator..].trim();
            (!suffix.is_empty() && suffix.chars().all(|ch| ch == '#'))
                .then(|| text[..separator].trim_end())
        })
        .unwrap_or(text);
    Some((count as u8, text))
}

fn parse_list_item(line: &str) -> Option<ListItem> {
    let indent = line.chars().take_while(|ch| matches!(ch, ' ' | '\t')).count();
    let depth = indent / 2;
    let rest = &line[indent..];
    let (marker, body) = if let Some(first) = rest.chars().next() {
        if matches!(first, '-' | '+' | '*') {
            let after = &rest[first.len_utf8()..];
            if !after.starts_with(char::is_whitespace) {
                return None;
            }
            (ListMarker::Bullet(first), after.trim_start())
        } else {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 || digits > 9 {
                return None;
            }
            let number = rest[..digits].parse().ok()?;
            let delimiter = rest[digits..].chars().next()?;
            if !matches!(delimiter, '.' | ')') {
                return None;
            }
            let after = &rest[digits + delimiter.len_utf8()..];
            if !after.starts_with(char::is_whitespace) {
                return None;
            }
            (
                ListMarker::Ordered { number, delimiter },
                after.trim_start(),
            )
        }
    } else {
        return None;
    };

    let (checked, text) = if body.len() >= 3
        && body.starts_with('[')
        && body.as_bytes()[2] == b']'
        && matches!(body.as_bytes()[1], b' ' | b'x' | b'X')
        && body.get(3..).is_none_or(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
    {
        (Some(!matches!(body.as_bytes()[1], b' ')), body[3..].trim_start())
    } else {
        (None, body)
    };

    Some(ListItem {
        depth,
        marker,
        checked,
        text: text.to_owned(),
    })
}

fn parse_quote_line(line: &str) -> Option<&str> {
    let indent = line.chars().take_while(|ch| *ch == ' ').count();
    if indent > 3 {
        return None;
    }
    line[indent..]
        .strip_prefix('>')
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

fn is_thematic_break(line: &str) -> bool {
    let compact = line.chars().filter(|ch| !ch.is_whitespace()).collect::<String>();
    let Some(marker) = compact.chars().next() else {
        return false;
    };
    compact.len() >= 3
        && matches!(marker, '-' | '_' | '*')
        && compact.chars().all(|ch| ch == marker)
}
