use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use super::text::{display_width, sanitize_inline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineStyle {
    Bold,
    Italic,
    Code,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineStyleRange {
    pub range: Range<usize>,
    pub style: InlineStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledText {
    pub text: String,
    pub styles: Vec<InlineStyleRange>,
}

pub(crate) fn parse_inline(source: &str) -> StyledText {
    parse_segment(&sanitize_inline(source))
}

pub(crate) fn wrap_inline(source: &str, width: usize) -> Vec<StyledText> {
    wrap_styled(parse_inline(source), width)
}

fn parse_segment(source: &str) -> StyledText {
    let mut output = StyledText::default();
    let mut offset = 0;
    while offset < source.len() {
        let rest = &source[offset..];
        if rest.starts_with('`') {
            let ticks = rest.bytes().take_while(|byte| *byte == b'`').count();
            if let Some(close) = find_tick_close(rest, ticks) {
                let start = output.text.len();
                output.text.push_str(&rest[ticks..close]);
                let end = output.text.len();
                if start < end {
                    output.styles.push(InlineStyleRange {
                        range: start..end,
                        style: InlineStyle::Code,
                    });
                }
                offset += close + ticks;
                continue;
            }
        }

        let delimiter = if rest.starts_with("**") {
            Some(("**", InlineStyle::Bold))
        } else if rest.starts_with("__") {
            Some(("__", InlineStyle::Bold))
        } else if rest.starts_with('*') {
            Some(("*", InlineStyle::Italic))
        } else if rest.starts_with('_') {
            Some(("_", InlineStyle::Italic))
        } else {
            None
        };
        if let Some((delimiter, style)) = delimiter
            && can_open(source, offset, delimiter.len())
            && let Some(close) = find_emphasis_close(source, offset + delimiter.len(), delimiter)
        {
            let inner = parse_segment(&source[offset + delimiter.len()..close]);
            let start = output.text.len();
            append_styled(&mut output, &inner, 0..inner.text.len());
            let end = output.text.len();
            if start < end {
                output.styles.push(InlineStyleRange {
                    range: start..end,
                    style,
                });
            }
            offset = close + delimiter.len();
            continue;
        }

        let character = rest.chars().next().expect("offset is inside source");
        output.text.push(character);
        offset += character.len_utf8();
    }
    output.styles.sort_by_key(|styled| (styled.range.start, styled.range.end));
    output
}

fn find_tick_close(rest: &str, ticks: usize) -> Option<usize> {
    let mut offset = ticks;
    while offset < rest.len() {
        let tail = &rest[offset..];
        let character = tail.chars().next()?;
        if character != '`' {
            offset += character.len_utf8();
            continue;
        }
        let run = tail.bytes().take_while(|byte| *byte == b'`').count();
        if run == ticks {
            return Some(offset);
        }
        offset += run;
    }
    None
}

fn find_emphasis_close(source: &str, mut offset: usize, delimiter: &str) -> Option<usize> {
    while offset + delimiter.len() <= source.len() {
        let tail = &source[offset..];
        if tail.starts_with('`') {
            let ticks = tail.bytes().take_while(|byte| *byte == b'`').count();
            if let Some(close) = find_tick_close(tail, ticks) {
                offset += close + ticks;
                continue;
            }
        }
        if tail.starts_with(delimiter) && can_close(source, offset, delimiter.len()) {
            return Some(offset);
        }
        offset += tail.chars().next()?.len_utf8();
    }
    None
}

fn can_open(source: &str, offset: usize, delimiter_len: usize) -> bool {
    if source[offset + delimiter_len..]
        .chars()
        .next()
        .is_none_or(char::is_whitespace)
    {
        return false;
    }
    !source[offset..].starts_with('_')
        || source[..offset]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric())
}

fn can_close(source: &str, offset: usize, delimiter_len: usize) -> bool {
    if source[..offset]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace)
    {
        return false;
    }
    !source[offset..].starts_with('_')
        || source[offset + delimiter_len..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric())
}

fn wrap_styled(parsed: StyledText, width: usize) -> Vec<StyledText> {
    let width = width.max(1);
    if parsed.text.trim().is_empty() {
        return vec![StyledText::default()];
    }
    let mut lines = Vec::new();
    let mut current = StyledText::default();
    let mut current_width = 0;
    for word in word_ranges(&parsed.text) {
        let word_width = display_width(&parsed.text[word.clone()]);
        if word_width <= width {
            let separator = usize::from(!current.text.is_empty());
            if current_width + separator + word_width <= width {
                if separator == 1 {
                    current.text.push(' ');
                    current_width += 1;
                }
                append_styled(&mut current, &parsed, word);
                current_width += word_width;
                continue;
            }
            if !current.text.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            append_styled(&mut current, &parsed, word);
            current_width = word_width;
            continue;
        }

        if !current.text.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        let chunks = hard_wrap_ranges(&parsed.text, word, width);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index == last {
                append_styled(&mut current, &parsed, chunk.clone());
                current_width = display_width(&parsed.text[chunk]);
            } else {
                let mut line = StyledText::default();
                append_styled(&mut line, &parsed, chunk);
                lines.push(line);
            }
        }
    }
    if !current.text.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(StyledText::default());
    }
    lines
}

fn word_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                ranges.push(start..offset);
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(start) = start {
        ranges.push(start..text.len());
    }
    ranges
}

fn hard_wrap_ranges(text: &str, word: Range<usize>, width: usize) -> Vec<Range<usize>> {
    let mut chunks = Vec::new();
    let mut start = word.start;
    let mut current_width = 0;
    for (relative, cluster) in text[word.clone()].grapheme_indices(true) {
        let offset = word.start + relative;
        let cluster_width = display_width(cluster);
        if cluster_width > width {
            if start < offset {
                chunks.push(start..offset);
            }
            chunks.push(offset..offset + cluster.len());
            start = offset + cluster.len();
            current_width = 0;
            continue;
        }
        if current_width + cluster_width > width && start < offset {
            chunks.push(start..offset);
            start = offset;
            current_width = 0;
        }
        current_width += cluster_width;
    }
    if start < word.end {
        chunks.push(start..word.end);
    }
    if chunks.is_empty() {
        chunks.push(word);
    }
    chunks
}

pub(crate) fn append_styled(output: &mut StyledText, source: &StyledText, range: Range<usize>) {
    let destination_start = output.text.len();
    output.text.push_str(&source.text[range.clone()]);
    for styled in &source.styles {
        let start = styled.range.start.max(range.start);
        let end = styled.range.end.min(range.end);
        if start < end {
            output.styles.push(InlineStyleRange {
                range: destination_start + start - range.start..destination_start + end - range.start,
                style: styled.style,
            });
        }
    }
}

pub(crate) fn shifted_styles(styles: &[InlineStyleRange], offset: usize) -> Vec<InlineStyleRange> {
    styles
        .iter()
        .map(|styled| InlineStyleRange {
            range: styled.range.start + offset..styled.range.end + offset,
            style: styled.style,
        })
        .collect()
}
