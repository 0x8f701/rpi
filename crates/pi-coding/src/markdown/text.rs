use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display width of `text` in terminal cells, measured per grapheme cluster.
///
/// Width conventions follow `unicode-width`'s defaults: East Asian Wide/Fullwidth
/// count 2 cells, everything else 1. East Asian **Ambiguous** characters
/// (`·`, `─`, `│`, `→`, `—`, box-drawing glyphs, …) are counted as **1 cell**
/// here — the ECMA-48 neutral default. A CJK-locale terminal (`LC_CTYPE` in
/// `ja`/`zh`/`ko`, or a terminal configured `Ambiguous=wide`) renders those
/// glyphs 2 cells wide, so frame math built on this function can diverge from
/// what such a terminal paints by one cell per Ambiguous glyph. That is a
/// terminal-locale limitation, deliberately NOT worked around here (forcing
/// Ambiguous=wide would break every non-CJK terminal); verbatim frames in a
/// CJK locale should run with `LC_CTYPE=C`/a narrow-Ambiguous terminal.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn safe_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

pub(crate) fn sanitize_inline(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => output.push_str("    "),
            ch if ch.is_control() => output.push('�'),
            ch => output.push(ch),
        }
    }
    output
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let safe = sanitize_inline(text);
    if safe.trim().is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in safe.split_whitespace() {
        let word_width = display_width(word);
        if word_width <= width {
            let separator = usize::from(!current.is_empty());
            if current_width + separator + word_width <= width {
                if separator == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(word);
                current_width += word_width;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            current.push_str(word);
            current_width = word_width;
            continue;
        }

        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        let chunks = hard_wrap_word(word, width);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index == last {
                current_width = display_width(&chunk);
                current = chunk;
            } else {
                lines.push(chunk);
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Wrap `text` verbatim (no word reflow) to `width` cells, keeping every
/// grapheme cluster intact. Tabs expand to four spaces and control characters
/// are replaced before measuring, so the caller can build exact-width frames.
///
/// A single cluster wider than `width` occupies its own row intact (user text
/// is never split); callers framing the output should clamp such rows with
/// [`fit_text`] so every frame row stays exactly frame-width.
pub fn wrap_verbatim(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let safe = sanitize_inline(text);
    if safe.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for cluster in clusters(&safe) {
        let cluster_width = display_width(cluster);
        if cluster_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            // Lossless overflow policy: an extended grapheme that is wider than
            // the target occupies its own line intact. The line may exceed the
            // requested width, but user text is never split or substituted.
            lines.push(cluster.to_owned());
            continue;
        }
        if current_width + cluster_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(cluster);
        current_width += cluster_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Truncate `text` to `width` cells with a trailing `…`, never splitting a
/// grapheme cluster. Tabs/control characters are sanitized first.
pub fn fit_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let safe = sanitize_inline(text);
    if display_width(&safe) <= width {
        return safe;
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let target = width - 1;
    let mut output_width = 0;
    for cluster in clusters(&safe) {
        let cluster_width = display_width(cluster);
        if output_width + cluster_width > target {
            break;
        }
        output.push_str(cluster);
        output_width += cluster_width;
    }
    output.push('…');
    output
}

fn hard_wrap_word(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for cluster in clusters(word) {
        let cluster_width = display_width(cluster);
        if cluster_width > width {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_width = 0;
            }
            // See wrap_verbatim: preserving the grapheme takes precedence over
            // the width bound when no lossless in-budget representation exists.
            chunks.push(cluster.to_owned());
            continue;
        }
        if current_width + cluster_width > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(cluster);
        current_width += cluster_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn clusters(text: &str) -> impl Iterator<Item = &str> {
    UnicodeSegmentation::graphemes(text, true)
}

pub(crate) fn pad_to_width(text: &str, width: usize) -> String {
    let actual = display_width(text);
    let mut output = String::with_capacity(text.len() + width.saturating_sub(actual));
    output.push_str(text);
    output.extend(std::iter::repeat_n(' ', width.saturating_sub(actual)));
    output
}

