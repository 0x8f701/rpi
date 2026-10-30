//! Read-only workspace file viewer overlay for the TUI (Web file-explorer
//! drawer parity: TASKS.md "TUI closest parity").
//!
//! Path candidates originate from the rendered transcript: inline markdown
//! code spans and relative markdown-link targets that parse as
//! `workspace-relative path[:line[:col]]` — the same semantics as the Web
//! `parseWorkspacePathCandidate` in `web/src/markdown.ts`. A keybinding
//! (default Alt+O) opens a single candidate directly, or a picker when several
//! exist; `/file <path[:line]>` opens directly and surfaces bounded rejection
//! messages for traversal, absolute, and scheme-shaped inputs.
//!
//! File content is served exclusively through the existing workspace-scoped
//! `file_explorer` read path (`resolve_scoped_path` + PolicyFs authority +
//! no-follow handles), so every rejection the Web drawer produces
//! (`not a text file`, `not a file`, `path is outside the workspace`,
//! `cursor out of range`) the TUI viewer produces too. Pages inherit the
//! Web caps (`file_explorer::DEFAULT_PAGE_LINES`/byte bounds), open tabs are
//! capped, and the overlay paints only a window of the active page.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use pi_coding::Application;

use crate::file_explorer;
use crate::theme::Theme;

/// Distinct file tabs the viewer retains. Bounded: opening a ninth file
/// evicts the oldest tab.
pub(crate) const MAX_OPEN_FILES: usize = 8;
/// Distinct path candidates one transcript scan may surface (bounded picker).
pub(crate) const MAX_CANDIDATES: usize = 32;

/// Web-parity caps for `path[:line[:col]]` candidates (`markdown.ts`).
const MAX_WORKSPACE_PATH_CHARS: usize = 512;
const MAX_WORKSPACE_PATH_LINE: usize = 10_000_000;
const MAX_WORKSPACE_PATH_COLUMN: usize = 1_000_000;

/// Trailing punctuation candidates tolerate (`markdown.ts`
/// `WORKSPACE_PATH_TRAILING`): sentence/parenthesis debris only.
const TRAILING_PUNCTUATION: [char; 6] = ['.', ',', ')', ':', ';', '`'];

/// One actionable `path[:line[:col]]` candidate parsed from transcript text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathCandidate {
    pub(crate) path: String,
    pub(crate) line: Option<usize>,
    pub(crate) column: Option<usize>,
}

impl PathCandidate {
    /// `path` (with `:line` appended when requested), for status messages.
    pub(crate) fn label(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

/// Returns `(head, tail)` when the last `:…` segment is entirely decimal
/// digits. Mirrors the tail groups of the Web regexes
/// `/^(.*?):(\d+)(?::(\d+))?$/`: the lazy prefix yields the unique split at
/// the rightmost valid `:digits` boundary, and an empty path head is allowed
/// so the final degenerate-path check can reject it (like `":5"`).
fn trailing_decimal(text: &str) -> Option<(&str, &str)> {
    let (head, tail) = text.rsplit_once(':')?;
    if tail.is_empty() || !tail.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((head, tail))
}

fn trailing_line_range(text: &str) -> Option<(&str, &str)> {
    let (head, tail) = text.rsplit_once(':')?;
    let (start, end) = tail.split_once('-')?;
    if start.is_empty()
        || end.is_empty()
        || !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((head, start))
}

/// Clamp a parsed decimal number like the Web `clampInt` (floor of the
/// finite value, minimum 1).
fn clamp_capacity(text: &str, max: usize) -> usize {
    let value = text.parse::<u128>().unwrap_or(u128::MAX);
    usize::try_from(value).unwrap_or(max).clamp(1, max)
}

/// Parse a workspace-relative `path[:line[:col]]` or `path:start-end`
/// candidate. Mirrors the Web code-surface parser.
pub(crate) fn parse_workspace_path_candidate(text: &str) -> Option<PathCandidate> {
    parse_workspace_path_on(text, PathCandidateSurface::Code)
}

#[derive(Clone, Copy)]
enum PathCandidateSurface {
    Code,
    Href,
}

fn parse_workspace_path_on(text: &str, surface: PathCandidateSurface) -> Option<PathCandidate> {
    let mut trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if matches!(surface, PathCandidateSurface::Href) && has_rfc_scheme(trimmed) {
        return None;
    }
    while let Some(last) = trimmed.chars().next_back() {
        if TRAILING_PUNCTUATION.contains(&last) {
            trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    let trimmed = trimmed.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_WORKSPACE_PATH_CHARS {
        return None;
    }
    if trimmed.ends_with('/') || trimmed.ends_with('\\') {
        return None;
    }
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_ascii_control() || c == '\u{7f}')
    {
        return None;
    }
    if matches!(surface, PathCandidateSurface::Code) && blocked_or_non_line_scheme(trimmed) {
        return None;
    }

    let mut path = trimmed;
    let mut line = None;
    let mut column = None;
    if let Some((rest, tail)) = trailing_decimal(trimmed) {
        if let Some((inner, line_tail)) = trailing_decimal(rest) {
            path = inner;
            line = Some(clamp_capacity(line_tail, MAX_WORKSPACE_PATH_LINE));
            column = Some(clamp_capacity(tail, MAX_WORKSPACE_PATH_COLUMN));
        } else {
            path = rest;
            line = Some(clamp_capacity(tail, MAX_WORKSPACE_PATH_LINE));
        }
    } else if let Some((rest, start)) = trailing_line_range(trimmed) {
        path = rest;
        line = Some(clamp_capacity(start, MAX_WORKSPACE_PATH_LINE));
    }

    finalize_path_candidate(path, line, column, surface)
}

fn finalize_path_candidate(
    path: &str,
    line: Option<usize>,
    column: Option<usize>,
    surface: PathCandidateSurface,
) -> Option<PathCandidate> {
    if path.is_empty() || path.chars().count() > MAX_WORKSPACE_PATH_CHARS {
        return None;
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return None;
    }
    if matches!(surface, PathCandidateSurface::Code) && blocked_or_non_line_scheme(path) {
        return None;
    }
    let segments = path.split(['/', '\\']).collect::<Vec<&str>>();
    if segments.iter().any(|segment| *segment == "..") {
        return None;
    }
    if segments
        .iter()
        .all(|segment| segment.is_empty() || *segment == ".")
    {
        return None;
    }
    if matches!(surface, PathCandidateSurface::Code) && !is_code_surface_path_like(path, line) {
        return None;
    }
    Some(PathCandidate {
        path: path.to_owned(),
        line,
        column,
    })
}

fn has_rfc_scheme(text: &str) -> bool {
    let Some((prefix, _)) = text.split_once(':') else {
        return false;
    };
    rfc_scheme_prefix(prefix)
}

fn blocked_or_non_line_scheme(text: &str) -> bool {
    let Some((prefix, rest)) = text.split_once(':') else {
        return false;
    };
    if !rfc_scheme_prefix(prefix) {
        return false;
    }
    if matches!(
        prefix.to_ascii_lowercase().as_str(),
        "javascript" | "data" | "file" | "http" | "https" | "mailto" | "vbscript" | "blob"
    ) {
        return true;
    }
    !line_col_or_range_payload(rest)
}

fn rfc_scheme_prefix(prefix: &str) -> bool {
    let mut chars = prefix.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

fn line_col_or_range_payload(rest: &str) -> bool {
    if rest.is_empty() {
        return false;
    }
    if rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    if let Some((left, right)) = rest.split_once(':') {
        return !left.is_empty()
            && !right.is_empty()
            && left.bytes().all(|byte| byte.is_ascii_digit())
            && right.bytes().all(|byte| byte.is_ascii_digit());
    }
    if let Some((left, right)) = rest.split_once('-') {
        return !left.is_empty()
            && !right.is_empty()
            && left.bytes().all(|byte| byte.is_ascii_digit())
            && right.bytes().all(|byte| byte.is_ascii_digit());
    }
    false
}

fn is_code_surface_path_like(path: &str, line: Option<usize>) -> bool {
    if line.is_some() {
        return true;
    }
    if path.contains('/') || path.contains('\\') {
        return true;
    }
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some(dot) = base.rfind('.') else {
        return false;
    };
    if dot == 0 || dot + 1 == base.len() {
        return false;
    }
    let ext = &base[dot + 1..];
    if ext.len() > 10 || !ext.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return false;
    }
    if ext.len() >= 2 {
        return true;
    }
    matches!(ext.to_ascii_lowercase().as_str(), "c" | "h" | "r" | "v")
}

/// Collect every workspace-path candidate mentioned in a markdown fragment:
/// inline code spans and relative markdown-link targets, both skipped inside
/// fenced code blocks (the Web interceptor only fires below `.md-fence`).
/// Order-stable, deduplicated by `(path, line, column)`, capped at
/// [`MAX_CANDIDATES`].
pub(crate) fn scan_markdown_path_candidates(text: &str) -> Vec<PathCandidate> {
    let mut candidates: Vec<PathCandidate> = Vec::new();
    for raw in markdown_candidate_strings(text) {
        let candidate = match raw {
            RawCandidate::Code(text) => parse_workspace_path_on(&text, PathCandidateSurface::Code),
            RawCandidate::Href(text) => parse_workspace_path_on(&text, PathCandidateSurface::Href),
        };
        if let Some(candidate) = candidate
            && !candidates.contains(&candidate)
        {
            candidates.push(candidate);
            if candidates.len() >= MAX_CANDIDATES {
                break;
            }
        }
    }
    candidates
}

enum RawCandidate {
    Code(String),
    Href(String),
}

fn markdown_candidate_strings(text: &str) -> Vec<RawCandidate> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        scan_line_candidates(line, &mut found);
    }
    found
}

fn scan_line_candidates(line: &str, found: &mut Vec<RawCandidate>) {
    let mut rest = line;
    while !rest.is_empty() {
        match next_candidate_span(rest) {
            Some((candidate, consumed_end)) => {
                found.push(candidate);
                rest = &rest[consumed_end..];
            }
            None => break,
        }
    }
}

/// One raw inline-code-span text or markdown link target plus the byte
/// offset where the span ends, so the caller can continue past it.
fn next_candidate_span(rest: &str) -> Option<(RawCandidate, usize)> {
    match (rest.find('`'), rest.find("](")) {
        (None, None) => None,
        (None, Some(link)) => link_span(rest, link),
        (Some(code), Some(link)) if code < link => code_span(rest, code),
        (_, Some(link)) => link_span(rest, link),
        (Some(code), None) => code_span(rest, code),
    }
}

fn code_span(rest: &str, code: usize) -> Option<(RawCandidate, usize)> {
    let close = code + 1 + rest[code + 1..].find('`')?;
    Some((RawCandidate::Code(rest[code + 1..close].to_owned()), close + 1))
}

fn link_span(rest: &str, link: usize) -> Option<(RawCandidate, usize)> {
    let close = link + 2 + rest[link + 2..].find(')')?;
    Some((RawCandidate::Href(rest[link + 2..close].to_owned()), close + 1))
}

#[cfg(test)]
impl FileViewer {
    pub(crate) fn push_test_tab(
        &mut self,
        path: &str,
        first_line: usize,
        lines: Vec<String>,
        end_of_file: bool,
    ) {
        self.tabs.push(FileTab {
            path: path.to_owned(),
            page: Page {
                first_line,
                lines,
                end_of_file,
            },
            scroll: 0,
        });
        self.active = self.tabs.len() - 1;
    }
}

/// One loaded page of the active file.
#[derive(Debug)]
struct Page {
    /// 1-based file line of the page's first row.
    first_line: usize,
    lines: Vec<String>,
    end_of_file: bool,
}

impl Page {
    fn last_line(&self) -> usize {
        self.first_line + self.lines.len().saturating_sub(1)
    }

    /// First line of the page after this one, when one exists.
    fn next_first_line(&self) -> Option<usize> {
        (!self.end_of_file).then(|| self.first_line + self.lines.len())
    }

    /// First line of a full page before this one, when this page is not the
    /// head (mirrors the Web drawer's backward pagination step).
    fn prev_first_line(&self) -> Option<usize> {
        (self.first_line > 1)
            .then(|| self.first_line.saturating_sub(file_explorer::DEFAULT_PAGE_LINES))
            .map(|start| start.max(1))
            .filter(|start| *start < self.first_line)
    }
}

/// One open file: its canonical workspace-relative path plus the paged state.
#[derive(Debug)]
struct FileTab {
    path: String,
    page: Page,
    scroll: usize,
}

#[derive(Debug, Default)]
pub(crate) struct FileViewer {
    tabs: Vec<FileTab>,
    active: usize,
}

impl FileViewer {
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub(crate) fn active(&self) -> Option<&FileTab> {
        self.tabs.get(self.active)
    }

    fn active_tab_mut(&mut self) -> Option<&mut FileTab> {
        self.tabs.get_mut(self.active)
    }

    /// Cycle among the open tabs (Web drawer's file-tabs parity).
    pub(crate) fn cycle_tab(&mut self, delta: isize) {
        let count = self.tabs.len() as isize;
        if count == 0 {
            return;
        }
        self.active = ((self.active as isize + delta).rem_euclid(count)) as usize;
    }

    /// Load a page from `path` starting at `first_line` (1-based) through the
    /// workspace-scoped explorer read, and open or refresh its tab. Returns
    /// the canonical relative path (surfaced for status messages).
    pub(crate) fn open(
        &mut self,
        application: &Application,
        path: &str,
        first_line: usize,
    ) -> anyhow::Result<String> {
        let (canonical, page) = load_page(application, path, first_line)?;
        Ok(self.admit_tab(canonical, page))
    }

    /// Insert or refresh the tab for `path` and make it active; evicts the
    /// oldest tab beyond [`MAX_OPEN_FILES`] (bounded open-file set, like the
    /// Web drawer's per-session tabs). Returns the stored path.
    fn admit_tab(&mut self, path: String, page: Page) -> String {
        if let Some(existing) = self.tabs.iter().position(|tab| tab.path == path) {
            self.tabs[existing] = FileTab {
                path: path.clone(),
                page,
                scroll: 0,
            };
            self.active = existing;
            return path;
        }
        if self.tabs.len() >= MAX_OPEN_FILES {
            self.tabs.remove(0);
        }
        self.tabs.push(FileTab {
            path: path.clone(),
            page,
            scroll: 0,
        });
        self.active = self.tabs.len() - 1;
        path
    }

    /// Jump the active tab back to line 1.
    pub(crate) fn goto_top(&mut self, application: &Application) -> anyhow::Result<()> {
        let path = self
            .active()
            .map(|tab| tab.path.clone())
            .ok_or_else(|| anyhow::anyhow!("file viewer is empty"))?;
        let (_, page) = load_page(application, &path, 1)?;
        if let Some(tab) = self.active_tab_mut() {
            tab.page = page;
            tab.scroll = 0;
        }
        Ok(())
    }

    /// Move the reading window down `rows` displays, rolling across page
    /// boundaries while a follow-up page exists.
    pub(crate) fn scroll_down(
        &mut self,
        application: &Application,
        rows: usize,
    ) -> anyhow::Result<bool> {
        let mut moved = false;
        for _ in 0..rows.max(1) {
            let Some(tab) = self.active_tab_mut() else {
                break;
            };
            if tab.scroll + 1 < tab.page.lines.len() {
                tab.scroll += 1;
                moved = true;
                continue;
            }
            let Some(next_first) = tab.page.next_first_line() else {
                break;
            };
            let (canonical, page) = load_page(application, &tab.path, next_first)?;
            tab.page = page;
            tab.scroll = 0;
            tab.path = canonical;
            moved = true;
        }
        Ok(moved)
    }

    /// Move the reading window up `rows` displays, loading the previous page
    /// when the viewport crosses the page top.
    pub(crate) fn scroll_up(
        &mut self,
        application: &Application,
        rows: usize,
    ) -> anyhow::Result<bool> {
        let mut moved = false;
        for _ in 0..rows.max(1) {
            let Some(tab) = self.active_tab_mut() else {
                break;
            };
            if tab.scroll > 0 {
                tab.scroll -= 1;
                moved = true;
                continue;
            }
            let Some(prev_first) = tab.page.prev_first_line() else {
                break;
            };
            let (canonical, page) = load_page(application, &tab.path, prev_first)?;
            tab.scroll = page.lines.len().saturating_sub(1);
            tab.page = page;
            tab.path = canonical;
            moved = true;
        }
        Ok(moved)
    }
}

/// Read one bounded page through the existing workspace-scoped explorer
/// read (PolicyFs authority, no-follow handles, 400-line default cap).
fn load_page(
    application: &Application,
    path: &str,
    first_line: usize,
) -> anyhow::Result<(String, Page)> {
    let response = file_explorer::read(application, path, first_line.saturating_sub(1), None)?;
    Ok((
        response.path,
        Page {
            first_line,
            lines: response.lines,
            end_of_file: response.next_cursor.is_none(),
        },
    ))
}

/// Geometry of the file-viewer overlay: the centered frame area plus the
/// content rows/width available inside it.
pub(crate) struct ViewerLayout {
    pub(crate) area: Rect,
    pub(crate) rows: usize,
    pub(crate) width: usize,
}
/// (wide terminals cap at 120 columns, narrow ones keep at least 20).
pub(crate) fn viewer_layout(frame_area: Rect) -> ViewerLayout {
    let width = frame_area.width.saturating_sub(4).min(120).max(20).min(frame_area.width.max(20));
    let height = frame_area
        .height
        .saturating_sub(2)
        .min(60)
        .max(7)
        .min(frame_area.height.max(7));
    let area = centered_rect(width, height, frame_area);
    // borders(2) + tab strip(1) + position row(1) + hint(1).
    let rows = usize::from(area.height.saturating_sub(5)).max(1);
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    ViewerLayout { area, rows, width }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area.y.saturating_add(area.height.saturating_sub(height) / 2),
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

/// Render the file-viewer overlay: tab strip, page position row, a window of
/// the active page with a line-number gutter, and the key hint row. Lines
/// wider than the frame are clipped by the terminal (never wrapped), so both
/// wide and narrow layouts stay bounded.
pub(crate) fn render_file_viewer(frame: &mut ratatui::Frame<'_>, viewer: &FileViewer, theme: Theme) {
    let Some(tab) = viewer.active() else {
        return;
    };
    let layout = viewer_layout(frame.area());
    frame.render_widget(Clear, layout.area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(layout.area);
    frame.render_widget(block, layout.area);

    let width = layout.width;
    let mut lines = Vec::with_capacity(layout.rows + 3);

    // Tab strip: every open file, active tab bright (clipped at one row).
    let mut strip = Vec::new();
    for (index, open) in viewer.tabs.iter().enumerate() {
        if index > 0 {
            strip.push(Span::styled(" · ", Style::default().fg(theme.dim)));
        }
        let active = index == viewer.active;
        let label = if active {
            format!("▪ {}", open.path)
        } else {
            open.path.clone()
        };
        strip.push(Span::styled(
            label,
            Style::default()
                .fg(if active { theme.accent } else { theme.dim })
                .add_modifier(if active { Modifier::BOLD } else { Modifier::empty() }),
        ));
    }
    lines.push(Line::from(strip));

    // Page position row.
    let end_of_file = tab.page.end_of_file;
    let position = format!(
        "lines {}–{}{}",
        tab.page.first_line,
        tab.page.last_line(),
        if end_of_file { " · end" } else { "" }
    );
    lines.push(Line::from(Span::styled(
        position,
        Style::default().fg(theme.dim),
    )));

    // Content window of the active page with a line-number gutter.
    let shown = tab.page.lines[tab.scroll.min(tab.page.lines.len())..]
        .iter()
        .take(layout.rows);
    let first_number = tab.page.first_line + tab.scroll.min(tab.page.lines.len());
    let last_number = first_number + layout.rows.min(tab.page.lines.len().saturating_sub(tab.scroll.min(tab.page.lines.len()))).saturating_sub(1);
    let gutter = last_number.max(first_number).to_string().len().max(3);
    let body_width = width.saturating_sub(gutter + 2).max(1);
    for (offset, line) in shown.enumerate() {
        let number = first_number + offset;
        lines.push(Line::from(vec![
            Span::styled(
                format!("{number:>gutter$} "),
                Style::default().fg(theme.dim),
            ),
            Span::styled("│ ", Style::default().fg(theme.border)),
            Span::styled(
                file_line_display(line, body_width),
                Style::default().fg(theme.text),
            ),
        ]));
    }

    // Hint row.
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k scroll · PgUp/PgDn page · g top · Tab/BackTab files · q/Esc close",
        Style::default().fg(theme.dim),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Bounded display projection of one file line: tabs become four columns,
/// control characters are dropped, and the line is grapheme-clipped with an
/// ellipsis when it exceeds `width` columns.
fn file_line_display(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut columns = 0usize;
    for character in line.chars() {
        match character {
            '\t' => {
                out.push_str("    ");
                columns += 4;
            }
            c if c.is_control() => {}
            c => {
                let display = c.width().unwrap_or(0);
                if columns + display > width {
                    out.push('…');
                    break;
                }
                columns += display;
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Option<PathCandidate> {
        parse_workspace_path_candidate(text)
    }

    #[test]
    fn parser_mirrors_web_candidate_semantics() {
        // Mirrors markdown.test.ts parseWorkspacePathCandidate cases.
        let relative = parse("src/app.ts").expect("relative file");
        assert_eq!(relative.path, "src/app.ts");
        assert_eq!(relative.line, None);

        let with_line = parse("crates/pi-cli/web/src/markdown.ts:42").expect("path:line");
        assert_eq!(with_line.path, "crates/pi-cli/web/src/markdown.ts");
        assert_eq!(with_line.line, Some(42));

        let with_col = parse("src/lib.rs:10:4").expect("path:line:col");
        assert_eq!(with_col.path, "src/lib.rs");
        assert_eq!(with_col.line, Some(10));
        assert_eq!(with_col.column, Some(4));

        assert!(parse("javascript:alert(1)").is_none());
        assert!(parse("data:text/plain,hi").is_none());
        assert!(parse("file:///etc/passwd").is_none());
        assert!(parse("https://example.com/x.rs").is_none());
        assert!(parse("/etc/passwd").is_none());
        assert!(parse("../secret.ts").is_none());
        assert!(parse("src/../secret.ts").is_none());

        assert_eq!(parse("src/app.ts.").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts,").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts)").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts:").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts`").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts;").unwrap().path, "src/app.ts");
        assert_eq!(parse("src/app.ts:12).").unwrap().line, Some(12));

        let clamped_zero = parse("src/app.ts:0").expect("zero line clamps");
        assert_eq!(clamped_zero.line, Some(1));
        let clamped_neg = parse("src/app.ts:0:0").expect("zero line:col clamps");
        assert_eq!(clamped_neg.line, Some(1));
        assert_eq!(clamped_neg.column, Some(1));

        // Bounded inputs: reject control characters and whitespace.
        assert!(parse("src/a\nb.rs").is_none());
        assert!(parse("src/a\u{7f}b.rs").is_none());
        // Bounded length: > 512 characters is inert, exactly 512 parses.
        let long = "a".repeat(513);
        assert!(parse(&long).is_none());
        let full = "a/".repeat(255) + "b";
        assert_eq!(parse(&full).expect("512 chars").path.chars().count(), 511);
        // Empty/degenerate segments.
        assert!(parse(".").is_none());
        assert!(parse("..").is_none());
        assert!(parse(":5").is_none());

        assert!(parse("foo").is_none());
        assert!(parse("foo()").is_none());
        assert!(parse("x.y").is_none());
        assert!(parse("SomeType").is_none());
        assert!(parse("guta_updates_backup/").is_none());
        assert!(parse("javascript:12").is_none());

        let makefile = parse("Makefile:12").expect("Makefile:12");
        assert_eq!(makefile.path, "Makefile");
        assert_eq!(makefile.line, Some(12));

        let app_ts = parse("app.ts").expect("known extension");
        assert_eq!(app_ts.path, "app.ts");
        assert_eq!(app_ts.line, None);

        let main_c = parse("main.c").expect("1-char known language");
        assert_eq!(main_c.path, "main.c");

        let bin = parse("checkpoint_tree.bin").expect("unknown 2+ suffix");
        assert_eq!(bin.path, "checkpoint_tree.bin");
        assert_eq!(bin.line, None);

        let range = parse("node_start_config.rs:37-41").expect("line range");
        assert_eq!(range.path, "node_start_config.rs");
        assert_eq!(range.line, Some(37));
        assert_eq!(range.column, None);

        assert!(parse("src/app.ts/").is_none());
    }

    #[test]
    fn screenshot_tokens_keep_files_and_drop_trailing_slash_dirs() {
        let text = "`guta_updates_backup/` then `checkpoint_tree.bin` and `node_start_config.rs:37-41`.";
        let found = scan_markdown_path_candidates(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].path, "checkpoint_tree.bin");
        assert_eq!(found[0].line, None);
        assert_eq!(found[1].path, "node_start_config.rs");
        assert_eq!(found[1].line, Some(37));
    }

    #[test]
    fn scanner_href_rejects_rfc_schemes_that_code_spans_may_keep() {
        let text = "[x](ftp:12) then `Makefile:12`.";
        let found = scan_markdown_path_candidates(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "Makefile");
        assert_eq!(found[0].line, Some(12));
    }

    #[test]
    fn rightmost_numeric_tail_wins_over_lazy_regex_parity() {
        // Path may itself contain `:digits` (lazy-regex parity): the split
        // lands at the rightmost valid `:digits` boundary...
        let parsed = parse("src/a:1:2:3").expect("nested tails");
        assert_eq!(parsed.path, "src/a:1");
        assert_eq!(parsed.line, Some(2));
        assert_eq!(parsed.column, Some(3));
        // ...but when the residual path grows a scheme-shaped `x:` prefix the
        // Web parser rejects it (scheme test on the final path), so do we.
        assert!(parse("a:1:2:3").is_none());
        // A lone numeric tail yields the line; two numeric tails as in
        // `a:1:3` are line+column, exactly like the Web regex.
        let parsed = parse("x/a:3").expect("one tail");
        assert_eq!(parsed.path, "x/a");
        assert_eq!(parsed.line, Some(3));
    }

    #[test]
    fn scanner_collects_code_spans_and_links_outside_fences() {
        let text = "See `src/a.rs:12` and [docs](docs/guide.md) first.\n\
                    ```rust\nlet bad = \"`etc/passwd:1`\";\n```\n\
                    Also `src/a.rs:12` (duplicate) and `../escape.rs`.";
        let found = scan_markdown_path_candidates(text);
        // The duplicate code span dedupes to one candidate; the guarded
        // fence line contributes nothing.
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].path, "src/a.rs");
        assert_eq!(found[0].line, Some(12));
        assert_eq!(found[1].path, "docs/guide.md");
        assert!(found.iter().all(|c| c.path != "etc/passwd"));
        // Traversal candidates are never surfaced.
        assert!(!found.iter().any(|c| c.path.contains("..")));
    }

    #[test]
    fn scanner_caps_candidates() {
        let text = (0..MAX_CANDIDATES + 10)
            .map(|index| format!("`dir/file{index}.rs`"))
            .collect::<Vec<_>>()
            .join(" ");
        let found = scan_markdown_path_candidates(&text);
        assert_eq!(found.len(), MAX_CANDIDATES);
    }

    #[test]
    fn page_steps_step_full_pages_like_the_web_drawer() {
        let page = Page {
            first_line: 1,
            lines: (0..400).map(|index| format!("line {index}")).collect(),
            end_of_file: false,
        };
        assert_eq!(page.next_first_line(), Some(401));
        assert_eq!(page.prev_first_line(), None);

        let page = Page {
            first_line: 801,
            lines: (0..200).map(|index| format!("line {index}")).collect(),
            end_of_file: true,
        };
        assert_eq!(page.next_first_line(), None);
        assert_eq!(
            page.prev_first_line(),
            Some(801 - 400),
            "backward step mirrors the Web 400-line cursor walk"
        );
    }
    #[test]
    fn open_caps_tabs_and_cycles() {
        let mut viewer = FileViewer::default();
        for index in 0..=MAX_OPEN_FILES {
            let page = Page {
                first_line: index + 1,
                lines: vec!["line".to_owned()],
                end_of_file: true,
            };
            viewer.admit_tab(format!("file{index}.rs"), page);
        }
        assert_eq!(viewer.tab_count(), MAX_OPEN_FILES, "oldest tab evicted");
        assert_eq!(
            viewer.active().map(|tab| tab.path.as_str()),
            Some("file8.rs")
        );
        viewer.cycle_tab(1);
        assert_eq!(
            viewer.active().map(|tab| tab.path.as_str()),
            Some("file1.rs"),
            "cycling wraps to the oldest retained tab"
        );
        viewer.cycle_tab(-1);
        assert_eq!(
            viewer.active().map(|tab| tab.path.as_str()),
            Some("file8.rs")
        );
    }


    #[test]
    fn viewer_layout_stays_bounded_wide_and_narrow() {
        let wide = viewer_layout(Rect::new(0, 0, 600, 300));
        assert_eq!(wide.area.width, 120);
        assert!(wide.area.height <= 60);
        assert!(wide.rows >= 1 && wide.width >= 1);

        let narrow = viewer_layout(Rect::new(0, 0, 20, 10));
        assert!(narrow.area.width <= 20);
        assert!(narrow.area.height <= 10);
        assert!(narrow.rows >= 1 && narrow.width >= 1);
    }

    #[test]
    fn file_line_display_is_bounded_and_expands_tabs() {
        assert_eq!(file_line_display("a\tb", 80), "a    b");
        assert_eq!(file_line_display("compact\u{1}line", 80), "compactline");
        let truncated = file_line_display(&"x".repeat(100), 10);
        assert!(truncated.starts_with('x'));
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().count() <= 11);
    }

    fn buffer_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer.get(x, y).symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn viewer_render_shows_page_window_with_line_numbers() {
        use ratatui::Terminal;

        let mut viewer = FileViewer::default();
        let lines: Vec<String> = (0..30).map(|index| format!("let x{index} = {index};")).collect();
        viewer.push_test_tab("src/b.rs", 1, vec!["second".to_owned()], true);
        viewer.push_test_tab("src/a.rs", 100, lines, true);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render_file_viewer(frame, &viewer, crate::theme::DARK))
            .expect("draw file viewer");
        let text = buffer_text(&terminal);
        assert!(text.contains("src/a.rs"), "{text}");
        assert!(text.contains("src/b.rs"), "tab strip lists every open file");
        assert!(text.contains("lines 100–"), "{text}");
        assert!(text.contains("100 │ let x0 = 0;"), "{text}");
    }
}
