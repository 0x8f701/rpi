//! Ratatui conversion for the shared terminal-neutral Markdown renderer.

use pi_coding::markdown::{
    InlineStyle, InlineStyleRange, LineRole, MarkdownRenderOptions, MarkdownRenderOutput,
    RenderDiagnostic, StreamingMarkdownRenderer,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span, Text},
};

/// Complete semantic style map for every [`LineRole`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkdownRatatuiStyles {
    pub text: Style,
    pub heading_1: Style,
    pub heading_2: Style,
    pub heading_3: Style,
    pub heading_4: Style,
    pub heading_5: Style,
    pub heading_6: Style,
    pub list_marker: Style,
    pub quote: Style,
    pub code: Style,
    pub code_fence: Style,
    pub inline_code: Style,
    pub syntax_comment: Style,
    pub syntax_keyword: Style,
    pub syntax_function: Style,
    pub syntax_variable: Style,
    pub syntax_string: Style,
    pub syntax_number: Style,
    pub syntax_type: Style,
    pub syntax_operator: Style,
    pub syntax_punctuation: Style,
    pub table_border: Style,
    pub table_header: Style,
    pub table_body: Style,
    pub mermaid_border: Style,
    pub mermaid_node: Style,
    pub mermaid_edge: Style,
    pub diagnostic: Style,
    pub thematic_break: Style,
}

impl Default for MarkdownRatatuiStyles {
    fn default() -> Self {
        let plain = Style::default();
        Self {
            text: plain,
            heading_1: plain.add_modifier(Modifier::BOLD),
            heading_2: plain.add_modifier(Modifier::BOLD),
            heading_3: plain.add_modifier(Modifier::BOLD),
            heading_4: plain.add_modifier(Modifier::BOLD),
            heading_5: plain.add_modifier(Modifier::BOLD),
            heading_6: plain.add_modifier(Modifier::BOLD),
            list_marker: plain,
            quote: plain.add_modifier(Modifier::ITALIC),
            code: plain,
            code_fence: plain,
            inline_code: plain,
            syntax_comment: plain,
            syntax_keyword: plain,
            syntax_function: plain,
            syntax_variable: plain,
            syntax_string: plain,
            syntax_number: plain,
            syntax_type: plain,
            syntax_operator: plain,
            syntax_punctuation: plain,
            table_border: plain,
            table_header: plain.add_modifier(Modifier::BOLD),
            table_body: plain,
            mermaid_border: plain,
            mermaid_node: plain,
            mermaid_edge: plain,
            diagnostic: plain.add_modifier(Modifier::ITALIC),
            thematic_break: plain,
        }
    }
}

impl MarkdownRatatuiStyles {
    #[must_use]
    pub const fn style_for(self, role: LineRole) -> Style {
        match role {
            LineRole::Text => self.text,
            LineRole::Heading(1) => self.heading_1,
            LineRole::Heading(2) => self.heading_2,
            LineRole::Heading(3) => self.heading_3,
            LineRole::Heading(4) => self.heading_4,
            LineRole::Heading(5) => self.heading_5,
            LineRole::Heading(_) => self.heading_6,
            LineRole::ListMarker => self.list_marker,
            LineRole::Quote => self.quote,
            LineRole::Code => self.code,
            LineRole::CodeFence => self.code_fence,
            LineRole::TableBorder => self.table_border,
            LineRole::TableHeader => self.table_header,
            LineRole::TableBody => self.table_body,
            LineRole::MermaidBorder => self.mermaid_border,
            LineRole::MermaidNode => self.mermaid_node,
            LineRole::MermaidEdge => self.mermaid_edge,
            LineRole::Diagnostic => self.diagnostic,
            LineRole::ThematicBreak => self.thematic_break,
        }
    }
}

/// Styled Ratatui output plus the shared renderer diagnostics.
#[derive(Clone, Debug, Default)]
pub struct RatatuiMarkdownOutput {
    pub lines: Vec<Line<'static>>,
    pub diagnostics: Vec<RenderDiagnostic>,
    pub truncated: bool,
}

impl RatatuiMarkdownOutput {
    #[must_use]
    pub fn text(&self) -> Text<'static> {
        Text::from(self.lines.clone())
    }
}

/// Convert neutral Markdown output without reparsing or changing its width.
///
/// Semantic styles are applied on both [`Line::style`] and each [`Span`]'s style so
/// callers that inspect either surface still observe heading bold, theme roles, and
/// other presentation contracts.
#[must_use]
pub fn to_ratatui(
    output: &MarkdownRenderOutput,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    RatatuiMarkdownOutput {
        lines: output
            .lines
            .iter()
            .map(|line| {
                let style = styles.style_for(line.role);
                if line.role == LineRole::Code {
                    return Line::from(syntax_spans(&line.text, line.language.as_deref(), styles))
                        .style(style);
                }
                if !line.inline_styles.is_empty() {
                    return styled_ranges(&line.text, &line.inline_styles, style, styles).style(style);
                }
                Line::from(vec![Span::styled(line.text.clone(), style)]).style(style)
            })
            .collect(),
        diagnostics: output.diagnostics.clone(),
        truncated: output.truncated,
    }
}

fn styled_ranges(
    text: &str,
    ranges: &[InlineStyleRange],
    base: Style,
    styles: MarkdownRatatuiStyles,
) -> Line<'static> {
    let mut boundaries = vec![0, text.len()];
    for styled in ranges {
        boundaries.push(styled.range.start);
        boundaries.push(styled.range.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut spans = Vec::new();
    for window in boundaries.windows(2) {
        let range = window[0]..window[1];
        if range.is_empty() {
            continue;
        }
        let mut style = base;
        for styled in ranges
            .iter()
            .filter(|styled| styled.range.start <= range.start && styled.range.end >= range.end)
        {
            match styled.style {
                InlineStyle::Bold => style = style.add_modifier(Modifier::BOLD),
                InlineStyle::Italic => style = style.add_modifier(Modifier::ITALIC),
                InlineStyle::Code => style = style.patch(styles.inline_code),
                // The marker is chrome, not item content: the bullet/number
                // prefix carries the list_marker theme while the item body
                // keeps the base text style.
                InlineStyle::ListMarker => style = styles.list_marker,
                // Separators are chrome, not header/body content. Their explicit
                // range makes the border theme authoritative without reparsing.
                InlineStyle::TableBorder => style = styles.table_border,
            }
        }
        // Split on whitespace so repeated plain vs styled tokens remain
        // addressable as exact span contents (e.g. two plain `same` + two code `same`).
        push_token_spans(&mut spans, &text[range], style);
    }
    Line::from(spans)
}

fn push_token_spans(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        let character = rest.chars().next().expect("offset is inside text");
        let is_whitespace = character.is_whitespace();
        let mut end = offset + character.len_utf8();
        while end < text.len() {
            let next = text[end..].chars().next().expect("end is inside text");
            if next.is_whitespace() != is_whitespace {
                break;
            }
            end += next.len_utf8();
        }
        spans.push(Span::styled(text[offset..end].to_owned(), style));
        offset = end;
    }
}

fn syntax_spans(
    line: &str,
    language: Option<&str>,
    styles: MarkdownRatatuiStyles,
) -> Vec<Span<'static>> {
    // Fenced body rows carry the frame chrome (`│ … │`). Lex only the code
    // between the sides; the chrome is styled with the code-fence role so the
    // per-line sides match the top/bottom border color.
    let (prefix, source, suffix) = split_code_chrome(line);
    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix.to_owned(), styles.code_fence));
    }
    // Separate trailing frame padding from the code source so comment lexers
    // (which run to end-of-line) don't absorb it. The padding stays
    // code-colored (invisible) and the joined spans still reproduce the row
    // text exactly.
    let content_len = source.trim_end().len();
    let (content, tail) = source.split_at(content_len);
    lex_code_source(content, language, styles, &mut spans);
    if !tail.is_empty() {
        spans.push(Span::styled(tail.to_owned(), styles.code));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix.to_owned(), styles.code_fence));
    }
    spans
}

/// Split the frame chrome off a fenced body row: a leading `│ ` (or the
/// legacy one-cell layout pad) and the trailing `│` with its preceding
/// padding. Rows without a trailing side (e.g. Mermaid source-fallback
/// lines) keep their whole body as code source.
fn split_code_chrome(line: &str) -> (&str, &str, &str) {
    let (prefix, rest) = if let Some(rest) = line.strip_prefix('│') {
        if let Some(rest) = rest.strip_prefix(' ') {
            ("│ ", rest)
        } else {
            ("│", rest)
        }
    } else if let Some(rest) = line.strip_prefix(' ') {
        (" ", rest)
    } else {
        ("", line)
    };
    // The closing frame glyph is the last `│` in the row.
    match rest.rfind('│') {
        Some(index) => (prefix, &rest[..index], &rest[index..]),
        None => (prefix, rest, ""),
    }
}

pub(crate) fn syntax_spans_unpadded(
    source: &str,
    language: Option<&str>,
    styles: MarkdownRatatuiStyles,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    lex_code_source(source, language, styles, &mut spans);
    spans
}

fn lex_code_source(
    source: &str,
    language: Option<&str>,
    styles: MarkdownRatatuiStyles,
    spans: &mut Vec<Span<'static>>,
) {
    if source.is_empty() {
        return;
    }
    match classify_language(language) {
        CodeLanguage::Shell => lex_shell(source, styles, spans),
        CodeLanguage::Json => lex_json(source, styles, spans),
        CodeLanguage::RustLike => lex_rust_like(source, styles, spans),
        CodeLanguage::Plain => spans.push(Span::styled(source.to_owned(), styles.code)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeLanguage {
    Shell,
    Json,
    RustLike,
    Plain,
}

fn classify_language(language: Option<&str>) -> CodeLanguage {
    match language.unwrap_or_default() {
        "sh" | "bash" | "shell" | "zsh" | "fish" | "ksh" | "shellscript" => CodeLanguage::Shell,
        "json" | "jsonc" | "json5" => CodeLanguage::Json,
        "rs" | "rust" | "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" | "python"
        | "py" | "go" | "java" | "c" | "cpp" | "c++" | "csharp" | "cs" | "kotlin" | "swift"
        | "ruby" | "rb" | "php" | "scala" | "toml" | "yaml" | "yml" | "sql" | "lua" | "zig"
        | "dart" | "r" | "perl" | "haskell" | "hs" | "elixir" | "ex" | "clojure" | "clj" => {
            CodeLanguage::RustLike
        }
        _ => CodeLanguage::Plain,
    }
}

fn push_styled(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(text.to_owned(), style));
    }
}

fn next_non_ws(chars: &[char], mut index: usize) -> Option<char> {
    while index < chars.len() {
        if !chars[index].is_whitespace() {
            return Some(chars[index]);
        }
        index += 1;
    }
    None
}

fn take_while(chars: &[char], index: &mut usize, mut pred: impl FnMut(char) -> bool) {
    while *index < chars.len() && pred(chars[*index]) {
        *index += 1;
    }
}

fn take_string(chars: &[char], index: &mut usize, quote: char) {
    *index += 1;
    while *index < chars.len() {
        let current = chars[*index];
        *index += 1;
        if current == '\\' {
            if *index < chars.len() {
                *index += 1;
            }
            continue;
        }
        if current == quote {
            break;
        }
    }
}

fn collect_slice(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

fn is_shell_command_separator_token(token: &str) -> bool {
    matches!(
        token,
        "|" | "||" | "&" | "&&" | ";" | "|&" | "(" | ")" | "{" | "}"
    ) || token.ends_with('|')
}

fn is_shell_flag_start(chars: &[char], index: usize) -> bool {
    chars.get(index) == Some(&'-')
        && chars
            .get(index + 1)
            .is_some_and(|next| !next.is_whitespace() && !matches!(next, '>' | '|' | '&'))
}

fn looks_like_path(token: &str) -> bool {
    if token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.contains('/')
    {
        return true;
    }
    // Bare filenames like `main.rs` / `package.json` — not `..` alone.
    token.contains('.')
        && token != "."
        && token != ".."
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+' | '~'))
}

fn lex_shell(source: &str, styles: MarkdownRatatuiStyles, spans: &mut Vec<Span<'static>>) {
    let chars = source.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut expect_command = true;

    while index < chars.len() {
        let start = index;
        let ch = chars[index];

        if ch.is_whitespace() {
            take_while(&chars, &mut index, char::is_whitespace);
            push_styled(spans, &collect_slice(&chars, start, index), styles.code);
            continue;
        }

        if ch == '#' {
            push_styled(
                spans,
                &collect_slice(&chars, start, chars.len()),
                styles.syntax_comment,
            );
            break;
        }

        if ch == '"' || ch == '\'' {
            take_string(&chars, &mut index, ch);
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_string,
            );
            expect_command = false;
            continue;
        }

        if ch == '`' {
            index += 1;
            while index < chars.len() {
                let current = chars[index];
                index += 1;
                if current == '\\' {
                    if index < chars.len() {
                        index += 1;
                    }
                    continue;
                }
                if current == '`' {
                    break;
                }
            }
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_string,
            );
            expect_command = false;
            continue;
        }

        if ch == '$' {
            index += 1;
            if index < chars.len() && chars[index] == '{' {
                index += 1;
                while index < chars.len() && chars[index] != '}' {
                    index += 1;
                }
                if index < chars.len() {
                    index += 1;
                }
            } else if index < chars.len() && chars[index] == '(' {
                index += 1;
                let mut depth = 1usize;
                while index < chars.len() && depth > 0 {
                    match chars[index] {
                        '(' => depth += 1,
                        ')' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    index += 1;
                }
            } else {
                take_while(&chars, &mut index, |c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '?' || c == '!' || c == '#' || c == '@' || c == '*'
                });
            }
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_variable,
            );
            expect_command = false;
            continue;
        }

        // Operators / separators. Flags (`-la`, `--long`) stay on the word path.
        if !is_shell_flag_start(&chars, index)
            && matches!(
                ch,
                '|' | '&' | ';' | '<' | '>' | '!' | '=' | '(' | ')' | '+' | '*' | '/' | '%' | '^'
                    | '~' | '?'
            )
        {
            index += 1;
            while index < chars.len() {
                let prev = chars[index - 1];
                let cur = chars[index];
                let digraph = matches!(
                    (prev, cur),
                    ('|', '|')
                        | ('&', '&')
                        | ('>', '>')
                        | ('<', '<')
                        | ('>', '|')
                        | ('|', '&')
                        | ('&', '>')
                        | ('>', '&')
                        | ('!', '=')
                        | ('=', '=')
                );
                if digraph {
                    index += 1;
                    continue;
                }
                break;
            }
            let token = collect_slice(&chars, start, index);
            push_styled(spans, &token, styles.syntax_operator);
            expect_command = is_shell_command_separator_token(&token);
            continue;
        }

        // Word: command, flag, path, or plain argument.
        take_while(&chars, &mut index, |c| {
            !c.is_whitespace()
                && c != '#'
                && c != '"'
                && c != '\''
                && c != '`'
                && c != '$'
                && c != '|'
                && c != '&'
                && c != ';'
                && c != '<'
                && c != '>'
                && c != '('
                && c != ')'
        });
        // Keep trailing globs attached if present as separate? already included via not-separator.
        let token = collect_slice(&chars, start, index);
        let style = if token.starts_with('-') && token != "-" && token != "--" {
            styles.syntax_keyword // flags
        } else if looks_like_path(&token) {
            styles.syntax_type // paths use type role (distinct, not punctuation soup)
        } else if expect_command {
            styles.syntax_function // command names
        } else if token.chars().all(|c| c.is_ascii_digit()) {
            styles.syntax_number
        } else {
            styles.syntax_variable
        };
        push_styled(spans, &token, style);
        expect_command = false;
    }
}

fn lex_json(source: &str, styles: MarkdownRatatuiStyles, spans: &mut Vec<Span<'static>>) {
    let chars = source.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let ch = chars[index];
        if ch.is_whitespace() {
            take_while(&chars, &mut index, char::is_whitespace);
            push_styled(spans, &collect_slice(&chars, start, index), styles.code);
        } else if ch == '"' {
            take_string(&chars, &mut index, '"');
            let token = collect_slice(&chars, start, index);
            let style = if next_non_ws(&chars, index) == Some(':') {
                styles.syntax_variable // object keys
            } else {
                styles.syntax_string
            };
            push_styled(spans, &token, style);
        } else if ch == '/' && index + 1 < chars.len() && chars[index + 1] == '/' {
            // jsonc line comment
            push_styled(
                spans,
                &collect_slice(&chars, start, chars.len()),
                styles.syntax_comment,
            );
            break;
        } else if ch.is_ascii_digit() || (ch == '-' && index + 1 < chars.len() && chars[index + 1].is_ascii_digit()) {
            if ch == '-' {
                index += 1;
            }
            take_while(&chars, &mut index, |c| {
                c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')
            });
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_number,
            );
        } else if ch.is_ascii_alphabetic() {
            take_while(&chars, &mut index, |c| c.is_ascii_alphanumeric());
            let token = collect_slice(&chars, start, index);
            let style = match token.as_str() {
                "true" | "false" | "null" => styles.syntax_keyword,
                _ => styles.syntax_variable,
            };
            push_styled(spans, &token, style);
        } else {
            index += 1;
            let style = if matches!(ch, '{' | '}' | '[' | ']' | ':' | ',') {
                styles.syntax_punctuation
            } else {
                styles.syntax_operator
            };
            push_styled(spans, &collect_slice(&chars, start, index), style);
        }
    }
}

fn lex_rust_like(source: &str, styles: MarkdownRatatuiStyles, spans: &mut Vec<Span<'static>>) {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "def", "else", "enum",
        "false", "fn", "for", "from", "if", "impl", "import", "in", "let", "loop", "match", "mod",
        "move", "mut", "null", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while", "yield", "class", "function",
        "var", "val", "interface", "package", "new", "this", "typeof", "instanceof", "switch",
        "case", "default", "try", "catch", "finally", "throw", "throws", "extends", "implements",
        "public", "private", "protected", "void", "None", "Some", "Ok", "Err", "and", "or", "not",
        "is", "lambda", "with", "pass", "raise", "except", "elif", "global", "nonlocal", "assert",
        "del", "nil", "func", "defer", "go", "chan", "select", "map", "range",
    ];
    const PRIMITIVE_TYPES: &[&str] = &[
        "bool", "boolean", "string", "str", "char", "usize", "isize", "u8", "u16", "u32", "u64",
        "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64", "int", "long", "float", "double",
        "byte", "short", "uint", "bigint", "number", "any", "never", "unknown", "object",
    ];

    let chars = source.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let ch = chars[index];

        if ch.is_whitespace() {
            take_while(&chars, &mut index, char::is_whitespace);
            push_styled(spans, &collect_slice(&chars, start, index), styles.code);
            continue;
        }

        // Line comments: // # --
        if (ch == '/' && index + 1 < chars.len() && chars[index + 1] == '/')
            || ch == '#'
            || (ch == '-' && index + 1 < chars.len() && chars[index + 1] == '-')
        {
            push_styled(
                spans,
                &collect_slice(&chars, start, chars.len()),
                styles.syntax_comment,
            );
            break;
        }

        // Block comment start /* ... */
        if ch == '/' && index + 1 < chars.len() && chars[index + 1] == '*' {
            index += 2;
            while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                index += 1;
            }
            if index + 1 < chars.len() {
                index += 2;
            } else {
                index = chars.len();
            }
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_comment,
            );
            continue;
        }

        if ch == '"' || ch == '\'' {
            take_string(&chars, &mut index, ch);
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_string,
            );
            continue;
        }

        // Raw / byte string prefixes kept simple: r"..." already handled if quote follows letter path.
        if ch.is_ascii_digit() {
            take_while(&chars, &mut index, |c| {
                c.is_ascii_hexdigit() || matches!(c, '.' | '_' | 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
            });
            push_styled(
                spans,
                &collect_slice(&chars, start, index),
                styles.syntax_number,
            );
            continue;
        }

        if ch.is_alphanumeric() || ch == '_' {
            take_while(&chars, &mut index, |c| c.is_alphanumeric() || c == '_');
            let token = collect_slice(&chars, start, index);
            let style = if KEYWORDS.contains(&token.as_str()) {
                styles.syntax_keyword
            } else if PRIMITIVE_TYPES.contains(&token.as_str())
                || token.chars().next().is_some_and(|c| c.is_uppercase())
            {
                styles.syntax_type
            } else if next_non_ws(&chars, index) == Some('(') {
                styles.syntax_function
            } else {
                styles.syntax_variable
            };
            push_styled(spans, &token, style);
            continue;
        }

        // Lifetime or char already handled; operators / punctuation.
        index += 1;
        // absorb multi-char operators
        while index < chars.len() {
            let prev = chars[index - 1];
            let cur = chars[index];
            let cont = matches!(
                (prev, cur),
                ('=', '=')
                    | ('!', '=')
                    | ('<', '=')
                    | ('>', '=')
                    | ('+', '=')
                    | ('-', '=')
                    | ('*', '=')
                    | ('/', '=')
                    | ('%', '=')
                    | ('&', '&')
                    | ('|', '|')
                    | ('<', '<')
                    | ('>', '>')
                    | ('-', '>')
                    | (':', ':')
                    | ('.', '.')
                    | ('=', '>')
            );
            if cont {
                index += 1;
                continue;
            }
            break;
        }
        let token = collect_slice(&chars, start, index);
        let style = if token.chars().all(|c| "+-*/%=!<>|&^~?".contains(c)) {
            styles.syntax_operator
        } else {
            styles.syntax_punctuation
        };
        push_styled(spans, &token, style);
    }
}
/// Render bounded Markdown at `width` and convert it to Ratatui lines.
#[must_use]
pub fn render_ratatui_markdown(
    source: &str,
    width: u16,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    let options = MarkdownRenderOptions {
        width: usize::from(width.max(1)),
        ..MarkdownRenderOptions::default()
    };
    to_ratatui(&pi_coding::markdown::render_markdown(source, &options), styles)
}

/// Render bounded in-progress Markdown at `width` and convert it to Ratatui lines.
///
/// Streaming mode keeps incomplete tables and Mermaid fences visible as source
/// until their closing syntax arrives, matching the shared print renderer.
#[must_use]
pub fn render_ratatui_markdown_streaming(
    source: &str,
    width: u16,
    styles: MarkdownRatatuiStyles,
) -> RatatuiMarkdownOutput {
    let mut renderer = StreamingRatatuiMarkdownRenderer::new(width, styles);
    renderer.push_str(source);
    renderer.output()
}

/// Stateful streaming adapter that preserves the shared renderer's frozen prefix.
#[derive(Clone, Debug)]
pub struct StreamingRatatuiMarkdownRenderer {
    renderer: StreamingMarkdownRenderer,
    styles: MarkdownRatatuiStyles,
}

impl StreamingRatatuiMarkdownRenderer {
    #[must_use]
    pub fn new(width: u16, styles: MarkdownRatatuiStyles) -> Self {
        Self::with_options(
            MarkdownRenderOptions {
                width: usize::from(width.max(1)),
                ..MarkdownRenderOptions::default()
            },
            styles,
        )
    }

    #[must_use]
    pub fn with_options(
        mut options: MarkdownRenderOptions,
        styles: MarkdownRatatuiStyles,
    ) -> Self {
        options.width = options.width.max(1);
        Self {
            renderer: StreamingMarkdownRenderer::new(options),
            styles,
        }
    }

    pub fn push_str(&mut self, chunk: &str) {
        self.renderer.push_str(chunk);
    }

    #[must_use]
    pub fn output(&self) -> RatatuiMarkdownOutput {
        to_ratatui(&self.renderer.output(), self.styles)
    }

    #[must_use]
    pub fn frozen_source_bytes(&self) -> usize {
        self.renderer.frozen_source_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::markdown::{LineRole, NeutralLine};
    use ratatui::style::{Color, Modifier};
    use std::collections::BTreeSet;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn color_key(color: Color) -> u32 {
        // Color is not Ord; map to a stable key for uniqueness checks in tests.
        match color {
            Color::Reset => 0,
            Color::Black => 1,
            Color::Red => 2,
            Color::Green => 3,
            Color::Yellow => 4,
            Color::Blue => 5,
            Color::Magenta => 6,
            Color::Cyan => 7,
            Color::Gray => 8,
            Color::DarkGray => 9,
            Color::LightRed => 10,
            Color::LightGreen => 11,
            Color::LightYellow => 12,
            Color::LightBlue => 13,
            Color::LightMagenta => 14,
            Color::LightCyan => 15,
            Color::White => 16,
            Color::Indexed(index) => 100 + u32::from(index),
            Color::Rgb(r, g, b) => {
                1_000_000 + (u32::from(r) << 16) + (u32::from(g) << 8) + u32::from(b)
            }
        }
    }

    #[test]
    fn every_line_role_maps_to_its_explicit_style() {
        let styles = MarkdownRatatuiStyles {
            text: Style::default().fg(Color::White),
            heading_1: Style::default().fg(Color::Red),
            heading_2: Style::default().fg(Color::Green),
            heading_3: Style::default().fg(Color::Blue),
            heading_4: Style::default().fg(Color::Yellow),
            heading_5: Style::default().fg(Color::Magenta),
            heading_6: Style::default().fg(Color::Cyan),
            list_marker: Style::default().fg(Color::LightBlue),
            quote: Style::default().fg(Color::LightGreen),
            code: Style::default().fg(Color::LightRed),
            code_fence: Style::default().fg(Color::LightYellow),
            inline_code: Style::default().fg(Color::LightRed),
            syntax_comment: Style::default().fg(Color::DarkGray),
            syntax_keyword: Style::default().fg(Color::Blue),
            syntax_function: Style::default().fg(Color::Green),
            syntax_variable: Style::default().fg(Color::White),
            syntax_string: Style::default().fg(Color::Yellow),
            syntax_number: Style::default().fg(Color::LightYellow),
            syntax_type: Style::default().fg(Color::Cyan),
            syntax_operator: Style::default().fg(Color::LightBlue),
            syntax_punctuation: Style::default().fg(Color::Gray),
            table_border: Style::default().fg(Color::DarkGray),
            table_header: Style::default().fg(Color::Gray),
            table_body: Style::default().fg(Color::LightCyan),
            mermaid_border: Style::default().fg(Color::LightMagenta),
            mermaid_node: Style::default().fg(Color::LightGreen),
            mermaid_edge: Style::default().fg(Color::LightBlue),
            diagnostic: Style::default().fg(Color::Red).add_modifier(Modifier::ITALIC),
            thematic_break: Style::default().fg(Color::Gray),
        };
        let roles = [
            LineRole::Text,
            LineRole::Heading(1),
            LineRole::Heading(2),
            LineRole::Heading(3),
            LineRole::Heading(4),
            LineRole::Heading(5),
            LineRole::Heading(6),
            LineRole::Heading(9),
            LineRole::ListMarker,
            LineRole::Quote,
            LineRole::Code,
            LineRole::CodeFence,
            LineRole::TableBorder,
            LineRole::TableHeader,
            LineRole::TableBody,
            LineRole::MermaidBorder,
            LineRole::MermaidNode,
            LineRole::MermaidEdge,
            LineRole::Diagnostic,
            LineRole::ThematicBreak,
        ];
        let neutral = MarkdownRenderOutput {
            lines: roles
                .iter()
                .enumerate()
                .map(|(index, role)| NeutralLine {
                    text: index.to_string(),
                    role: *role,
                    inline_styles: Vec::new(),
                    language: None,
                })
                .collect(),
            diagnostics: Vec::new(),
            truncated: false,
        };
        let converted = to_ratatui(&neutral, styles);
        for (line, role) in converted.lines.iter().zip(roles) {
            assert_eq!(line.style, styles.style_for(role));
        }
        assert_eq!(converted.lines[7].style, styles.heading_6);
    }

    #[test]
    fn headings_tables_mermaid_and_fallback_match_neutral_text() {
        let source = "# Heading\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\npie\n\"Breakfast\" 5\n```";
        let options = MarkdownRenderOptions {
            width: 32,
            ..MarkdownRenderOptions::default()
        };
        let neutral = pi_coding::markdown::render_markdown(source, &options);
        let tui = to_ratatui(&neutral, MarkdownRatatuiStyles::default());
        assert_eq!(
            tui.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
        assert!(tui.lines.iter().any(|line| line_text(line).contains("┌─ mermaid · flowchart")));
        assert!(tui.lines.iter().any(|line| line_text(line).contains("source fallback")));
        assert_eq!(tui.diagnostics, neutral.diagnostics);
    }

    #[test]
    fn streaming_output_matches_neutral_and_keeps_frozen_prefix_stable() {
        let styles = MarkdownRatatuiStyles::default();
        let mut streaming = StreamingRatatuiMarkdownRenderer::new(40, styles);
        streaming.push_str("# Stable\n\nmutable");
        let first = streaming.output();
        let frozen = streaming.frozen_source_bytes();
        assert_eq!(frozen, "# Stable\n".len());
        let prefix = first.lines[..1].iter().map(line_text).collect::<Vec<_>>();

        streaming.push_str(" tail\n\n| a | b |\n| --- | --- |\n| 1 | 2 |");
        let second = streaming.output();
        assert_eq!(
            second.lines[..prefix.len()].iter().map(line_text).collect::<Vec<_>>(),
            prefix
        );
        let source = "# Stable\n\nmutable tail\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
        let neutral = pi_coding::markdown::render_markdown_streaming(
            source,
            &MarkdownRenderOptions {
                width: 40,
                ..MarkdownRenderOptions::default()
            },
        );
        assert_eq!(
            second.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
    }

    #[test]
    fn streaming_adapter_matches_shared_neutral_output_from_chunks() {
        let source = "# Stable\n\n- [x] done\n  2. next\n\n| Name | Stat |\n| --- | --- |\n| Tokyo | ✅ |\n\n```mermaid\nflowchart TD\nA --> B";
        let options = MarkdownRenderOptions {
            width: 32,
            ..MarkdownRenderOptions::default()
        };
        let neutral = pi_coding::markdown::render_markdown_streaming(source, &options);
        let mut renderer = StreamingRatatuiMarkdownRenderer::new(
            32,
            MarkdownRatatuiStyles::default(),
        );
        for chunk in [
            "# Stable\n\n- [x] done\n  2. next\n\n| Name |",
            " Stat |\n| --- | --- |\n",
            "| Tokyo | ✅ |\n\n```mermaid\nflowchart TD\nA --> B",
        ] {
            renderer.push_str(chunk);
        }
        let tui = renderer.output();
        assert_eq!(
            tui.lines.iter().map(line_text).collect::<Vec<_>>(),
            neutral.plain_lines()
        );
        assert_eq!(tui.diagnostics, neutral.diagnostics);
        assert!(tui.lines.iter().any(|line| line_text(line).contains("flowchart TD")));
        assert!(!tui.lines.iter().any(|line| line_text(line).contains("mermaid · flowchart")));
    }

    #[test]
    fn fenced_code_uses_multiple_semantic_token_styles() {
        let styles = MarkdownRatatuiStyles {
            syntax_keyword: Style::default().fg(Color::Blue),
            syntax_function: Style::default().fg(Color::Green),
            syntax_variable: Style::default().fg(Color::White),
            syntax_string: Style::default().fg(Color::Yellow),
            syntax_number: Style::default().fg(Color::LightYellow),
            syntax_type: Style::default().fg(Color::Cyan),
            syntax_operator: Style::default().fg(Color::LightBlue),
            syntax_punctuation: Style::default().fg(Color::Gray),
            ..MarkdownRatatuiStyles::default()
        };
        for source in [
            "```rust\nlet count: usize = parse(42);\n```",
            "```json\n{\"ready\": true, \"count\": 42}\n```",
        ] {
            let output = render_ratatui_markdown(source, 80, styles);
            let colors = output
                .lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter_map(|span| span.style.fg)
                .map(color_key)
                .collect::<BTreeSet<_>>();
            assert!(colors.len() >= 3, "expected semantic token colors: {colors:?}");
        }
    }

    #[test]
    fn list_marker_colors_only_the_prefix_and_item_text_keeps_base() {
        let styles = MarkdownRatatuiStyles {
            text: Style::default().fg(Color::White),
            list_marker: Style::default().fg(Color::LightBlue),
            ..MarkdownRatatuiStyles::default()
        };
        let output = render_ratatui_markdown(
            "- 第一条要点\n- 第二条 **重点**\n1. 编号步骤\n  2. 嵌套项",
            80,
            styles,
        );
        let lines = &output.lines;
        assert_eq!(lines.len(), 4, "one row per item: {lines:?}");
        // Bullet list: marker span blue, body span base.
        let bullet = &lines[0];
        assert_eq!(span_fg(bullet, "•"), Some(Color::LightBlue));
        assert_eq!(span_fg(bullet, "第一条要点"), Some(Color::White));
        // Bold inside a list item keeps BOLD and the base foreground.
        let bold = &lines[1];
        let bold_span = bold
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "重点")
            .expect("bold span");
        assert_eq!(bold_span.style.fg, Some(Color::White));
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        // Ordered list: number span blue, body span base.
        let ordered = &lines[2];
        assert_eq!(span_fg(ordered, "1."), Some(Color::LightBlue));
        assert_eq!(span_fg(ordered, "编号步骤"), Some(Color::White));
        // Nested list: leading indent stays base, only the marker is blue.
        let nested = &lines[3];
        assert_eq!(span_fg(nested, "  "), Some(Color::White));
        assert_eq!(span_fg(nested, "2."), Some(Color::LightBlue));
        assert_eq!(span_fg(nested, "嵌套项"), Some(Color::White));
    }

    fn distinct_syntax_styles() -> MarkdownRatatuiStyles {
        MarkdownRatatuiStyles {
            code: Style::default().fg(Color::White),
            code_fence: Style::default().fg(Color::DarkGray),
            syntax_comment: Style::default().fg(Color::DarkGray),
            syntax_keyword: Style::default().fg(Color::Blue),
            syntax_function: Style::default().fg(Color::Green),
            syntax_variable: Style::default().fg(Color::LightCyan),
            syntax_string: Style::default().fg(Color::Yellow),
            syntax_number: Style::default().fg(Color::LightYellow),
            syntax_type: Style::default().fg(Color::Cyan),
            syntax_operator: Style::default().fg(Color::LightBlue),
            syntax_punctuation: Style::default().fg(Color::Gray),
            ..MarkdownRatatuiStyles::default()
        }
    }

    fn code_body_lines(output: &RatatuiMarkdownOutput) -> Vec<&Line<'static>> {
        let total = output.lines.len();
        if total <= 2 {
            return Vec::new();
        }
        output.lines[1..total - 1].iter().collect()
    }


    fn span_fg(line: &Line<'_>, needle: &str) -> Option<Color> {
        line.spans.iter().find_map(|span| {
            (span.content.as_ref() == needle).then_some(span.style.fg).flatten()
        })
    }

    #[test]
    fn shell_sample_preserves_text_and_distinct_token_styles() {
        let styles = distinct_syntax_styles();
        let source = "```bash\n# list project\nls -la ./src | grep -E 'main' && echo \"done $HOME\"\n```";
        let width = 72u16;
        let output = render_ratatui_markdown(source, width, styles);
        let plain = output.lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(plain.iter().any(|line| line.contains("╭── code · bash")));
        assert!(plain.iter().any(|line| line.starts_with("╰") && line.ends_with("╯")));
        assert!(output.lines.iter().all(|line| line.width() <= usize::from(width)));

        for line in &output.lines {
            let joined: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
            assert_eq!(joined, line_text(line));
        }

        let body = code_body_lines(&output);
        assert!(body.iter().all(|line| line_text(line).starts_with("│ ")));

        let comment = body
            .iter()
            .find(|line| line_text(line).contains("# list project"))
            .expect("comment line");
        assert_eq!(span_fg(comment, "# list project"), Some(Color::DarkGray));

        let command = body
            .iter()
            .find(|line| line_text(line).contains("ls -la"))
            .expect("command line");
        let expected_command =
            format!("│ ls -la ./src | grep -E 'main' && echo \"done $HOME\"{} │", " ".repeat(18));
        assert_eq!(line_text(command), expected_command);
        assert_eq!(span_fg(command, "ls"), Some(Color::Green));
        assert_eq!(span_fg(command, "-la"), Some(Color::Blue));
        assert_eq!(span_fg(command, "./src"), Some(Color::Cyan));
        assert_eq!(span_fg(command, "|"), Some(Color::LightBlue));
        assert_eq!(span_fg(command, "grep"), Some(Color::Green));
        assert_eq!(span_fg(command, "-E"), Some(Color::Blue));
        assert_eq!(span_fg(command, "'main'"), Some(Color::Yellow));
        assert_eq!(span_fg(command, "&&"), Some(Color::LightBlue));
        assert_eq!(span_fg(command, "echo"), Some(Color::Green));
        assert_eq!(span_fg(command, "\"done $HOME\""), Some(Color::Yellow));

        let colors = command
            .spans
            .iter()
            .filter_map(|span| span.style.fg)
            .map(color_key)
            .collect::<BTreeSet<_>>();
        assert!(
            colors.len() >= 5,
            "shell sample needs distinct command/flag/path/op/string colors, got {colors:?}"
        );
    }

    #[test]
    fn rust_and_json_semantics_keep_comments_strings_and_exact_text() {
        let styles = distinct_syntax_styles();
        let width = 80u16;

        let rust = render_ratatui_markdown(
            "```rust\n// setup\nlet count: usize = parse(\"42\");\n```",
            width,
            styles,
        );
        assert!(rust
            .lines
            .iter()
            .any(|line| line_text(line).contains("╭── code · rust")));
        let rust_body = code_body_lines(&rust);
        let comment = rust_body
            .iter()
            .find(|line| line_text(line).contains("// setup"))
            .expect("rust comment");
        assert_eq!(span_fg(comment, "// setup"), Some(Color::DarkGray));
        let stmt = rust_body
            .iter()
            .find(|line| line_text(line).contains("let count"))
            .expect("rust stmt");
        let expected_stmt =
            format!("│ let count: usize = parse(\"42\");{} │", " ".repeat(45));
        assert_eq!(line_text(stmt), expected_stmt);
        assert_eq!(span_fg(stmt, "let"), Some(Color::Blue));
        assert_eq!(span_fg(stmt, "count"), Some(Color::LightCyan));
        assert_eq!(span_fg(stmt, "usize"), Some(Color::Cyan));
        assert_eq!(span_fg(stmt, "parse"), Some(Color::Green));
        assert_eq!(span_fg(stmt, "\"42\""), Some(Color::Yellow));
        assert_eq!(span_fg(stmt, "="), Some(Color::LightBlue));

        let json = render_ratatui_markdown(
            "```json\n{\"ready\": true, \"count\": 42, \"msg\": \"hi\"}\n```",
            width,
            styles,
        );
        assert!(json
            .lines
            .iter()
            .any(|line| line_text(line).contains("╭── code · json")));
        let json_line = code_body_lines(&json)
            .into_iter()
            .find(|line| line_text(line).contains("ready"))
            .expect("json body");
        let expected_json =
            format!("│ {{\"ready\": true, \"count\": 42, \"msg\": \"hi\"}}{} │", " ".repeat(35));
        assert_eq!(line_text(json_line), expected_json);
        assert_eq!(span_fg(json_line, "\"ready\""), Some(Color::LightCyan));
        assert_eq!(span_fg(json_line, "true"), Some(Color::Blue));
        assert_eq!(span_fg(json_line, "42"), Some(Color::LightYellow));
        assert_eq!(span_fg(json_line, "\"hi\""), Some(Color::Yellow));
        assert_eq!(span_fg(json_line, "{"), Some(Color::Gray));

        for output in [&rust, &json] {
            assert!(output
                .lines
                .iter()
                .all(|line| line.width() <= usize::from(width)));
            for line in &output.lines {
                let joined: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
                assert_eq!(joined, line_text(line));
            }
        }
    }

    #[test]
    fn unicode_code_and_unknown_language_stay_width_safe() {
        let styles = distinct_syntax_styles();
        let width = 40u16;
        let source = "```text\n// not a real comment lexer target\nAthína/emoji 😀 and café\n```";
        let output = render_ratatui_markdown(source, width, styles);
        assert!(output
            .lines
            .iter()
            .any(|line| line_text(line).contains("╭── code · text")));
        assert!(output
            .lines
            .iter()
            .all(|line| line.width() <= usize::from(width)));
        let body = code_body_lines(&output);
        let unicode = body
            .iter()
            .find(|line| line_text(line).contains("😀"))
            .expect("unicode body");
        assert!(line_text(unicode).contains("Athína/emoji 😀 and café"));
        let joined: String = unicode
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(joined, line_text(unicode));
    }

    #[test]
    fn fenced_code_geometry_has_title_padding_and_closing_border() {
        let output =
            render_ratatui_markdown("```sh\necho hi\n```", 48, MarkdownRatatuiStyles::default());
        let plain = output.lines.iter().map(line_text).collect::<Vec<_>>();
        let expected_top = format!("╭── code · sh ──{}╮", "─".repeat(31));
        assert_eq!(plain.first().map(String::as_str), Some(expected_top.as_str()));
        let expected_bottom = format!("╰{}╯", "─".repeat(46));
        assert!(plain.last().is_some_and(|line| *line == expected_bottom));
        let expected_body = format!("│ echo hi{} │", " ".repeat(37));
        assert_eq!(
            plain.iter().find(|line| line.contains("echo")).map(String::as_str),
            Some(expected_body.as_str())
        );
        let empty = render_ratatui_markdown("```rust\n```", 32, MarkdownRatatuiStyles::default());
        let empty_plain = empty.lines.iter().map(line_text).collect::<Vec<_>>();
        let expected_empty = format!(
            "╭── code · rust ──{}╮",
            "─".repeat(13)
        );
        assert_eq!(
            empty_plain,
            vec![
                expected_empty,
                format!("│ {} │", " ".repeat(28)),
                format!("╰{}╯", "─".repeat(30))
            ]
        );
    }

    #[test]
    fn unclosed_fence_renders_complete_frame_with_marker() {
        // User-reported: a still-streaming code block must never render with
        // only side borders. The streaming path emits the tool-card top, the
        // side-bordered body rows, and a temporary bottom carrying the
        // unclosed marker — while a closed fence keeps the plain bottom.
        let styles = MarkdownRatatuiStyles::default();
        let streaming =
            render_ratatui_markdown_streaming("```json\n{\"a\":1}", 48, styles);
        let plain = streaming.lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(
            plain.iter().any(|line| line.starts_with("╭── code · json")),
            "unclosed fence must keep the top border: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("│ {")),
            "unclosed fence must keep side-bordered body rows: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.contains("… (unclosed fence)")),
            "unclosed fence must carry the marker on the bottom: {plain:?}"
        );
        assert!(
            plain
                .iter()
                .rev()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| line.starts_with("╰")),
            "unclosed fence must end with a bottom border row: {plain:?}"
        );
        assert!(
            streaming
                .diagnostics
                .iter()
                .any(|diag| matches!(diag, RenderDiagnostic::UnclosedFence { .. })),
            "streaming unclosed fence must surface the diagnostic"
        );

        let closed = render_ratatui_markdown("```json\n{\"a\":1}\n```", 48, styles);
        let closed_plain = closed.lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(
            closed_plain
                .last()
                .is_some_and(|line| line.starts_with("╰") && !line.contains("unclosed")),
            "closed fences keep the plain bottom border: {closed_plain:?}"
        );
        assert!(closed.diagnostics.is_empty());
    }


    #[test]
    fn emphasis_and_repeated_code_project_exact_semantic_spans() {
        let styles = MarkdownRatatuiStyles {
            inline_code: Style::default().fg(Color::LightRed),
            ..MarkdownRatatuiStyles::default()
        };
        let output = render_ratatui_markdown(
            "plain **bold** and *italic* plus same `same` and same `same`",
            80,
            styles,
        );
        let line = &output.lines[0];
        assert_eq!(line_text(line), "plain bold and italic plus same same and same same");
        assert!(line.spans.iter().any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(line.spans.iter().any(|span| {
            span.content == "italic" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert_eq!(
            line.spans
                .iter()
                .filter(|span| span.content == "same" && span.style.fg == Some(Color::LightRed))
                .count(),
            2
        );
        assert_eq!(
            line.spans
                .iter()
                .filter(|span| span.content == "same" && span.style.fg != Some(Color::LightRed))
                .count(),
            2
        );
    }

    #[test]
    fn table_inline_code_strips_delimiters_and_keeps_wrapped_url_aligned() {
        let styles = MarkdownRatatuiStyles {
            table_body: Style::default().fg(Color::White),
            inline_code: Style::default().fg(Color::LightRed),
            ..MarkdownRatatuiStyles::default()
        };
        let source = "| channel | delivery |\n| --- | --- |\n| web | `https://example.com/releases/download/latest/artifact.tar.gz` |";
        let output = render_ratatui_markdown(source, 34, styles);
        let text = output.lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().all(|line| !line.contains('`')));
        assert!(output.lines.iter().all(|line| line.width() <= 34));
        assert!(output.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(Color::LightRed) && span.content.contains("https"))
        }));
        assert!(text
            .iter()
            .filter(|line| line.starts_with('│'))
            .all(|line| line.ends_with('│')));
    }

    #[test]
    fn table_borders_use_one_style_while_content_semantics_survive() {
        let styles = MarkdownRatatuiStyles {
            table_border: Style::default().fg(Color::DarkGray),
            table_header: Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            table_body: Style::default().fg(Color::White),
            inline_code: Style::default().fg(Color::LightRed),
            ..MarkdownRatatuiStyles::default()
        };
        let source = "| **Document** | Role |\n| --- | --- |\n| Guide | Read `semantic ranges` before changing borders |";
        let output = render_ratatui_markdown(source, 52, styles);
        assert!(output.lines.iter().all(|line| line.width() <= 52));

        for line in &output.lines {
            for span in &line.spans {
                if span.content.chars().any(|character| "┌┬┐├┼┤│└┴┘─".contains(character)) {
                    assert_eq!(span.style.fg, Some(Color::DarkGray), "{span:?}");
                }
            }
        }
        assert!(output.lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "Document"
                && span.style.fg == Some(Color::Yellow)
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(output.lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("semantic") && span.style.fg == Some(Color::LightRed)
        }));
        assert!(output.lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "Guide" && span.style.fg == Some(Color::White)
        }));
    }

    #[test]
    fn zero_width_is_safely_clamped() {
        let output = render_ratatui_markdown("---", 0, MarkdownRatatuiStyles::default());
        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].width(), 1);
    }

    #[test]
    fn reported_class_and_subgraph_match_neutral_single_cards() {
        let source = "```mermaid\n\
classDiagram\n\
class Application {\n\
+run()\n\
}\n\
class Session {\n\
+id: String\n\
}\n\
class Agent {\n\
+tools: Vec\n\
}\n\
class AgentTool {\n\
+name: String\n\
}\n\
Application --> Session\n\
Agent ..> AgentTool : via context\n\
```\n\n```mermaid\n\
flowchart LR\n\
subgraph records[\"SessionRecord types\"]\n\
A[Session] --> B[Message]\n\
B --> C[ToolCall]\n\
end\n\
X[User] --> A\n\
```";
        let options = MarkdownRenderOptions {
            width: 48,
            ..MarkdownRenderOptions::default()
        };
        let neutral = pi_coding::markdown::render_markdown(source, &options);
        let tui = to_ratatui(&neutral, MarkdownRatatuiStyles::default());
        let text = tui.lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(text, neutral.plain_lines());
        assert_eq!(tui.diagnostics, neutral.diagnostics);
        assert!(neutral.diagnostics.is_empty());
        assert_eq!(
            text.iter().filter(|line| line.contains("┌─ mermaid ·")).count(),
            2
        );
        assert!(text.iter().any(|line| line.contains("┌─ mermaid · classDiagram")));
        assert!(text.iter().any(|line| line.contains("┌─ mermaid · flowchart")));
        assert!(text.iter().all(|line| !line.contains("source fallback")));
        assert!(text.iter().any(|line| line.contains("via context")));
        assert!(text
            .iter()
            .any(|line| line.contains("subgraph records · SessionRecord types")));
        assert_eq!(
            text.iter()
                .filter(|line| line.starts_with("└─") && !line.contains('┘'))
                .count(),
            2
        );
    }

    #[test]
    fn table_border_uses_one_theme_token_and_wraps_within_budget() {
        // Coherency contract: the live TUI maps every table chrome glyph to a
        // single semantic theme token (theme.tool_card_border — the same
        // visible frame role as code/tool-card frames), so all `─│┌┐└┘├┤┬┼┴`
        // cells share one real-RGB color — never a mix of ANSI defaults, never
        // Reset. md_code_block_border (#2a3038) sits too close to the #0f1216
        // background to read as a card border, so the table must NOT fall back
        // to that dim token. Driven through the same public style map the
        // transcript uses (table_border <- theme token), so a wiring regression
        // (hardcoded color, Reset, or per-glyph divergence) fails at the
        // rendered-span level rather than by grepping source.
        let theme = crate::theme::DARK;
        let border = theme.tool_card_border;
        assert_ne!(border, Color::Reset, "table border token must be a real RGB");
        assert_ne!(
            border, theme.md_code_block_border,
            "table border must use the visible card frame color, not the dim md_code_block_border"
        );
        let styles = MarkdownRatatuiStyles {
            text: Style::default().fg(theme.text),
            heading_2: Style::default().fg(theme.md_heading),
            table_border: Style::default().fg(border),
            table_header: Style::default().fg(theme.md_heading).add_modifier(Modifier::BOLD),
            table_body: Style::default().fg(theme.text),
            ..MarkdownRatatuiStyles::default()
        };

        // A realistic 3-column comparison table plus a 2-column doc/role table
        // at a constrained width. Balanced wrapping must keep every row within
        // budget and preserve each column's content (no clipping/overflow, no
        // collapsed column).
        let source = "| Field | OMP dark | OMP light |\n\
                      | --- | --- | --- |\n\
                      | text | #d4d4d4 | default |\n\
                      | toolTitle | #d4d4d4 | default |\n\n\
                      | Document | Role |\n\
                      | --- | --- |\n\
                      | Guide | Read semantic ranges before changing borders |";
        let width = 52u16;
        let output = render_ratatui_markdown(source, width, styles);

        // (1) Width bound — balanced wrapping, no row clips past the budget.
        assert!(
            output.lines.iter().all(|line| line.width() <= usize::from(width)),
            "every table row must fit within {width}: {:?}",
            output.lines.iter().map(line_text).collect::<Vec<_>>()
        );

        // (2) Single border color — every chrome glyph span carries the token.
        let mut border_colors = BTreeSet::new();
        let glyphs = "┌┬┐├┼┤│└┴┘─";
        for line in &output.lines {
            for span in &line.spans {
                if span.content.chars().any(|c| glyphs.contains(c)) {
                    assert_eq!(
                        span.style.fg,
                        Some(border),
                        "border glyph must use the theme token: {span:?}"
                    );
                    border_colors.insert(color_key(span.style.fg.unwrap_or(Color::Reset)));
                }
            }
        }
        assert!(!border_colors.is_empty(), "source must render at least one border glyph");
        assert_eq!(
            border_colors.len(),
            1,
            "table border must be a single color, got {border_colors:?}"
        );

        // (3) Content survival — each column's distinctive token remains
        // visible, so balanced wrapping kept columns materially usable.
        let plain: String = output
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        for needle in ["Field", "toolTitle", "Document", "Role", "Guide", "semantic", "default"] {
            assert!(plain.contains(needle), "column content {needle:?} must survive wrapping: {plain}");
        }
    }
}
