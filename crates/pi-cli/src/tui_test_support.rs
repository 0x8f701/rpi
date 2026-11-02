//! Test-support seams for integration tests that drive shipped TUI rendering
//! through ratatui's `TestBackend`. `#[doc(hidden)]`: not public API, exists
//! so `tests/` can assert grid-level fidelity without reaching into `tui.rs`
//! internals.

use pi_ai::Model;
use pi_coding::SessionContextUsage;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::job_card_adapter::TaskCardRows;
use crate::theme::{self, Theme};
use crate::tool_card_adapter::ToolCardRows;

/// Build a fixture catalog model (mirrors the unit-test helper in `tui.rs`).
#[doc(hidden)]
pub fn selector_frame_fixture(provider: &str, id: &str, name: &str) -> Model {
    Model {
        provider: provider.to_owned(),
        id: id.to_owned(),
        name: name.to_owned(),
        context_window: 128_000,
        max_tokens: 128_000,
        ..Model::default()
    }
}

/// Render the active /model picker overlay at the given terminal size and
/// return the full grid as text (one line per terminal row).
///
/// # Panics
/// Panics if the model reference is not present in `models`.
#[doc(hidden)]
pub fn model_selector_screen(
    models: &[Model],
    current_reference: &str,
    width: u16,
    height: u16,
) -> String {
    let current = models
        .iter()
        .find(|model| format!("{}/{}", model.provider, model.id) == current_reference)
        .unwrap_or_else(|| panic!("fixture missing {current_reference}"))
        .clone();
    let selector = crate::scoped_model_selector::ScopedModelSelector::new_positioned(
        models.to_vec(),
        Some(&current),
    );
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            crate::tui::render_active_model_selector_for_test(frame, &selector, current_reference);
        })
        .expect("draw");
    grid_text(terminal.backend(), width)
}

/// Uniform live-region inset used by production layout (`live_content_width`):
/// two outer columns of padding so cards never sit flush against the terminal
/// edge. Widths of 2 or less keep the original value.
#[doc(hidden)]
#[must_use]
pub fn live_outer_content_width(width: u16) -> u16 {
    if width > 2 { width - 2 } else { width }
}

/// Compact git `⑂` segment body matching production `footer_git_text`:
/// `{branch}*{modified}+{staged}?{untracked}`, each count omitted when zero.
#[doc(hidden)]
#[must_use]
pub fn footer_git_status_text(
    branch: Option<&str>,
    staged: u32,
    modified: u32,
    untracked: u32,
) -> Option<String> {
    let branch = branch.map(str::trim).filter(|branch| !branch.is_empty())?;
    let mut text = branch.to_owned();
    if modified > 0 {
        text.push_str(&format!("*{modified}"));
    }
    if staged > 0 {
        text.push_str(&format!("+{staged}"));
    }
    if untracked > 0 {
        text.push_str(&format!("?{untracked}"));
    }
    Some(text)
}

/// Compact `◫` context segment matching production `footer_context_text`:
/// `{percent}% {tokens}/{window}` with a `k` suffix for thousands.
#[doc(hidden)]
#[must_use]
pub fn footer_context_status_text(usage: &SessionContextUsage) -> Option<String> {
    if usage.context_window <= 0 {
        return None;
    }
    let percent = usage
        .percent
        .map(|value| value.round().clamp(0.0, 999.0) as u32)
        .unwrap_or(0);
    Some(format!(
        "{}% {}/{}",
        percent,
        footer_token_k(usage.tokens.unwrap_or(0).max(0)),
        footer_token_k(usage.context_window)
    ))
}

fn footer_token_k(n: i64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// Compact warning banner matching production `composer_error_toast_lines`
/// for `composer_error_is_warning`: `Warning: ` + word-wrapped body, at most
/// two rows, warning fg on `custom_message_bg`, width-filled.
#[doc(hidden)]
#[must_use]
pub fn composer_warning_chrome(message: &str, width: u16, theme: Theme) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let clean = format!(
        "Warning: {}",
        crate::tui::clean_terminal_text(message)
            .lines()
            .collect::<Vec<_>>()
            .join(" ")
    );
    let wrapped = pi_coding::markdown::wrap_text(&clean, width);
    let style = Style::default()
        .fg(theme.warning)
        .bg(theme.custom_message_bg);
    wrapped
        .into_iter()
        .take(2)
        .map(|row| {
            let row = pi_coding::markdown::fit_text(&row, width);
            let fill = " ".repeat(width.saturating_sub(pi_coding::markdown::display_width(&row)));
            Line::from(vec![
                Span::styled(row, style.add_modifier(Modifier::BOLD)),
                Span::styled(fill, style),
            ])
        })
        .collect()
}

/// Default OMP titanium palette used by production `render_tool_card`.
#[doc(hidden)]
#[must_use]
pub fn omp_default_theme() -> Theme {
    theme::DARK
}

/// Render ToolCard lines through the production `tui.rs::render_tool_card`
/// path. Production titles come from `ToolCardRows::summary`. Pass the real
/// compact and expanded adapter projections; `expand` selects which card
/// production paints (hub/IRC body limits and `ToolTranscript` card choice).
#[doc(hidden)]
#[must_use]
pub fn render_tool_card_lines(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    theme: Theme,
    width: u16,
    expand: bool,
) -> Vec<Line<'static>> {
    crate::tui::render_tool_card_lines_for_test(
        compact.clone(),
        expanded.clone(),
        expand,
        theme,
        width,
    )
}

/// Paint the production ToolCard renderer onto a `TestBackend` grid.
#[doc(hidden)]
#[must_use]
pub fn render_tool_card_screen(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    width: u16,
    height: u16,
    expand: bool,
) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let lines = render_tool_card_lines(compact, expanded, omp_default_theme(), width, expand);
    terminal
        .draw(|frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new(lines), frame.area());
        })
        .expect("draw");
    terminal
}

/// Render Task/Job card lines through production `tui.rs::render_job_card`.
#[doc(hidden)]
#[must_use]
pub fn render_job_card_lines(card: &TaskCardRows, theme: Theme, width: u16) -> Vec<Line<'static>> {
    crate::tui::render_job_card_lines_for_test(card.clone(), theme, width)
}
fn grid_text(backend: &ratatui::backend::TestBackend, width: u16) -> String {
    backend
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
