//! Side-chat overlay renderer for `/btw`.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::side_chat::{SideChatController, SideChatEntry, SideChatRole};
use crate::theme::Theme;
use crate::tui::clean_terminal_text;

/// Render the side-chat overlay into `frame`.
pub fn render_side_chat_panel(
    frame: &mut ratatui::Frame<'_>,
    controller: &SideChatController,
    theme: Theme,
) {
    let area = centered_rect(frame.area().width.saturating_mul(9) / 10, frame.area().height.saturating_mul(4) / 5, frame.area());
    frame.render_widget(Clear, area);

    let mode = controller.tool_mode().label();
    let title = if controller.show_edit_warning() {
        format!(" Side chat · EDIT MODE · {mode} · Ctrl+T toggle · Esc close ")
    } else {
        format!(" Side chat · {mode} · Ctrl+T edit · Esc close ")
    };
    let border_color = if controller.show_edit_warning() {
        theme.error
    } else {
        theme.border_accent
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(if controller.show_edit_warning() { 2 } else { 0 }),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.trim().to_owned(),
            Style::default()
                .fg(if controller.show_edit_warning() {
                    theme.error
                } else {
                    theme.accent
                })
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    let transcript_width = usize::from(chunks[1].width.saturating_sub(1)).max(1);
    let transcript_height = usize::from(chunks[1].height).max(1);
    let lines = build_transcript_lines(controller, theme, transcript_width);
    let total = lines.len();
    let scroll = controller.scroll().min(total.saturating_sub(1));
    let start = total.saturating_sub(transcript_height + scroll);
    let end = (start + transcript_height).min(total);
    let visible = lines
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible).wrap(Wrap { trim: false }),
        chunks[1],
    );

    if controller.show_edit_warning() && chunks[2].height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ⚠ EDIT MODE: Write/Exec enabled — may overlap main agent file changes ",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[2],
        );
    }

    if chunks[3].height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", clean_terminal_text(controller.status())),
                Style::default().fg(theme.muted),
            ))),
            chunks[3],
        );
    }

    let editor_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border));
    let editor_inner = editor_block.inner(chunks[4]);
    frame.render_widget(editor_block, chunks[4]);
    let editor_text = clean_terminal_text(&controller.editor_text());
    let prompt = if editor_text.is_empty() && !controller.is_streaming() {
        "Type a side-chat message…"
    } else {
        editor_text.as_str()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("> {prompt}"),
            Style::default().fg(if editor_text.is_empty() {
                theme.muted
            } else {
                theme.text
            }),
        ))),
        editor_inner,
    );

    // Place cursor in the side editor when idle. Column is a raw byte offset;
    // map the prefix through the same sanitizer used for display so stripped
    // CSI/OSC bytes cannot push the caret past the visible text.
    if !controller.is_streaming() {
        let (row, column) = controller.editor_cursor();
        let line = controller
            .editor_lines()
            .get(row)
            .map(String::as_str)
            .unwrap_or("");
        let prefix = &line[..column.min(line.len())];
        let display_column = UnicodeWidthStr::width(clean_terminal_text(prefix).as_str());
        let cursor_x = editor_inner.x.saturating_add(2).saturating_add(
            u16::try_from(display_column.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
        );
        let cursor_y = editor_inner.y;
        if cursor_x < editor_inner.right() && cursor_y < editor_inner.bottom() {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn build_transcript_lines(
    controller: &SideChatController,
    theme: Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in controller.entries() {
        lines.extend(entry_lines(entry, theme, width));
    }
    append_streaming_lines(&mut lines, controller.streaming_text(), theme, width);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Forked side chat. Messages stay here and never merge into main.",
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(Span::styled(
            "  Default tools are read-only. Ctrl+T enables edit mode with a visible warning.",
            Style::default().fg(theme.muted),
        )));
    }
    lines
}

fn append_streaming_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    theme: Theme,
    width: usize,
) {
    if text.is_empty() {
        return;
    }
    lines.push(role_line("side", theme.accent, theme));
    let streaming = clean_terminal_text(text);
    for row in wrap_text(&streaming, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(
            format!("  {row}"),
            Style::default().fg(theme.text),
        )));
    }
}

fn entry_lines(entry: &SideChatEntry, theme: Theme, width: usize) -> Vec<Line<'static>> {
    let (label, color) = match entry.role {
        SideChatRole::User => ("you", theme.accent),
        SideChatRole::Assistant => ("side", theme.accent),
        SideChatRole::Tool => ("tool", theme.muted),
        SideChatRole::System => ("sys", if entry.is_error { theme.error } else { theme.muted }),
    };
    let mut lines = vec![role_line(label, color, theme)];
    let style = Style::default().fg(if entry.is_error {
        theme.error
    } else {
        theme.text
    });
    for row in wrap_text(&clean_terminal_text(&entry.text), width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {row}"), style)));
    }
    lines
}

fn role_line(label: &str, color: ratatui::style::Color, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("·", Style::default().fg(theme.muted)),
    ])
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for raw in text.replace('\r', "").split('\n') {
        if raw.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
                continue;
            }
            let candidate = format!("{current} {word}");
            if UnicodeWidthStr::width(candidate.as_str()) <= width {
                current = candidate;
            } else {
                rows.push(current);
                current = word.to_owned();
            }
        }
        if !current.is_empty() {
            rows.push(current);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width).max(20);
    let height = height.min(area.height).max(8);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::Model;
    use pi_coding::{Application, create_coding_tools};
    use pi_agent::ThinkingLevel;
    use ratatui::{Terminal, backend::TestBackend};

    fn test_session(cwd: &std::path::Path) -> pi_coding::Session {
        pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: "main system".to_owned(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(create_coding_tools(&cwd.to_string_lossy())),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session")
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect::<String>()
    }

    fn assert_no_unsafe_controls(text: &str) {
        assert!(
            !text.contains('\u{1b}'),
            "escape byte leaked into side panel buffer: {text:?}"
        );
        assert!(
            !text.chars().any(|ch| ch.is_control() && ch != '\n'),
            "unsafe control leaked into side panel buffer: {text:?}"
        );
    }

    fn lines_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn clean_terminal_text_strips_csi_cursor_and_osc8_hyperlinks() {
        // SGR color + cursor positioning CSI
        assert_eq!(
            clean_terminal_text("pre\u{1b}[31mRED\u{1b}[0m\u{1b}[2;5Hpost"),
            "preREDpost"
        );
        // OSC 8 hyperlink terminated by BEL
        assert_eq!(
            clean_terminal_text("go\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}end"),
            "golinkend"
        );
        // OSC 8 hyperlink terminated by ST (ESC \)
        assert_eq!(
            clean_terminal_text("go\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\end"),
            "golinkend"
        );
        // Tabs expand; newlines and visible Unicode remain
        assert_eq!(
            clean_terminal_text("a\tb\n你好\u{1b}[1mc"),
            "a    b\n你好c"
        );
        // Bare ESC and other unsafe controls disappear (not replaced with glyphs)
        assert_eq!(clean_terminal_text("x\u{1b}y\u{7}z\u{8}w"), "xyzw");
    }

    #[test]
    fn entry_lines_sanitize_before_wrapping_all_roles() {
        let theme = crate::theme::DARK;
        let samples = [
            (
                SideChatRole::User,
                "ask \u{1b}[31mcolor\u{1b}[0m \u{1b}[1;1Hhere",
                "ask color here",
            ),
            (
                SideChatRole::Assistant,
                "see \u{1b}]8;;https://x.test\u{7}label\u{1b}]8;;\u{7} ok",
                "see label ok",
            ),
            (
                SideChatRole::Tool,
                "[read] \u{1b}]8;;https://y.test\u{1b}\\path\u{1b}]8;;\u{1b}\\ done",
                "[read] path done",
            ),
            (
                SideChatRole::System,
                "err\u{1b}[2J\u{7}or",
                "error",
            ),
        ];
        for (role, raw, expected) in samples {
            let text = lines_text(&entry_lines(
                &SideChatEntry {
                    role,
                    text: raw.to_owned(),
                    is_error: matches!(role, SideChatRole::System),
                    is_partial: false,
                },
                theme,
                80,
            ));
            assert!(
                text.contains(expected),
                "role {role:?}: expected visible {expected:?} in {text:?}"
            );
            assert_no_unsafe_controls(&text);
        }
    }

    #[test]
    fn streaming_lines_sanitize_before_wrapping() {
        let theme = crate::theme::DARK;
        let mut lines = Vec::new();
        append_streaming_lines(
            &mut lines,
            "live \u{1b}[31mstream\u{1b}[0m \u{1b}[2;5H\u{1b}]8;;https://stream.test\u{7}label\u{1b}]8;;\u{7} \u{1b}]8;;https://stream.test\u{1b}\\st\u{1b}]8;;\u{1b}\\",
            theme,
            80,
        );
        let text = lines_text(&lines);
        assert!(text.contains("live stream label st"), "visible streaming payload missing: {text:?}");
        assert!(
            !text.contains("https://stream.test"),
            "hyperlink target must not render: {text:?}"
        );
        assert_no_unsafe_controls(&text);
        assert!(
            !text.contains("[31m") && !text.contains("[0m") && !text.contains("[2;5H"),
            "raw CSI payload leaked from streaming path: {text:?}"
        );
    }

    #[tokio::test]
    async fn render_side_chat_shows_readonly_chrome() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &side, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("Side chat") || text.contains("read-only") || text.contains("read"));
    }

    #[tokio::test]
    async fn side_chat_top_and_editor_borders_remain_continuous_without_titles() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &side, crate::theme::DARK))
            .expect("draw");

        let area = centered_rect(90, 24, Rect::new(0, 0, 100, 30));
        let buffer = terminal.backend().buffer();
        let top = (area.left()..area.right())
            .map(|x| buffer[(x, area.top())].symbol())
            .collect::<String>();
        assert_eq!(top, format!("╭{}╮", "─".repeat(usize::from(area.width - 2))));

        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(0),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let divider = (chunks[4].left()..chunks[4].right())
            .map(|x| buffer[(x, chunks[4].top())].symbol())
            .collect::<String>();
        assert_eq!(divider, "─".repeat(usize::from(chunks[4].width)));
    }

    #[tokio::test]
    async fn render_side_chat_strips_csi_and_osc_from_buffer() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");

        // Transcript (user entry) carries CSI color + cursor controls.
        side.submit_prompt("hello \u{1b}[31mworld\u{1b}[0m\u{1b}[2;5H!");
        // Editor carries OSC 8 hyperlinks (BEL and ST forms) plus a bare control.
        side.handle_paste(
            "edit \u{1b}]8;;https://example.com\u{7}label\u{1b}]8;;\u{7} \u{1b}]8;;https://example.com\u{1b}\\st\u{1b}]8;;\u{1b}\\\u{7}",
        );

        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &side, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);

        assert!(text.contains("hello"), "visible user payload missing: {text:?}");
        assert!(text.contains("world"), "CSI-wrapped label missing: {text:?}");
        assert!(text.contains("label"), "OSC8 BEL label missing: {text:?}");
        assert!(text.contains("st"), "OSC8 ST label missing: {text:?}");
        assert!(text.contains("edit"), "editor payload missing: {text:?}");
        assert_no_unsafe_controls(&text);
        assert!(
            !text.contains("https://example.com"),
            "hyperlink target must not render: {text:?}"
        );
        assert!(
            !text.contains("[31m") && !text.contains("[0m") && !text.contains("[2;5H"),
            "raw CSI payload leaked: {text:?}"
        );
    }
}
