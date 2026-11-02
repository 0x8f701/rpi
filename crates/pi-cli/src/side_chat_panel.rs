//! Side-chat overlay renderer for `/btw`.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use pi_coding::redact::redact_secrets;

use crate::markdown::ratatui::{render_ratatui_markdown, render_ratatui_markdown_streaming};
use crate::side_chat::{SideChatController, SideChatEntry, SideChatRole, SideChatTabs};
use crate::theme::Theme;
use crate::tui::{clean_terminal_text, markdown_ratatui_styles};

/// Render the side-chat overlay into `frame`: a tab bar row over the active
/// tab's transcript and editor.
pub fn render_side_chat_panel(frame: &mut ratatui::Frame<'_>, tabs: &SideChatTabs, theme: Theme) {
    let controller: &SideChatController = tabs;
    let area = centered_rect(
        frame.area().width.saturating_mul(9) / 10,
        frame.area().height.saturating_mul(4) / 5,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let mode = controller.tool_mode().label();
    let tab_name = tabs.active_name();
    let title = if controller.show_edit_warning() {
        format!(" Side chat · {tab_name} · EDIT MODE · {mode} · Ctrl+T toggle · Esc close ")
    } else {
        format!(" Side chat · {tab_name} · {mode} · Ctrl+T edit · Esc close ")
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

    if chunks[1].height > 0 {
        frame.render_widget(
            Paragraph::new(tab_bar_line(tabs, theme, usize::from(chunks[1].width))),
            chunks[1],
        );
    }

    let transcript_width = usize::from(chunks[2].width.saturating_sub(1)).max(1);
    let transcript_height = usize::from(chunks[2].height).max(1);
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
        chunks[2],
    );

    if controller.show_edit_warning() && chunks[3].height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ⚠ EDIT MODE: Write/Exec enabled — may overlap main agent file changes ",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[3],
        );
    }

    if chunks[4].height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", clean_terminal_text(controller.status())),
                Style::default().fg(theme.muted),
            ))),
            chunks[4],
        );
    }

    let editor_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border));
    let editor_inner = editor_block.inner(chunks[5]);
    frame.render_widget(editor_block, chunks[5]);
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

/// Build the tab-bar line: every open tab name, active one highlighted with a
/// leading arrow. Truncated to `width` display columns.
fn tab_bar_line(tabs: &SideChatTabs, theme: Theme, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let mut spans = vec![Span::styled(" tabs: ", Style::default().fg(theme.muted))];
    let active = tabs.active_name();
    for (index, name) in tabs.tab_names().into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
        }
        if name == active {
            spans.push(Span::styled(
                format!("▸{name}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                name.to_owned(),
                Style::default().fg(theme.muted),
            ));
        }
    }
    truncate_line(Line::from(spans), width)
}

fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let mut used = 0usize;
    let mut spans = Vec::new();
    for span in line.spans {
        if used >= width {
            break;
        }
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= width {
            spans.push(span);
            used += span_width;
        } else {
            let mut content = String::new();
            for ch in span.content.chars() {
                let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
                if used + ch_width > width {
                    break;
                }
                content.push(ch);
                used += ch_width;
            }
            spans.push(Span::styled(content, span.style));
        }
    }
    Line::from(spans)
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

fn entry_lines(entry: &SideChatEntry, theme: Theme, width: usize) -> Vec<Line<'static>> {
    let (label, color) = match entry.role {
        SideChatRole::User => ("you", theme.accent),
        SideChatRole::Assistant => ("side", theme.accent),
        SideChatRole::Tool => ("tool", theme.muted),
        SideChatRole::System => (
            "sys",
            if entry.is_error {
                theme.error
            } else {
                theme.muted
            },
        ),
    };
    let mut lines = vec![role_line(label, color, theme)];
    let inner_width = width.saturating_sub(2).clamp(1, usize::from(u16::MAX)) as u16;
    if entry.is_error || matches!(entry.role, SideChatRole::Tool | SideChatRole::System) {
        let style = Style::default().fg(if entry.is_error {
            theme.error
        } else {
            theme.text
        });
        for row in wrap_text(
            &redact_secrets(&clean_terminal_text(&entry.text)),
            width.saturating_sub(2),
        ) {
            lines.push(Line::from(Span::styled(format!("  {row}"), style)));
        }
        return lines;
    }
    let styles = markdown_ratatui_styles(theme, theme.text);
    let cleaned = redact_secrets(&clean_terminal_text(&entry.text));
    let rendered = if entry.is_partial {
        render_ratatui_markdown_streaming(&cleaned, inner_width, styles)
    } else {
        render_ratatui_markdown(&cleaned, inner_width, styles)
    };
    lines.extend(rendered.lines.into_iter().map(indent_line));
    lines
}

fn indent_line(line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw("  "));
    spans.extend(line.spans);
    Line::from(spans)
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
    use pi_agent::{AgentEvent, ThinkingLevel};
    use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
    use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
    use pi_coding::Application;
    use ratatui::{Terminal, backend::TestBackend};

    struct PanelTestApplication {
        application: Application,
        _agent: tempfile::TempDir,
        _session: tempfile::TempDir,
        _resources: pi_coding::ResourceManager,
        _faux: FauxProviderRegistration,
    }

    impl std::ops::Deref for PanelTestApplication {
        type Target = Application;

        fn deref(&self) -> &Self::Target {
            &self.application
        }
    }

    async fn test_application(cwd: &std::path::Path) -> PanelTestApplication {
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let model = Model {
            id: format!("side-chat-panel-{suffix}"),
            name: "Side chat panel fixture".to_owned(),
            api: format!("side-chat-panel-api-{suffix}"),
            provider: format!("side-chat-panel-provider-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let faux = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        let workspace = pi_coding::WorkspaceRoots::new(cwd, Vec::<std::path::PathBuf>::new())
            .expect("workspace");
        let session =
            pi_coding::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                pi_coding::SessionOptions {
                    model,
                    cwd: cwd.to_path_buf(),
                    system_prompt: "main system".to_owned(),
                    thinking_level: ThinkingLevel::Off,
                    api_key: "faux".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(std::sync::Arc::new(|_, _, _| {
                        Box::pin(async { pi_ai::new_assistant_message_event_stream() })
                    })),
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        let agent = tempfile::tempdir().expect("agent dir");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let session_dir = tempfile::tempdir().expect("session dir");
        session.set_session_dir(session_dir.path().to_path_buf());
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        let application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session),
        )
        .await
        .expect("publish application");
        PanelTestApplication {
            application,
            _agent: agent,
            _session: session_dir,
            _resources: resources,
            _faux: faux,
        }
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
        assert_eq!(clean_terminal_text("a\tb\n你好\u{1b}[1mc"), "a    b\n你好c");
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
            (SideChatRole::System, "err\u{1b}[2J\u{7}or", "error"),
        ];
        for (role, raw, expected) in samples {
            let text = lines_text(&entry_lines(
                &SideChatEntry {
                    role,
                    text: raw.to_owned(),
                    is_error: matches!(role, SideChatRole::System),
                    is_partial: false,
                    ..SideChatEntry::default()
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
        let text = lines_text(&entry_lines(
            &SideChatEntry {
                role: SideChatRole::Assistant,
                text: "live \u{1b}[31mstream\u{1b}[0m \u{1b}[2;5H\u{1b}]8;;https://stream.test\u{7}label\u{1b}]8;;\u{7} \u{1b}]8;;https://stream.test\u{1b}\\st\u{1b}]8;;\u{1b}\\".to_owned(),
                is_error: false,
                is_partial: true,
                ..SideChatEntry::default()
            },
            theme,
            80,
        ));
        assert!(
            text.contains("live stream label st"),
            "visible streaming payload missing: {text:?}"
        );
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

    fn side_role_count(lines: &[Line<'static>]) -> usize {
        lines
            .iter()
            .filter(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.content.trim() == "side")
            })
            .count()
    }

    #[tokio::test]
    async fn live_assistant_paints_once_while_streaming_text_is_set() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let mut side = crate::side_chat::SideChatController::fork_from(&session)
            .await
            .expect("fork");
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        let mut partial = pi_ai::AssistantMessage::pending(side.model());
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "LIVEANSWERZX".into(),
                partial: partial.clone(),
            },
        });
        assert_eq!(side.streaming_text(), "LIVEANSWERZX");
        let theme = crate::theme::DARK;
        let live = build_transcript_lines(&side, theme, 80);
        let live_text = lines_text(&live);
        assert_eq!(live_text.matches("LIVEANSWERZX").count(), 1);
        assert_eq!(side_role_count(&live), 1);

        partial.content.push(ContentBlock::text("LIVEANSWERZX"));
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(partial),
        });
        assert!(side.streaming_text().is_empty());
        let settled = build_transcript_lines(&side, theme, 80);
        let settled_text = lines_text(&settled);
        assert_eq!(settled_text.matches("LIVEANSWERZX").count(), 1);
        assert_eq!(side_role_count(&settled), 1);
    }

    #[tokio::test]
    async fn thinking_then_live_text_then_message_end_paints_one_block_each() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let mut side = crate::side_chat::SideChatController::fork_from(&session)
            .await
            .expect("fork");
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        let mut partial = pi_ai::AssistantMessage::pending(side.model());
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "REASONBLOCKQ".into(),
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "REASONBLOCKQ".into(),
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "LIVEANSWERZX".into(),
                partial: partial.clone(),
            },
        });
        assert_eq!(side.streaming_text(), "LIVEANSWERZX");
        let theme = crate::theme::DARK;
        let live = build_transcript_lines(&side, theme, 80);
        let live_text = lines_text(&live);
        assert_eq!(live_text.matches("REASONBLOCKQ").count(), 1);
        assert_eq!(live_text.matches("LIVEANSWERZX").count(), 1);
        assert_eq!(side_role_count(&live), 2);

        partial.content.push(ContentBlock::thinking("REASONBLOCKQ"));
        partial.content.push(ContentBlock::text("LIVEANSWERZX"));
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(partial),
        });
        assert!(side.streaming_text().is_empty());
        let settled = build_transcript_lines(&side, theme, 80);
        let settled_text = lines_text(&settled);
        assert_eq!(settled_text.matches("REASONBLOCKQ").count(), 1);
        assert_eq!(settled_text.matches("LIVEANSWERZX").count(), 1);
        assert_eq!(side_role_count(&settled), 2);
    }

    #[test]
    fn assistant_entry_renders_markdown_not_literal_markers() {
        let theme = crate::theme::DARK;
        let text = lines_text(&entry_lines(
            &SideChatEntry {
                role: SideChatRole::Assistant,
                text: "plan **bold** step".to_owned(),
                is_error: false,
                is_partial: false,
                ..SideChatEntry::default()
            },
            theme,
            80,
        ));
        assert!(
            !text.contains("**"),
            "assistant markdown markers must not leak as literals: {text:?}"
        );
        assert!(text.contains("plan bold step"), "rendered body: {text:?}");
        let decorated = entry_lines(
            &SideChatEntry {
                role: SideChatRole::Assistant,
                text: "**bold**".to_owned(),
                is_error: false,
                is_partial: false,
                ..SideChatEntry::default()
            },
            theme,
            80,
        );
        assert!(
            decorated[1..].iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.content.as_ref() == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))),
            "assistant bold must be a styled span"
        );
    }

    #[test]
    fn tool_entry_keeps_markdown_literals_and_redacts_secrets() {
        let theme = crate::theme::DARK;
        let secret = ["sk-", "abcdefghijklmnop1"].concat();
        let text = lines_text(&entry_lines(
            &SideChatEntry {
                role: SideChatRole::Tool,
                text: format!("**not markdown** key {secret}"),
                is_error: false,
                is_partial: false,
                ..SideChatEntry::default()
            },
            theme,
            80,
        ));
        assert!(
            text.contains("**not markdown**"),
            "tool rows must stay plain, no markdown rendering: {text:?}"
        );
        assert!(
            !text.contains(&secret),
            "tool rows must redact credentials: {text:?}"
        );
        assert!(text.contains("[REDACTED]"), "redaction marker: {text:?}");
    }

    #[test]
    fn assistant_markdown_path_redacts_credentials() {
        let theme = crate::theme::DARK;
        let secret = ["ghp_", "abcdefghijklmnopqrst"].concat();
        let text = lines_text(&entry_lines(
            &SideChatEntry {
                role: SideChatRole::Assistant,
                text: format!("token **is** {secret}"),
                is_error: false,
                is_partial: false,
                ..SideChatEntry::default()
            },
            theme,
            80,
        ));
        assert!(
            !text.contains(&secret),
            "assistant markdown path must redact: {text:?}"
        );
        assert!(text.contains("[REDACTED]"), "redaction marker: {text:?}");
    }

    #[test]
    fn markdown_lines_keep_two_space_indent_within_width() {
        let theme = crate::theme::DARK;
        let width = 30usize;
        let raw = "a **bold** b `code` c *it* done".to_owned();
        let lines = entry_lines(
            &SideChatEntry {
                role: SideChatRole::Assistant,
                text: raw,
                is_error: false,
                is_partial: false,
                ..SideChatEntry::default()
            },
            theme,
            width,
        );
        assert!(lines.len() > 1, "expected body lines beyond the role line");
        for line in &lines[1..] {
            assert_eq!(
                line.spans.first().map(|span| span.content.as_ref()),
                Some("  "),
                "body line must keep the two-column indent: {line:?}"
            );
            let visual: usize = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
            assert!(
                visual <= width,
                "indented line overflows transcript width {width}: {line:?}"
            );
        }
    }

    #[tokio::test]
    async fn render_side_chat_shows_readonly_chrome() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let tabs = crate::side_chat::SideChatTabs::from_single("default", side);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &tabs, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("Side chat") || text.contains("read-only") || text.contains("read"));
    }

    #[tokio::test]
    async fn side_chat_top_and_editor_borders_remain_continuous_without_titles() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let tabs = crate::side_chat::SideChatTabs::from_single("default", side);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &tabs, crate::theme::DARK))
            .expect("draw");

        let area = centered_rect(90, 24, Rect::new(0, 0, 100, 30));
        let buffer = terminal.backend().buffer();
        let top = (area.left()..area.right())
            .map(|x| buffer[(x, area.top())].symbol())
            .collect::<String>();
        assert_eq!(
            top,
            format!("╭{}╮", "─".repeat(usize::from(area.width - 2)))
        );

        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(0),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let divider = (chunks[5].left()..chunks[5].right())
            .map(|x| buffer[(x, chunks[5].top())].symbol())
            .collect::<String>();
        assert_eq!(divider, "─".repeat(usize::from(chunks[5].width)));
    }

    #[tokio::test]
    async fn render_side_chat_strips_csi_and_osc_from_buffer() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut side = crate::side_chat::SideChatController::fork_from(&application)
            .await
            .expect("fork");

        // Transcript (user entry) carries CSI color + cursor controls.
        side.submit_prompt("hello \u{1b}[31mworld\u{1b}[0m\u{1b}[2;5H!", Vec::new());
        // Editor carries OSC 8 hyperlinks (BEL and ST forms) plus a bare control.
        side.handle_paste(
            "edit \u{1b}]8;;https://example.com\u{7}label\u{1b}]8;;\u{7} \u{1b}]8;;https://example.com\u{1b}\\st\u{1b}]8;;\u{1b}\\\u{7}",
        );
        let tabs = crate::side_chat::SideChatTabs::from_single("default", side);

        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &tabs, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);

        assert!(
            text.contains("hello"),
            "visible user payload missing: {text:?}"
        );
        assert!(
            text.contains("world"),
            "CSI-wrapped label missing: {text:?}"
        );
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

    #[tokio::test]
    async fn render_side_chat_shows_tab_bar_with_active_marker() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = crate::side_chat::SideChatTabs::new_default(&application)
            .await
            .expect("default");
        tabs.new_tab(&application, "alpha").await.expect("alpha");
        tabs.new_tab(&application, "beta").await.expect("beta");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &tabs, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("default"),
            "tab bar must list every tab: {text:?}"
        );
        assert!(
            text.contains("alpha"),
            "tab bar must list every tab: {text:?}"
        );
        assert!(
            text.contains("beta"),
            "tab bar must list every tab: {text:?}"
        );
        assert!(
            text.contains("▸beta"),
            "active tab must carry the ▸ marker: {text:?}"
        );

        // Switch and re-render: the marker moves.
        tabs.switch_to("alpha").expect("switch");
        terminal
            .draw(|frame| render_side_chat_panel(frame, &tabs, crate::theme::DARK))
            .expect("draw");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("▸alpha"),
            "marker must follow the active tab: {text:?}"
        );
        assert!(
            !text.contains("▸beta"),
            "inactive tabs must not carry the marker: {text:?}"
        );
    }
}
