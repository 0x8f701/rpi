use pi_agent::{AgentEvent, AgentToolResult};
use pi_ai::BashExecutionMessage;
use pi_cli::job_card_adapter::JobCardPresentationAdapter;
use pi_cli::process_commands::format_process_list;
use pi_cli::theme;
use pi_cli::tool_card_adapter::{
    ToolCardPresentationAdapter, ToolCardRowRole, ToolCardRows, ToolCardSummary,
    format_tool_card_summary,
};
use pi_cli::tui_test_support::{
    composer_warning_chrome, footer_context_status_text, footer_git_status_text,
    live_outer_content_width, omp_default_theme, render_job_card_lines, render_tool_card_lines,
    render_tool_card_screen,
};
use pi_coding::orchestration::{
    AgentSnapshot, JobSnapshot, JobStatus, MailboxMessage, OrchestrationEvent,
};
use pi_coding::process::{ProcessInfo, ProcessState};
use pi_coding::{AgentStatus, SessionContextUsage, TaskResult, ToolCallViewStatus};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

const SCREEN_HEIGHT: u16 = 96;
const COMPACT_BASH_CONTENT_LIMIT: usize = 10;
const EXPANDED_BASH_CONTENT_LIMIT: usize = 30;

fn start(id: &str, name: &str, arguments: Value) -> AgentEvent {
    AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        arguments,
    }
}

fn update(id: &str, name: &str, text: &str) -> AgentEvent {
    AgentEvent::ToolExecutionUpdate {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        arguments: Value::Null,
        partial_result: AgentToolResult::text(text),
    }
}

fn end(id: &str, name: &str, text: &str, details: Value, is_error: bool) -> AgentEvent {
    let mut tool_result = AgentToolResult::text(text);
    tool_result.details = details;
    AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        result: tool_result,
        is_error,
    }
}

fn summary_text(summary: &ToolCardSummary) -> String {
    format_tool_card_summary(summary)
}

fn tool_card_lines(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    width: u16,
) -> Vec<Line<'static>> {
    render_tool_card_lines(compact, expanded, omp_default_theme(), width, false)
}

fn expanded_tool_card_lines(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    width: u16,
) -> Vec<Line<'static>> {
    render_tool_card_lines(compact, expanded, omp_default_theme(), width, true)
}

fn first_border_color(lines: &[Line<'static>]) -> Option<Color> {
    lines.iter().find_map(|line| {
        line.spans.iter().find_map(|span| {
            let text = span.content.as_ref();
            if text.contains('╭') || text.contains('│') {
                span.style.fg
            } else {
                None
            }
        })
    })
}

fn render_lines(terminal: &mut Terminal<TestBackend>, lines: Vec<Line<'static>>) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        })
        .expect("draw test card");
}

fn screen_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let width = usize::from(terminal.backend().buffer().area.width);
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

fn trimmed_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    screen_rows(terminal)
        .into_iter()
        .map(|row| row.trim_end().to_owned())
        .collect()
}

fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    trimmed_rows(terminal).join("\n")
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
fn assert_grid_width(terminal: &Terminal<TestBackend>, width: u16) {
    let expected_cells = usize::from(width);
    let buffer_width = usize::from(terminal.backend().buffer().area.width);
    for (row_index, row) in terminal
        .backend()
        .buffer()
        .content
        .chunks(buffer_width)
        .enumerate()
    {
        assert_eq!(
            row.len(),
            expected_cells,
            "row {row_index} does not contain {width} terminal cells"
        );
        assert!(
            row.iter().all(|cell| !cell.symbol().contains('\u{FFFD}')),
            "wide character split on row {row_index}"
        );
    }
}

fn render_tool(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    width: u16,
) -> Terminal<TestBackend> {
    render_tool_card_screen(compact, expanded, width, SCREEN_HEIGHT, false)
}

fn render_expanded_tool(
    compact: &ToolCardRows,
    expanded: &ToolCardRows,
    width: u16,
) -> Terminal<TestBackend> {
    render_tool_card_screen(compact, expanded, width, SCREEN_HEIGHT, true)
}

fn job(id: &str, agent_id: &str, status: JobStatus) -> JobSnapshot {
    JobSnapshot {
        id: id.to_owned(),
        agent_id: agent_id.to_owned(),
        agent: "task".to_owned(),
        parent_id: "Main".to_owned(),
        description: Some(format!("work for {agent_id}")),
        todo_task_id: None,
        workflow_id: None,
        workflow_generation: None,
        status,
        created_at: 1_000,
        started_at: (status != JobStatus::Queued).then_some(1_100),
        finished_at: status.is_settled().then_some(2_100),
        result: None,
        soft_budget_exhausted: false,
    }
}

fn agent(id: &str, status: AgentStatus) -> AgentSnapshot {
    AgentSnapshot {
        id: id.to_owned(),
        display_name: id.to_owned(),
        agent: "task".to_owned(),
        parent_id: Some("Main".to_owned()),
        status,
        created_at: 1_000,
        last_activity: 1_000,
        unread: 0,
        artifact_ref: None,
        history_ref: None,
    }
}

fn message(index: usize, from: &str, body: String) -> OrchestrationEvent {
    OrchestrationEvent::MessageDelivered {
        group_id: "group".to_owned(),
        waiter_claimed: false,
        message: MailboxMessage {
            id: format!("message-{index}"),
            from: from.to_owned(),
            to: "Main".to_owned(),
            body,
            timestamp: 2_000 + index as u64,
            reply_to: None,
        },
    }
}

#[test]
fn bash_streaming_terminal_variants_are_bounded_and_escape_free() {
    let output = (1..=200)
        .map(|line| {
            format!(
                "\u{1b}[3{}mline-{line:03} {}\u{1b}[0m",
                line % 8,
                "界🙂".repeat(60)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let command = "printf 'first\\n'\nprintf 'second\\n'\nprintf 'third\\n'";
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("success", "bash", json!({"command": command})));
    for end_line in [40, 100, 160] {
        adapter.apply_agent_event(&update(
            "success",
            "bash",
            &output.lines().take(end_line).collect::<Vec<_>>().join("\n"),
        ));
    }
    adapter.apply_agent_event(&end("success", "bash", &output, Value::Null, false));
    let success = adapter.rows("success", false).expect("success card");
    let expanded = adapter.rows("success", true).expect("expanded success");
    assert_eq!(success.status, ToolCallViewStatus::Succeeded);
    assert!(success.truncated);
    assert_eq!(
        success
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        COMPACT_BASH_CONTENT_LIMIT
    );
    assert_eq!(
        expanded
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        EXPANDED_BASH_CONTENT_LIMIT.max(COMPACT_BASH_CONTENT_LIMIT)
    );
    let theme = omp_default_theme();
    let success_lines = tool_card_lines(&success, &expanded, 40);
    assert_eq!(first_border_color(&success_lines), Some(theme.dim));
    assert_ne!(first_border_color(&success_lines), Some(theme.bash_mode));
    let terminal = render_tool(&success, &expanded, 40);

    let text = screen_text(&terminal);
    assert!(text.contains("$ printf 'first\\n'"), "{text}");
    assert!(text.contains(" Output "), "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "ANSI escape reached grid: {text:?}"
    );
    assert_eq!(text.matches("Ctrl+O: Expand").count(), 1, "{text}");
    assert!(
        text.contains('╭') && text.contains('╰'),
        "OMP frame missing: {text}"
    );
    assert_grid_width(&terminal, 40);

    let failed = BashExecutionMessage {
        command: command.to_owned(),
        output: output.clone(),
        exit_code: Some(7),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 7,
        exclude_from_context: None,
    };
    let failed = ToolCardPresentationAdapter::bash_execution_rows(&failed, false);
    let failed_expanded = ToolCardPresentationAdapter::bash_execution_rows(
        &BashExecutionMessage {
            command: command.to_owned(),
            output: output.clone(),
            exit_code: Some(7),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 7,
            exclude_from_context: None,
        },
        true,
    );
    assert_eq!(failed.status, ToolCallViewStatus::Failed);
    let failed_lines = tool_card_lines(&failed, &failed_expanded, 40);
    assert_eq!(first_border_color(&failed_lines), Some(theme.error));
    let terminal = render_tool(&failed, &failed_expanded, 40);

    let text = screen_text(&terminal);
    assert!(text.contains("⟦Exit: 7⟧"), "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "ANSI escape reached failed grid: {text:?}"
    );
    assert_grid_width(&terminal, 40);

    let cancelled = BashExecutionMessage {
        command: command.to_owned(),
        output: output.clone(),
        exit_code: None,
        cancelled: true,
        truncated: false,
        full_output_path: None,
        timestamp: 8,
        exclude_from_context: None,
    };
    let cancelled = ToolCardPresentationAdapter::bash_execution_rows(&cancelled, false);
    let cancelled_expanded = ToolCardPresentationAdapter::bash_execution_rows(
        &BashExecutionMessage {
            command: command.to_owned(),
            output: output.clone(),
            exit_code: None,
            cancelled: true,
            truncated: false,
            full_output_path: None,
            timestamp: 8,
            exclude_from_context: None,
        },
        true,
    );
    assert_eq!(cancelled.status, ToolCallViewStatus::Cancelled);
    let cancelled_lines = tool_card_lines(&cancelled, &cancelled_expanded, 40);
    assert_eq!(first_border_color(&cancelled_lines), Some(theme.warning));
    let terminal = render_tool(&cancelled, &cancelled_expanded, 40);

    let text = screen_text(&terminal);
    assert!(text.contains("⟦Cancelled⟧"), "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "ANSI escape reached cancelled grid: {text:?}"
    );
    assert_grid_width(&terminal, 40);
}

#[test]
fn read_edit_write_and_grep_summaries_render_current_tokens() {
    let cjk_path = "src/界面/非常长的目录/🙂工具卡/连续帧/paint.rs";
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("read", "read", json!({"path": cjk_path})));
    adapter.apply_agent_event(&end(
        "read",
        "read",
        "one\ntwo\nthree\n\n[Showing lines 40-42 of 500. Use offset=43 to continue.]",
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start("edit", "edit", json!({"path": cjk_path})));
    adapter.apply_agent_event(&end(
        "edit",
        "edit",
        "ok",
        json!({"diff":"-old\n+new\n-old-two\n+new-two", "patch":"@@ -1 +1 @@\n@@ -8 +8 @@"}),
        false,
    ));
    adapter.apply_agent_event(&start(
        "write",
        "write",
        json!({"path": cjk_path, "content":"alpha\nbeta\n"}),
    ));
    adapter.apply_agent_event(&end("write", "write", "written", Value::Null, false));
    adapter.apply_agent_event(&start(
        "grep",
        "grep",
        json!({"pattern":"needle", "path":"src"}),
    ));
    adapter.apply_agent_event(&end(
        "grep",
        "grep",
        "src/a.rs:1: needle\nsrc/a.rs:8: needle\nsrc/b.rs:2: needle",
        Value::Null,
        false,
    ));

    let expectations = [
        ("read", vec!["lines 40-42", "3 returned"]),
        ("edit", vec!["2 hunks", "+2", "−2"]),
        ("write", vec!["11 bytes", "2 lines"]),
        ("grep", vec!["needle", "3 matches", "2 files"]),
    ];
    for (id, tokens) in expectations {
        let compact = adapter.rows(id, false).expect("tool card");
        let expanded = adapter.rows(id, true).expect("expanded tool card");
        let summary = adapter.summary(id).expect("tool summary");
        let terminal = render_tool(&compact, &expanded, 80);
        let text = screen_text(&terminal);
        assert!(
            summary_text(&summary).contains(cjk_path) || id == "grep",
            "path absent from adapter summary: {summary:?}"
        );
        for token in tokens {
            assert!(text.contains(token), "{id} token {token:?} absent: {text}");
        }
    }

    let compact = adapter.rows("read", false).expect("read card");
    let expanded = adapter.rows("read", true).expect("expanded read card");
    let terminal = render_tool(&compact, &expanded, 30);

    let first_summary_row = trimmed_rows(&terminal)
        .into_iter()
        .find(|row| row.contains("src/") || row.contains('界'))
        .expect("path row");
    assert!(
        !first_summary_row.contains('\u{FFFD}'),
        "wide path split: {first_summary_row:?}"
    );
}

#[test]
fn task_markdown_sections_become_labels_and_activity_is_bounded() {
    let mut adapter = JobCardPresentationAdapter::new();
    adapter.set_task_request(
        "# Goal\nPaint continuous cards\n\n# Constraints\nKeep every row bounded\n\n# Contract\nNo stale cells".to_owned(),
        [(Some("Worker".to_owned()), Some("reviewer".to_owned()), "Inspect the terminal grid".to_owned())],
    );
    let mut running = job("job", "Worker", JobStatus::Running);
    running.started_at = Some(1_100);
    adapter.set_now(12_100);
    adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: running,
    });
    adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated {
        group_id: "group".to_owned(),
        agent: agent("Worker", AgentStatus::Running),
    });
    for index in 0..80 {
        adapter.apply_orchestration_event(&message(
            index,
            "Worker",
            format!("activity-{index:02} {}", "界🙂".repeat(100)),
        ));
    }
    let card = adapter.task_card().expect("task card");
    let progress = card.children[0].progress.as_ref().expect("progress");
    assert!(
        progress.events.len() <= 64,
        "event feed exceeded bound: {}",
        progress.events.len()
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.text.chars().count() <= 140)
    );
    let lines = render_job_card_lines(&card, omp_default_theme(), 80);

    let backend = TestBackend::new(80, SCREEN_HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal");
    render_lines(&mut terminal, lines);
    let text = screen_text(&terminal);
    for label in ["Goal", "Constraints", "Contract", "Agents"] {
        assert!(text.contains(label), "label {label:?} absent: {text}");
    }
    for raw_heading in ["# Goal", "# Constraints", "# Contract"] {
        assert!(!text.contains(raw_heading), "raw heading leaked: {text}");
    }
    assert!(
        text.contains("activity-79"),
        "latest activity absent: {text}"
    );
    assert!(
        !text.contains("activity-00"),
        "oldest activity should be evicted from bounded feed: {text}"
    );
    assert_grid_width(&terminal, 80);
}

#[test]
fn process_list_formatter_paints_bounded_rows_and_exact_empty_state() {
    let empty = format_process_list(&[]);
    assert_eq!(empty, "No supervised processes");
    let mut empty_adapter = ToolCardPresentationAdapter::new();
    empty_adapter.apply_agent_event(&start("empty-ps", "bash", json!({"command": "ps"})));
    empty_adapter.apply_agent_event(&end("empty-ps", "bash", &empty, Value::Null, false));
    let empty_compact = empty_adapter
        .rows("empty-ps", false)
        .expect("empty process card");
    let empty_expanded = empty_adapter
        .rows("empty-ps", true)
        .expect("empty process expanded");
    let empty_terminal = render_tool(&empty_compact, &empty_expanded, 30);
    assert!(screen_text(&empty_terminal).contains("No supervised processes"));
    assert_grid_width(&empty_terminal, 30);

    let processes: Vec<ProcessInfo> = serde_json::from_value(json!([
        {
            "id":"process-1",
            "ownerId":"owner",
            "label":"long supervised process label 界🙂 with trailing content",
            "state":"running",
            "pid":1234,
            "tty":true,
            "startedAtMs":1,
            "outputStartCursor":0,
            "outputCursor":9000
        },
        {
            "id":"settled",
            "ownerId":"owner",
            "label":"finished",
            "state":"exited",
            "tty":false,
            "startedAtMs":1,
            "exitedAtMs":2,
            "exitCode":0,
            "outputStartCursor":10,
            "outputCursor":20
        }
    ]))
    .expect("process fixtures");
    assert_eq!(processes[0].state, ProcessState::Running);
    let formatted = format_process_list(&processes);
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("ps", "bash", json!({"command": "ps"})));
    adapter.apply_agent_event(&end("ps", "bash", &formatted, Value::Null, false));
    let compact = adapter.rows("ps", false).expect("process card");
    let expanded = adapter.rows("ps", true).expect("process expanded");
    let terminal = render_tool(&compact, &expanded, 40);
    let text = screen_text(&terminal);
    assert!(text.contains("Running"), "running process absent: {text}");
    assert!(text.contains("Exited"), "settled process absent: {text}");
    assert_grid_width(&terminal, 40);
}

#[test]
fn consecutive_activity_frames_blank_rows_after_shorter_settled_card() {
    let mut adapter = JobCardPresentationAdapter::new();
    adapter.set_task_request(
        "# Goal\nRender frame replacement".to_owned(),
        [(
            Some("Worker".to_owned()),
            Some("task".to_owned()),
            "Process activity".to_owned(),
        )],
    );
    let mut running = job("job", "Worker", JobStatus::Running);
    running.started_at = Some(1_100);
    adapter.set_now(8_100);
    adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: running,
    });
    for index in 0..12 {
        adapter.apply_orchestration_event(&message(
            index,
            "Worker",
            format!("frame-one-activity-{index}"),
        ));
    }
    let tall = adapter.task_card().expect("running task card");
    let tall_lines = render_job_card_lines(&tall, omp_default_theme(), 60);

    let tall_height = tall_lines.len();
    let backend = TestBackend::new(60, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    render_lines(&mut terminal, tall_lines);
    assert!(screen_text(&terminal).contains("frame-one-activity-11"));

    let mut settled = job("job", "Worker", JobStatus::Completed);
    settled.started_at = Some(1_100);
    settled.finished_at = Some(2_100);
    settled.description = None;
    adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: settled,
    });
    adapter.set_task_request(String::new(), std::iter::empty());
    let short = adapter.task_card().expect("settled task card");
    let short_lines = render_job_card_lines(&short, omp_default_theme(), 60);

    let short_height = short_lines.len();
    assert!(
        short_height < tall_height,
        "fixture did not shrink: {tall_height} -> {short_height}"
    );
    render_lines(&mut terminal, short_lines);
    let rows = screen_rows(&terminal);
    assert!(
        rows.iter()
            .skip(short_height)
            .all(|row| row.trim().is_empty()),
        "stale frame text below row {short_height}: {:?}",
        &rows[short_height..]
    );
    assert!(!screen_text(&terminal).contains("frame-one-activity"));
}

#[test]
fn width_matrix_handles_cjk_emoji_and_long_paths_without_overflow() {
    let fixtures = [
        "普通 ASCII output",
        "中文路径/界面/连续帧/工具卡.rs",
        "emoji/🙂/🧪/🚀/card-output.txt",
        "clusters/👩‍💻/e\u{301}/终端绘制.txt",
        "deeply/nested/path/with/a/very/long/component/that/must/not/overflow/the-grid.rs",
        "混合/mixed/🙂/long-long-long/终端绘制.txt",
    ];
    for width in [30u16, 40, 80, 120] {
        for (index, fixture) in fixtures.iter().enumerate() {
            let mut adapter = ToolCardPresentationAdapter::new();
            let id = format!("fixture-{index}");
            adapter.apply_agent_event(&start(&id, "read", json!({"path": fixture})));
            adapter.apply_agent_event(&end(
                &id,
                "read",
                &format!("{fixture}\n{}", fixture.repeat(8)),
                Value::Null,
                false,
            ));
            let compact = adapter.rows(&id, false).expect("fixture card");
            let expanded = adapter.rows(&id, true).expect("expanded fixture card");
            let terminal = render_tool(&compact, &expanded, width);
            assert_grid_width(&terminal, width);
        }
    }
}

#[test]
fn adapter_status_and_row_roles_drive_glyph_and_text_role_markers() {
    let theme = omp_default_theme();
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("running", "read", json!({"path":"running.rs"})));
    let running = adapter.rows("running", false).expect("running card");
    let running_expanded = adapter.rows("running", true).expect("running expanded");
    assert_eq!(running.status, ToolCallViewStatus::Running);
    assert!(!running.is_error);
    let running_lines = tool_card_lines(&running, &running_expanded, 40);
    assert_eq!(
        first_border_color(&running_lines),
        Some(theme.border_accent)
    );
    let terminal = render_tool(&running, &running_expanded, 40);
    let text = screen_text(&terminal);
    assert!(text.contains("• running.rs"), "{text}");
    assert_eq!(
        &terminal.backend().buffer()[(0, 0)].fg,
        &theme.border_accent
    );
    adapter.apply_agent_event(&start("failed", "read", json!({"path":"failed.rs"})));
    adapter.apply_agent_event(&end(
        "failed",
        "read",
        "permission denied",
        Value::Null,
        true,
    ));
    let failed = adapter.rows("failed", false).expect("failed card");
    let failed_expanded = adapter.rows("failed", true).expect("failed expanded");
    assert_eq!(failed.status, ToolCallViewStatus::Failed);
    assert!(failed.is_error);
    let failed_lines = tool_card_lines(&failed, &failed_expanded, 40);
    assert_eq!(first_border_color(&failed_lines), Some(theme.error));
    let terminal = render_tool(&failed, &failed_expanded, 40);
    let text = screen_text(&terminal);
    assert!(text.contains("✘ failed.rs"), "{text}");
    assert_eq!(&terminal.backend().buffer()[(0, 0)].fg, &theme.error);
}

#[test]
fn compact_and_expanded_frames_preserve_edges_and_fold_contract() {
    let output = (1..=30)
        .map(|line| format!("frame-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("toggle", "bash", json!({"command":"seq 1 30"})));
    adapter.apply_agent_event(&end("toggle", "bash", &output, Value::Null, false));
    let compact = adapter.rows("toggle", false).expect("compact card");
    let expanded = adapter.rows("toggle", true).expect("expanded card");
    let compact_content = compact
        .rows
        .iter()
        .filter(|row| row.role == ToolCardRowRole::Content)
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>();
    let expanded_content = expanded
        .rows
        .iter()
        .filter(|row| row.role == ToolCardRowRole::Content)
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>();
    assert!(
        compact_content
            .iter()
            .all(|row| expanded_content.contains(row))
    );
    assert_eq!(compact_content.first(), Some(&"frame-21"));
    assert_eq!(compact_content.last(), Some(&"frame-30"));
    assert_eq!(expanded_content.first(), Some(&"frame-01"));
    assert_eq!(expanded_content.last(), Some(&"frame-30"));

    let compact_lines = tool_card_lines(&compact, &expanded, 40);
    let expanded_lines = expanded_tool_card_lines(&compact, &expanded, 40);
    assert_eq!(
        compact_lines.first().map(Line::width),
        expanded_lines.first().map(Line::width)
    );
    assert_eq!(
        compact_lines
            .first()
            .map(|line| line.spans[0].content.as_ref()),
        expanded_lines
            .first()
            .map(|line| line.spans[0].content.as_ref())
    );
    assert!(
        compact_lines
            .iter()
            .any(|line| line_text(line).contains("Ctrl+O: Expand"))
    );
    assert!(
        expanded_lines
            .iter()
            .all(|line| !line_text(line).contains("Ctrl+O: Expand"))
    );
    assert_eq!(compact_content.last(), expanded_content.last());

    let compact_terminal = render_tool(&compact, &expanded, 40);
    let expanded_terminal = render_expanded_tool(&compact, &expanded, 40);
    assert_grid_width(&compact_terminal, 40);
    assert_grid_width(&expanded_terminal, 40);
}

#[test]
fn bash_frames_shrink_without_ghost_rows_on_same_backend() {
    let width = 50u16;
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start(
        "bash-shrink",
        "bash",
        json!({"command": "printf activity"}),
    ));
    let tall_body = (0..12)
        .map(|index| format!("activity-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    adapter.apply_agent_event(&update("bash-shrink", "bash", &tall_body));
    let tall_compact = adapter
        .rows("bash-shrink", false)
        .expect("streaming bash compact");
    let tall = adapter.rows("bash-shrink", true).expect("streaming bash card");
    assert!(tall.rows.iter().any(|row| row.text.contains("activity-11")));
    let tall_lines = expanded_tool_card_lines(&tall_compact, &tall, width);
    let tall_height = tall_lines.len();

    let backend = TestBackend::new(width, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    render_lines(&mut terminal, tall_lines);

    adapter.apply_agent_event(&end("bash-shrink", "bash", "settled", Value::Null, false));
    let short = adapter.rows("bash-shrink", false).expect("settled bash card");
    let short_expanded = adapter
        .rows("bash-shrink", true)
        .expect("settled bash expanded");
    let short_lines = tool_card_lines(&short, &short_expanded, width);
    let short_height = short_lines.len();
    assert!(
        short_height < tall_height,
        "bash fixture did not shrink: {tall_height} -> {short_height}"
    );
    render_lines(&mut terminal, short_lines);
    let rows = screen_rows(&terminal);
    assert!(
        rows.iter()
            .skip(short_height)
            .all(|row| row.trim().is_empty()),
        "ghost rows: {:?}",
        &rows[short_height..]
    );
    assert!(!screen_text(&terminal).contains("activity-11"));
}

#[test]
fn paragraph_clipping_uses_the_declared_render_area() {
    let backend = TestBackend::new(30, 6);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new("界🙂".repeat(100)).wrap(Wrap { trim: false }),
                Rect::new(0, 0, 30, 6),
            );
        })
        .expect("draw clipped paragraph");
    assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 30, 6));
    assert_eq!(terminal.backend().buffer().content.len(), 180);
}

#[test]
fn omp_theme_summary_padding_warning_and_footer_contracts() {
    let theme = omp_default_theme();
    assert_eq!(theme, theme::DARK);
    assert_eq!(live_outer_content_width(80), 78);
    assert_eq!(live_outer_content_width(2), 2);

    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("summary", "read", json!({"path":"src/界面.rs"})));
    adapter.apply_agent_event(&end("summary", "read", "line", Value::Null, false));
    let card = adapter.rows("summary", false).expect("summary card");
    let expanded = adapter.rows("summary", true).expect("summary expanded");
    let summary = adapter.summary("summary").expect("summary");
    let rendered = render_tool(&card, &expanded, 40);
    let text = screen_text(&rendered);
    assert!(
        summary_text(&summary).contains("src/界面.rs") && text.contains("界面"),
        "{text}"
    );
    assert_eq!(trimmed_rows(&rendered).last().map(String::as_str), Some(""));
    let warning = composer_warning_chrome("provider unavailable; retrying", 32, theme);
    assert!(!warning.is_empty());
    assert!(line_text(&warning[0]).contains("Warning:"));
    assert_eq!(warning[0].spans[0].style.bg, Some(theme.custom_message_bg));

    assert_eq!(
        footer_git_status_text(Some("master"), 2, 3, 4).as_deref(),
        Some("master*3+2?4")
    );
    let context = SessionContextUsage {
        tokens: Some(12_345),
        context_window: 128_000,
        percent: Some(9.64),
    };
    assert_eq!(
        footer_context_status_text(&context).as_deref(),
        Some("10% 12k/128k")
    );
}

#[test]
fn hub_send_compact_and_expanded_stay_metadata_only() {
    let message = (0..12)
        .map(|index| format!("line-{index:02} 界面"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start(
        "hub-expand",
        "hub",
        json!({"op":"send", "to":"Worker", "message": message}),
    ));
    adapter.apply_agent_event(&end(
        "hub-expand",
        "hub",
        "ok",
        json!({"op":"send", "receipts":[{"to":"Worker", "outcome":"woken"}]}),
        false,
    ));
    let compact = adapter.rows("hub-expand", false).expect("hub compact");
    let expanded = adapter.rows("hub-expand", true).expect("hub expanded");
    let compact_lines = tool_card_lines(&compact, &expanded, 60);
    let expanded_lines = expanded_tool_card_lines(&compact, &expanded, 60);
    let compact_text = compact_lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    let expanded_text = expanded_lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    for text in [&compact_text, &expanded_text] {
        assert!(
            text.contains("✉ IRC ➤ Worker injected"),
            "hub send title missing: {text}"
        );
        assert!(
            !text.contains("line-00") && !text.contains("line-11") && !text.contains('▏'),
            "hub send body stays off the card: {text}"
        );
    }
}

#[test]
fn production_tool_card_keeps_cjk_zwj_and_combining_clusters() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let path = format!("src/界面{family}{combining}.rs");
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("zwj", "read", json!({"path": path})));
    adapter.apply_agent_event(&end(
        "zwj",
        "read",
        "file body must not paint",
        Value::Null,
        false,
    ));
    let compact = adapter.rows("zwj", false).expect("compact card");
    let expanded = adapter.rows("zwj", true).expect("expanded card");
    for width in [20u16, 40, 80] {
        let compact_terminal = render_tool(&compact, &expanded, width);
        let expanded_terminal = render_expanded_tool(&compact, &expanded, width);
        assert_grid_width(&compact_terminal, width);
        assert_grid_width(&expanded_terminal, width);
        let compact_text = screen_text(&compact_terminal);
        let expanded_text = screen_text(&expanded_terminal);
        let compact_clusters: Vec<&str> = compact_text.graphemes(true).collect();
        let expanded_clusters: Vec<&str> = expanded_text.graphemes(true).collect();
        assert!(
            compact_clusters.contains(&family),
            "ZWJ family missing from compact production paint: {compact_text}"
        );
        assert!(
            compact_clusters.contains(&combining),
            "combining cluster missing from compact production paint: {compact_text}"
        );
        assert!(
            expanded_clusters.contains(&family),
            "ZWJ family missing from expanded production paint: {expanded_text}"
        );
        assert!(
            expanded_clusters.contains(&combining),
            "combining cluster missing from expanded production paint: {expanded_text}"
        );
        assert!(
            !compact_text.contains('\u{FFFD}'),
            "compact split a wide cluster: {compact_text}"
        );
        assert!(
            !expanded_text.contains('\u{FFFD}'),
            "expanded split a wide cluster: {expanded_text}"
        );
        for row in trimmed_rows(&compact_terminal)
            .into_iter()
            .chain(trimmed_rows(&expanded_terminal))
        {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(
                    row_clusters.contains(&family),
                    "ZWJ family split across a production row: {row:?}"
                );
            }
            if row.contains('\u{301}') {
                assert!(
                    row_clusters.contains(&combining),
                    "combining cluster split across a production row: {row:?}"
                );
            }
        }
    }
}

fn todo_phase_tasks(tasks: &[(&str, &str)]) -> Value {
    json!(
        tasks
            .iter()
            .map(|(content, status)| json!({"content": content, "status": status, "waitingOn": []}))
            .collect::<Vec<_>>()
    )
}

#[test]
fn production_todo_card_matches_omp_glyphs_and_compact_cap() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let mixed = json!({
        "phases": [{
            "name": "Steps",
            "tasks": todo_phase_tasks(&[
                ("Create todo-test.txt with step1", "completed"),
                ("Append step2 to file", "pending"),
                ("Read back and confirm both lines", "in_progress"),
            ]),
        }],
    });
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start("todo-mixed", "todo", json!({"op":"set_status"})));
    adapter.apply_agent_event(&end(
        "todo-mixed",
        "todo",
        "Remaining items\n- id=t2 ready",
        mixed,
        false,
    ));
    let compact = adapter.rows("todo-mixed", false).expect("todo compact");
    let expanded = adapter.rows("todo-mixed", true).expect("todo expanded");
    assert_eq!(compact.summary, "Todo 3 tasks");
    assert_eq!(compact.status_glyph(), "☑");
    assert!(
        compact
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Details),
        "compact Todo must not append details JSON"
    );
    assert!(
        expanded
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Details),
        "expanded structured Todo must not append details JSON"
    );

    let overflow_tasks = {
        let mut tasks = vec![
            json!({"content": format!("Create 界面 {family}"), "status": "completed", "waitingOn": []}),
            json!({"content": format!("Confirm {combining} 终端"), "status": "completed", "waitingOn": []}),
            json!({"content": "Archive older completion", "status": "completed", "waitingOn": []}),
            json!({"content": "Record recent completion", "status": "completed", "waitingOn": []}),
            json!({"content": "Write current file", "status": "in_progress", "waitingOn": []}),
        ];
        for index in 1..=5 {
            tasks.push(
                json!({"content": format!("open-{index}"), "status": "pending", "waitingOn": []}),
            );
        }
        tasks.push(json!({"content": "tail-open 终端绘制", "status": "pending", "waitingOn": []}));
        tasks
    };
    adapter.apply_agent_event(&start("todo-overflow", "todo", json!({"op":"set_status"})));
    adapter.apply_agent_event(&end(
        "todo-overflow",
        "todo",
        "Remaining items id=hidden",
        json!({"phases": [{"name": "Steps", "tasks": overflow_tasks}]}),
        false,
    ));
    let overflow_compact = adapter
        .rows("todo-overflow", false)
        .expect("overflow compact");
    let overflow_expanded = adapter
        .rows("todo-overflow", true)
        .expect("overflow expanded");
    let compact_tasks = overflow_compact
        .rows
        .iter()
        .filter(|row| {
            row.role == ToolCardRowRole::Content
                && (row.text.contains("☑") || row.text.contains("☐"))
        })
        .count();
    let expanded_tasks = overflow_expanded
        .rows
        .iter()
        .filter(|row| {
            row.role == ToolCardRowRole::Content
                && (row.text.contains("☑") || row.text.contains("☐"))
        })
        .count();
    assert_eq!(overflow_compact.summary, "Todo 11 tasks");
    assert_eq!(compact_tasks, 8, "compact Todo cap is 8 task rows");
    assert_eq!(
        expanded_tasks, 11,
        "expanded Todo shows every structured task"
    );
    assert!(overflow_compact.truncated);
    assert_eq!(overflow_compact.omitted_content_lines, 3);
    assert!(
        overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("tail-open 终端绘制"))
    );
    assert!(
        overflow_expanded
            .rows
            .iter()
            .any(|row| row.text.contains("Create 界面"))
    );
    assert!(
        overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("Write current file") && row.text.contains("☐"))
    );
    assert_eq!(
        overflow_compact
            .rows
            .iter()
            .filter(|row| row.text.contains("☑") && row.role == ToolCardRowRole::Content)
            .count(),
        1,
        "compact retains exactly the most recent completed task when seven active/open rows remain"
    );
    assert!(
        overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("Record recent completion"))
    );
    assert!(
        !overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("Archive older completion"))
    );

    adapter.apply_agent_event(&start("todo-phases", "todo", json!({"op":"set_status"})));
    adapter.apply_agent_event(&end(
        "todo-phases",
        "todo",
        "phase summary",
        json!({"phases": [
            {"name": "Build", "tasks": todo_phase_tasks(&[("Prepare", "completed")])},
            {"name": "Check", "tasks": todo_phase_tasks(&[("Validate", "pending")])},
        ]}),
        false,
    ));
    let phase_compact = adapter.rows("todo-phases", false).expect("phase compact");
    let phase_expanded = adapter.rows("todo-phases", true).expect("phase expanded");

    adapter.apply_agent_event(&start("todo-failed", "todo", json!({"op":"set_status"})));
    adapter.apply_agent_event(&end("todo-failed", "todo", "todo failed", json!({}), true));
    let failed_compact = adapter.rows("todo-failed", false).expect("failed compact");
    let failed_expanded = adapter.rows("todo-failed", true).expect("failed expanded");
    assert_eq!(failed_compact.status, ToolCallViewStatus::Failed);
    assert_eq!(failed_compact.status_glyph(), "✘");

    adapter.apply_agent_event(&start("todo-cancelled", "todo", json!({"op":"set_status"})));
    adapter.apply_agent_event(&end(
        "todo-cancelled",
        "todo",
        "Operation aborted",
        json!({
            "cancelled": true,
            "phases": [{"name": "Cancel", "tasks": todo_phase_tasks(&[("Keep queued work", "pending")])}],
        }),
        true,
    ));
    let cancelled_compact = adapter
        .rows("todo-cancelled", false)
        .expect("cancelled compact");
    let cancelled_expanded = adapter
        .rows("todo-cancelled", true)
        .expect("cancelled expanded");
    assert_eq!(cancelled_compact.status, ToolCallViewStatus::Cancelled);
    assert_eq!(cancelled_compact.status_glyph(), "■");

    for width in [20u16, 40, 80] {
        let mixed_compact = render_tool(&compact, &expanded, width);
        let mixed_expanded = render_expanded_tool(&compact, &expanded, width);
        let overflow_compact_term = render_tool(&overflow_compact, &overflow_expanded, width);
        let overflow_expanded_term =
            render_expanded_tool(&overflow_compact, &overflow_expanded, width);
        let phase_terminal = render_tool(&phase_compact, &phase_expanded, width);
        let failed_terminal = render_tool(&failed_compact, &failed_expanded, width);
        let cancelled_terminal = render_tool(&cancelled_compact, &cancelled_expanded, width);
        assert_grid_width(&mixed_compact, width);
        assert_grid_width(&mixed_expanded, width);
        assert_grid_width(&overflow_compact_term, width);
        assert_grid_width(&overflow_expanded_term, width);
        assert_grid_width(&phase_terminal, width);
        assert_grid_width(&failed_terminal, width);
        assert_grid_width(&cancelled_terminal, width);
        let mixed_text = screen_text(&mixed_compact);
        let mixed_expanded_text = screen_text(&mixed_expanded);
        let overflow_text = screen_text(&overflow_compact_term);
        let overflow_expanded_text = screen_text(&overflow_expanded_term);
        let phase_text = screen_text(&phase_terminal);
        let failed_text = screen_text(&failed_terminal);
        let cancelled_text = screen_text(&cancelled_terminal);
        for text in [&mixed_text, &mixed_expanded_text] {
            assert!(text.contains("☑ Todo 3 tasks"), "title missing: {text}");
            assert!(
                !text.contains("Remaining items"),
                "summary prose leaked: {text}"
            );
            assert!(
                !text.contains("\"phases\""),
                "raw details JSON leaked: {text}"
            );
            assert!(!text.contains("⇶"), "Task chrome leaked: {text}");
            assert!(
                !text.contains("⟦bash⟧") && !text.contains("⟦task⟧"),
                "Job chrome leaked: {text}"
            );
            assert!(!text.contains("Task "), "Task title leaked: {text}");
        }
        if width >= 40 {
            for text in [&mixed_text, &mixed_expanded_text] {
                assert!(
                    text.contains("├─ ☑ Create todo-test.txt with step1"),
                    "{text}"
                );
                assert!(text.contains("├─ ☐ Append step2 to file"), "{text}");
                assert!(
                    text.contains("└─ ☐ Read back and confirm both lines"),
                    "{text}"
                );
            }
        }
        assert!(phase_text.contains("Build · 1/1"), "{phase_text}");
        assert!(phase_text.contains("Check · 0/1"), "{phase_text}");
        assert!(phase_text.contains("☑ Prepare"), "{phase_text}");
        assert!(phase_text.contains("☐ Validate"), "{phase_text}");
        assert!(failed_text.contains("✘ Todo 0 tasks"), "{failed_text}");
        assert!(failed_text.contains("todo failed"), "{failed_text}");
        assert!(
            cancelled_text.contains("■ Todo 1 tasks"),
            "{cancelled_text}"
        );
        assert!(
            cancelled_text.contains("☐ Keep queued work"),
            "{cancelled_text}"
        );
        assert!(overflow_text.contains("☑ Todo 11 tasks"), "{overflow_text}");
        assert!(
            overflow_text.contains("… 3 more lines"),
            "hidden count missing: {overflow_text}"
        );
        assert!(
            overflow_text.contains("tail-open"),
            "active/open tail missing: {overflow_text}"
        );
        assert!(
            overflow_expanded_text.contains("Create 界面"),
            "{overflow_expanded_text}"
        );
        let overflow_clusters: Vec<&str> = overflow_expanded_text.graphemes(true).collect();
        assert!(
            overflow_clusters.contains(&family),
            "ZWJ family missing: {overflow_expanded_text}"
        );
        assert!(
            overflow_clusters.contains(&combining),
            "combining cluster missing: {overflow_expanded_text}"
        );
        for row in trimmed_rows(&overflow_compact_term)
            .into_iter()
            .chain(trimmed_rows(&overflow_expanded_term))
        {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(row_clusters.contains(&family), "ZWJ family split: {row:?}");
            }
            if row.contains('\u{301}') {
                assert!(
                    row_clusters.contains(&combining),
                    "combining split: {row:?}"
                );
            }
        }
    }
}

#[test]
fn production_read_card_matches_omp_unboxed_line() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let long_path = format!("<workspace>/fixture-dir/界面{family}{combining}-longline.txt:raw");
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start(
        "read-ok",
        "read",
        json!({"path": "<workspace>/fixture-dir/sample.txt"}),
    ));
    adapter.apply_agent_event(&end(
        "read-ok",
        "read",
        "CJK 日本語 body that must not paint\n{\"phases\":[]}",
        json!({"bytes": 12}),
        false,
    ));
    adapter.apply_agent_event(&start("read-raw", "read", json!({"path": long_path})));
    adapter.apply_agent_event(&end(
        "read-raw",
        "read",
        "long line body must not paint",
        json!({"encoding": "raw"}),
        false,
    ));
    adapter.apply_agent_event(&start(
        "read-fail",
        "read",
        json!({"path": "<workspace>/missing.txt"}),
    ));
    adapter.apply_agent_event(&end(
        "read-fail",
        "read",
        "ENOENT: no such file",
        json!({}),
        true,
    ));
    adapter.apply_agent_event(&start(
        "read-cancel",
        "read",
        json!({"path": "<workspace>/fixture-dir/notes.md"}),
    ));
    adapter.apply_agent_event(&end(
        "read-cancel",
        "read",
        "Operation aborted",
        json!({"cancelled": true}),
        true,
    ));
    adapter.apply_agent_event(&start(
        "read-offset",
        "read",
        json!({"path": "src/lib.rs", "offset": 1, "limit": 10}),
    ));
    let offset_body = (0..10)
        .map(|n| format!("line-{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    adapter.apply_agent_event(&end(
        "read-offset",
        "read",
        &format!("{offset_body}\n\n[4320 more lines in file. Use offset=11 to continue.]"),
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start(
        "read-window",
        "read",
        json!({"path": "big.log", "offset": 40, "limit": 21}),
    ));
    adapter.apply_agent_event(&end(
        "read-window",
        "read",
        "mid\n\n[Showing lines 40-60 of 500. Use offset=61 to continue.]",
        json!({"truncation": {"truncated": true}}),
        false,
    ));
    adapter.apply_agent_event(&start(
        "read-skill-sub",
        "read",
        json!({"path": "skill://research/asset.md"}),
    ));
    adapter.apply_agent_event(&end(
        "read-skill-sub",
        "read",
        "---\nname: asset\ndescription: \"not the skill\"\n---\nasset body",
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start(
        "read-skill-missing",
        "read",
        json!({"path": "skill://missing"}),
    ));
    adapter.apply_agent_event(&end(
        "read-skill-missing",
        "read",
        "not found",
        Value::Null,
        false,
    ));

    let ok_compact = adapter.rows("read-ok", false).expect("ok compact");
    let ok_expanded = adapter.rows("read-ok", true).expect("ok expanded");
    let raw_compact = adapter.rows("read-raw", false).expect("raw compact");
    let raw_expanded = adapter.rows("read-raw", true).expect("raw expanded");
    let fail_compact = adapter.rows("read-fail", false).expect("fail compact");
    let fail_expanded = adapter.rows("read-fail", true).expect("fail expanded");
    let cancel_compact = adapter.rows("read-cancel", false).expect("cancel compact");
    let cancel_expanded = adapter.rows("read-cancel", true).expect("cancel expanded");
    let offset_compact = adapter.rows("read-offset", false).expect("offset compact");
    let offset_expanded = adapter.rows("read-offset", true).expect("offset expanded");
    let window_compact = adapter.rows("read-window", false).expect("window compact");
    let window_expanded = adapter.rows("read-window", true).expect("window expanded");
    let skill_sub_compact = adapter
        .rows("read-skill-sub", false)
        .expect("skill sub compact");
    let skill_sub_expanded = adapter
        .rows("read-skill-sub", true)
        .expect("skill sub expanded");
    let skill_missing_compact = adapter
        .rows("read-skill-missing", false)
        .expect("skill missing compact");
    let skill_missing_expanded = adapter
        .rows("read-skill-missing", true)
        .expect("skill missing expanded");

    assert_eq!(ok_compact.status_glyph(), "●");
    assert_eq!(raw_compact.status_glyph(), "●");
    assert_eq!(offset_compact.status_glyph(), "●");
    assert_eq!(fail_compact.status, ToolCallViewStatus::Failed);
    assert_eq!(fail_compact.status_glyph(), "✘");
    assert_eq!(cancel_compact.status, ToolCallViewStatus::Cancelled);
    assert_eq!(cancel_compact.status_glyph(), "■");
    assert!(ok_compact.summary.contains("sample.txt") && ok_compact.summary.contains("returned"));
    assert!(raw_compact.summary.contains(":raw") && raw_compact.summary.contains("returned"));
    assert_eq!(ok_compact.summary, ok_expanded.summary);
    assert_eq!(raw_compact.summary, raw_expanded.summary);
    match adapter.summary("read-offset").expect("offset summary") {
        ToolCardSummary::Read {
            path,
            line_range,
            lines_returned,
        } => {
            assert_eq!(path, "src/lib.rs");
            assert_eq!(line_range.as_deref(), Some("1-10"));
            assert_eq!(lines_returned, 10);
        }
        other => panic!("expected Read summary, got {other:?}"),
    }
    match adapter.summary("read-window").expect("window summary") {
        ToolCardSummary::Read {
            path,
            line_range,
            lines_returned,
        } => {
            assert_eq!(path, "big.log");
            assert_eq!(line_range.as_deref(), Some("40-60"));
            assert_eq!(lines_returned, 1);
        }
        other => panic!("expected Read summary, got {other:?}"),
    }
    assert!(
        offset_compact.summary.contains(" · lines 1-10")
            && offset_compact.summary.contains("10 returned")
    );
    assert!(
        window_compact.summary.contains(" · lines 40-60")
            && window_compact.summary.contains("1 returned")
    );
    assert!(skill_sub_compact.skill_name.is_none());
    assert!(skill_missing_compact.skill_name.is_none());
    assert_ne!(skill_sub_compact.status_glyph(), "●");
    assert_ne!(skill_missing_compact.status_glyph(), "●");
    assert!(
        skill_sub_compact
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Content && row.text.contains("asset body"))
    );
    assert!(
        skill_missing_compact
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Content && row.text.contains("not found"))
    );
    for card in [
        &ok_compact,
        &ok_expanded,
        &raw_compact,
        &raw_expanded,
        &fail_compact,
        &fail_expanded,
        &cancel_compact,
        &cancel_expanded,
        &offset_compact,
        &offset_expanded,
        &window_compact,
        &window_expanded,
    ] {
        assert!(
            card.rows
                .iter()
                .all(|row| row.role == ToolCardRowRole::Command),
            "ordinary Read must not emit result/details rows: {:?}",
            card.rows
        );
        assert!(!card.truncated);
        assert_eq!(card.omitted_content_lines, 0);
        assert!(card.skill_name.is_none());
        assert!(
            !card.summary.contains("Read"),
            "adapter summary must not duplicate the tool name: {}",
            card.summary
        );
    }

    let theme = omp_default_theme();
    let fail_lines = tool_card_lines(&fail_compact, &fail_expanded, 80);
    let cancel_lines = tool_card_lines(&cancel_compact, &cancel_expanded, 80);
    assert!(
        fail_lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| { span.content.contains('✘') && span.style.fg == Some(theme.error) }),
        "failed Read must use error color"
    );
    assert!(
        cancel_lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| { span.content.contains('■') && span.style.fg == Some(theme.warning) }),
        "cancelled Read must use warning color"
    );

    for width in [20u16, 40, 80] {
        let ok = render_tool(&ok_compact, &ok_expanded, width);
        let ok_expanded_term = render_expanded_tool(&ok_compact, &ok_expanded, width);
        let raw = render_tool(&raw_compact, &raw_expanded, width);
        let raw_expanded_term = render_expanded_tool(&raw_compact, &raw_expanded, width);
        let fail = render_tool(&fail_compact, &fail_expanded, width);
        let cancel = render_tool(&cancel_compact, &cancel_expanded, width);
        let offset = render_tool(&offset_compact, &offset_expanded, width);
        let window = render_tool(&window_compact, &window_expanded, width);
        let skill_sub = render_tool(&skill_sub_compact, &skill_sub_expanded, width);
        let skill_missing = render_tool(&skill_missing_compact, &skill_missing_expanded, width);
        assert_grid_width(&ok, width);
        assert_grid_width(&ok_expanded_term, width);
        assert_grid_width(&raw, width);
        assert_grid_width(&raw_expanded_term, width);
        assert_grid_width(&fail, width);
        assert_grid_width(&cancel, width);
        assert_grid_width(&offset, width);
        assert_grid_width(&window, width);
        assert_grid_width(&skill_sub, width);
        assert_grid_width(&skill_missing, width);
        let ok_text = screen_text(&ok);
        let ok_expanded_text = screen_text(&ok_expanded_term);
        let raw_text = screen_text(&raw);
        let raw_expanded_text = screen_text(&raw_expanded_term);
        let fail_text = screen_text(&fail);
        let cancel_text = screen_text(&cancel);
        let offset_text = screen_text(&offset);
        let window_text = screen_text(&window);
        let skill_sub_text = screen_text(&skill_sub);
        let skill_missing_text = screen_text(&skill_missing);
        for text in [
            &ok_text,
            &ok_expanded_text,
            &raw_text,
            &raw_expanded_text,
            &fail_text,
            &cancel_text,
            &offset_text,
            &window_text,
        ] {
            assert!(
                !text.contains('╭') && !text.contains('╰') && !text.contains('│'),
                "boxed chrome leaked: {text}"
            );
            assert!(
                !text.contains("CJK 日本語 body"),
                "file result leaked: {text}"
            );
            assert!(
                !text.contains("long line body"),
                "file result leaked: {text}"
            );
            assert!(!text.contains("ENOENT"), "error body leaked: {text}");
            assert!(
                !text.contains("Operation aborted"),
                "cancel body leaked: {text}"
            );
            assert!(
                !text.contains("line-0") && !text.contains("line-9"),
                "offset body leaked: {text}"
            );
            assert!(
                !text.contains("\"bytes\"") && !text.contains("\"encoding\""),
                "details JSON leaked: {text}"
            );
            assert!(
                !text.contains("☑") && !text.contains("☐"),
                "Todo chrome leaked: {text}"
            );
            assert!(
                !text.contains("⇶") && !text.contains("⟦bash⟧"),
                "Task/Job chrome leaked: {text}"
            );
            assert!(!text.contains("Read"), "duplicate Read label: {text}");
        }
        assert!(
            ok_text.contains('●') && ok_text.contains("sample.txt") && ok_text.contains("returned"),
            "{ok_text}"
        );
        assert!(
            ok_expanded_text.contains("sample.txt") && ok_expanded_text.contains("returned"),
            "{ok_expanded_text}"
        );
        assert!(
            raw_text.contains('●') && raw_text.contains(":raw") && raw_text.contains("returned"),
            "{raw_text}"
        );
        assert!(
            fail_text.contains('✘') && fail_text.contains("missing.txt"),
            "{fail_text}"
        );
        assert!(
            cancel_text.contains('■') && cancel_text.contains("notes.md"),
            "{cancel_text}"
        );
        assert!(
            offset_text.contains("lines 1-10") && offset_text.contains("10 returned"),
            "{offset_text}"
        );
        assert!(
            window_text.contains("lines 40-60") && window_text.contains("1 returned"),
            "{window_text}"
        );
        assert!(
            skill_sub_text.contains('╭') && skill_sub_text.contains("asset body"),
            "{skill_sub_text}"
        );
        assert!(
            skill_missing_text.contains('╭') && skill_missing_text.contains("not found"),
            "{skill_missing_text}"
        );
        for text in [&raw_text, &raw_expanded_text] {
            let clusters: Vec<&str> = text.graphemes(true).collect();
            assert!(clusters.contains(&family), "ZWJ family missing: {text}");
            assert!(
                clusters.contains(&combining),
                "combining cluster missing: {text}"
            );
            assert!(text.contains("界面"), "{text}");
        }
        for row in trimmed_rows(&raw)
            .into_iter()
            .chain(trimmed_rows(&raw_expanded_term))
        {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(row_clusters.contains(&family), "ZWJ family split: {row:?}");
            }
            if row.contains('\u{301}') {
                assert!(
                    row_clusters.contains(&combining),
                    "combining split: {row:?}"
                );
            }
        }
        if width == 80 {
            assert!(
                ok_text.contains(&format!("● {}", ok_compact.summary)),
                "{ok_text}"
            );
            assert!(
                raw_text.contains(&format!("● {}", raw_compact.summary)),
                "{raw_text}"
            );
            assert!(
                fail_text.contains(&format!("✘ {}", fail_compact.summary)),
                "{fail_text}"
            );
            assert!(
                cancel_text.contains(&format!("■ {}", cancel_compact.summary)),
                "{cancel_text}"
            );
            assert!(
                offset_text.contains(&format!("● {}", offset_compact.summary)),
                "{offset_text}"
            );
            assert!(
                window_text.contains(&format!("● {}", window_compact.summary)),
                "{window_text}"
            );
        }
    }
}

#[test]
fn production_bash_card_matches_omp_frame_and_status() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let cjk_command = format!("echo '日本語 한국어 中文 {family} caf{combining}'");
    let cjk_output = format!("日本語 한국어 中文 {family} café");
    let long_output = (1..=40)
        .map(|line| format!("line-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start(
        "bash-ok",
        "bash",
        json!({"command": cjk_command, "timeout": 300}),
    ));
    adapter.apply_agent_event(&end("bash-ok", "bash", &cjk_output, Value::Null, false));
    adapter.apply_agent_event(&start("bash-empty", "bash", json!({"command": "true"})));
    adapter.apply_agent_event(&end("bash-empty", "bash", "", Value::Null, false));
    adapter.apply_agent_event(&start("bash-long", "bash", json!({"command": "seq 1 40"})));
    adapter.apply_agent_event(&end("bash-long", "bash", &long_output, Value::Null, false));
    adapter.apply_agent_event(&start("bash-run", "bash", json!({"command": "sleep 30"})));
    adapter.apply_agent_event(&start(
        "bash-fail",
        "bash",
        json!({"command": "ls /nonexistent/path/that/does/not/exist/at/all", "timeout": 300}),
    ));
    adapter.apply_agent_event(&end(
        "bash-fail",
        "bash",
        "ls: cannot access '/nonexistent/path/that/does/not/exist/at/all': No such file or directory\n\nCommand exited with code 2",
        Value::Null,
        true,
    ));
    adapter.apply_agent_event(&start(
        "bash-cancel",
        "bash",
        json!({"command": "sleep 30 && echo 'should not appear'", "timeout": 35}),
    ));
    adapter.apply_agent_event(&end(
        "bash-cancel",
        "bash",
        "Command aborted",
        Value::Null,
        true,
    ));
    adapter.apply_agent_event(&start(
        "bash-timeout",
        "bash",
        json!({"command": "sleep 30", "timeout": 35}),
    ));
    adapter.apply_agent_event(&end(
        "bash-timeout",
        "bash",
        "partial output\n\nCommand timed out after 35 seconds",
        Value::Null,
        true,
    ));
    adapter.apply_agent_event(&start(
        "bash-bg",
        "bash",
        json!({"command": "sleep 5 && echo done", "background": true, "timeout": 300}),
    ));
    adapter.apply_agent_event(&end(
        "bash-bg",
        "bash",
        "Process started: 00000000-0000-7000-8000-000000000003 (visible in /ps)",
        json!({
            "id": "00000000-0000-7000-8000-000000000003",
            "ownerId": "00000000-0000-7000-8000-000000000004",
            "state": "running",
            "pid": 1234,
            "tty": false,
            "startedAtMs": 1,
            "outputStartCursor": 0,
            "outputCursor": 0
        }),
        false,
    ));
    adapter.apply_agent_event(&start(
        "bash-details-only-exit",
        "bash",
        json!({"command": "false"}),
    ));
    adapter.apply_agent_event(&end(
        "bash-details-only-exit",
        "bash",
        "failure without canonical terminal line",
        json!({"exitCode": 7}),
        true,
    ));

    let ok_compact = adapter.rows("bash-ok", false).expect("ok compact");
    let ok_expanded = adapter.rows("bash-ok", true).expect("ok expanded");
    let empty_compact = adapter.rows("bash-empty", false).expect("empty compact");
    let empty_expanded = adapter.rows("bash-empty", true).expect("empty expanded");
    let long_compact = adapter.rows("bash-long", false).expect("long compact");
    let long_expanded = adapter.rows("bash-long", true).expect("long expanded");
    let run_compact = adapter.rows("bash-run", false).expect("run compact");
    let run_expanded = adapter.rows("bash-run", true).expect("run expanded");
    let fail_compact = adapter.rows("bash-fail", false).expect("fail compact");
    let fail_expanded = adapter.rows("bash-fail", true).expect("fail expanded");
    let cancelled = adapter.rows("bash-cancel", false).expect("cancel compact");
    let cancelled_expanded = adapter.rows("bash-cancel", true).expect("cancel expanded");
    let timeout_compact = adapter
        .rows("bash-timeout", false)
        .expect("timeout compact");
    let timeout_expanded = adapter
        .rows("bash-timeout", true)
        .expect("timeout expanded");
    let bg_compact = adapter.rows("bash-bg", false).expect("bg compact");
    let bg_expanded = adapter.rows("bash-bg", true).expect("bg expanded");
    let details_only_exit = adapter
        .rows("bash-details-only-exit", false)
        .expect("details-only exit");

    assert_eq!(ok_compact.status, ToolCallViewStatus::Succeeded);
    assert_eq!(run_compact.status, ToolCallViewStatus::Running);
    assert_eq!(fail_compact.status, ToolCallViewStatus::Failed);
    assert_eq!(cancelled.status, ToolCallViewStatus::Cancelled);
    assert_eq!(timeout_compact.status, ToolCallViewStatus::Failed);
    assert_eq!(
        long_compact
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        COMPACT_BASH_CONTENT_LIMIT
    );
    assert_eq!(
        long_expanded
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        EXPANDED_BASH_CONTENT_LIMIT
    );
    assert!(long_compact.truncated);
    assert_eq!(long_compact.omitted_content_lines, 30);
    assert!(long_expanded.truncated);
    assert_eq!(long_expanded.omitted_content_lines, 10);
    assert!(
        empty_compact
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Content)
    );
    assert!(
        run_compact
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Content && row.role != ToolCardRowRole::Status)
    );
    assert!(
        fail_compact
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Status && row.text == "⟦Exit: 2⟧")
    );
    assert!(
        cancelled
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Status && row.text == "⟦Cancelled⟧")
    );
    assert!(
        timeout_compact
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Status && row.text == "⟦Timeout: 35s⟧")
    );
    assert!(
        bg_compact
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Status
                && row.text
                    == "⟦Backgrounded: 00000000-0000-7000-8000-000000000003 | Timeout: 300s⟧")
    );
    assert!(
        details_only_exit
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Status)
    );
    assert!(
        details_only_exit
            .rows
            .iter()
            .any(|row| row.role == ToolCardRowRole::Content
                && row.text == "failure without canonical terminal line")
    );
    for card in [
        &ok_compact,
        &ok_expanded,
        &long_compact,
        &long_expanded,
        &fail_compact,
        &fail_expanded,
        &cancelled,
        &cancelled_expanded,
        &timeout_compact,
        &timeout_expanded,
        &bg_compact,
        &bg_expanded,
    ] {
        assert!(
            card.rows
                .iter()
                .all(|row| row.role != ToolCardRowRole::Details),
            "bash must not dump details JSON: {:?}",
            card.rows
        );
    }

    let theme = omp_default_theme();
    assert_eq!(
        first_border_color(&tool_card_lines(&ok_compact, &ok_expanded, 80)),
        Some(theme.dim)
    );
    assert_ne!(
        first_border_color(&tool_card_lines(&ok_compact, &ok_expanded, 80)),
        Some(theme.bash_mode)
    );
    assert_eq!(
        first_border_color(&tool_card_lines(&run_compact, &run_expanded, 80)),
        Some(theme.border_accent)
    );
    assert_ne!(
        first_border_color(&tool_card_lines(&run_compact, &run_expanded, 80)),
        Some(theme.bash_mode)
    );
    assert_eq!(
        first_border_color(&tool_card_lines(&fail_compact, &fail_expanded, 80)),
        Some(theme.error)
    );
    assert_eq!(
        first_border_color(&tool_card_lines(&cancelled, &cancelled_expanded, 80)),
        Some(theme.warning)
    );
    assert_eq!(
        first_border_color(&tool_card_lines(&timeout_compact, &timeout_expanded, 80)),
        Some(theme.error)
    );

    for width in [20u16, 40, 80] {
        let ok = render_tool(&ok_compact, &ok_expanded, width);
        let ok_expanded_term = render_expanded_tool(&ok_compact, &ok_expanded, width);
        let empty = render_tool(&empty_compact, &empty_expanded, width);
        let long = render_tool(&long_compact, &long_expanded, width);
        let long_expanded_term = render_expanded_tool(&long_compact, &long_expanded, width);
        let run = render_tool(&run_compact, &run_expanded, width);
        let fail = render_tool(&fail_compact, &fail_expanded, width);
        let cancel = render_tool(&cancelled, &cancelled_expanded, width);
        let timed_out = render_tool(&timeout_compact, &timeout_expanded, width);
        let bg = render_tool(&bg_compact, &bg_expanded, width);
        for terminal in [
            &ok,
            &ok_expanded_term,
            &empty,
            &long,
            &long_expanded_term,
            &run,
            &fail,
            &cancel,
            &timed_out,
            &bg,
        ] {
            assert_grid_width(terminal, width);
        }
        let ok_text = screen_text(&ok);
        let ok_expanded_text = screen_text(&ok_expanded_term);
        let empty_text = screen_text(&empty);
        let long_text = screen_text(&long);
        let long_expanded_text = screen_text(&long_expanded_term);
        let run_text = screen_text(&run);
        let fail_text = screen_text(&fail);
        let cancel_text = screen_text(&cancel);
        let timeout_text = screen_text(&timed_out);
        let bg_text = screen_text(&bg);
        for text in [
            &ok_text,
            &ok_expanded_text,
            &empty_text,
            &long_text,
            &long_expanded_text,
            &run_text,
            &fail_text,
            &cancel_text,
            &timeout_text,
            &bg_text,
        ] {
            assert!(
                text.contains('╭') && text.contains('╰') && text.contains('│'),
                "bash frame missing: {text}"
            );
            assert!(!text.contains("Bash"), "duplicate Bash title: {text}");
            assert!(
                !text.contains("☑") && !text.contains("☐"),
                "Todo chrome leaked: {text}"
            );
            assert!(!text.contains("● Read"), "Read chrome leaked: {text}");
            assert!(
                !text.contains("⇶") && !text.contains("⟦task⟧"),
                "Task/Job chrome leaked: {text}"
            );
            assert!(
                !text.contains("\"exitCode\""),
                "raw details JSON leaked: {text}"
            );
        }
        assert!(
            ok_text.contains("$ echo") && ok_text.contains(" Output "),
            "{ok_text}"
        );
        assert!(
            ok_text.contains("日本語") && ok_text.contains("한국어") && ok_text.contains("中文"),
            "{ok_text}"
        );
        assert!(
            !empty_text.contains(" Output "),
            "empty bash must not paint Output: {empty_text}"
        );
        assert!(empty_text.contains("$ true"), "{empty_text}");
        assert!(
            long_text.contains(" Output ") && long_text.contains("… 30 more lines"),
            "{long_text}"
        );
        assert!(
            !long_text.contains("line-01"),
            "compact bash keeps the tail: {long_text}"
        );
        assert!(
            !long_expanded_text.contains("line-01")
                && long_expanded_text.contains("line-11")
                && long_expanded_text.contains("line-40"),
            "{long_expanded_text}"
        );
        assert!(
            long_expanded_text.contains("… 10 more lines")
                && !long_expanded_text.contains("Ctrl+O: Expand"),
            "{long_expanded_text}"
        );
        assert!(run_text.contains("$ sleep 30"), "{run_text}");
        assert!(
            !run_text.contains(" Output "),
            "running bash has no Output: {run_text}"
        );
        assert!(
            !ok_text.contains("Timeout:"),
            "configured timeout is not an execution result: {ok_text}"
        );
        assert!(
            fail_text.contains("$ ls")
                && fail_text.contains(" Output ")
                && fail_text.contains("⟦Exit: 2⟧"),
            "{fail_text}"
        );
        assert!(
            !fail_text.contains("Timeout:"),
            "configured timeout is not an execution result: {fail_text}"
        );
        assert!(
            !fail_text.contains("Command exited with code"),
            "control footer duplicated: {fail_text}"
        );
        assert!(
            cancel_text.contains("$ sleep 30")
                && cancel_text.contains(" Output ")
                && cancel_text.contains("⟦Cancelled⟧"),
            "{cancel_text}"
        );
        assert!(
            !cancel_text.contains("Timeout:"),
            "configured timeout is not an execution result: {cancel_text}"
        );
        assert!(
            !cancel_text.contains("Command aborted"),
            "control footer duplicated: {cancel_text}"
        );
        assert!(
            timeout_text.contains("partial output") && timeout_text.contains("⟦Timeout: 35s⟧"),
            "{timeout_text}"
        );
        assert!(
            !timeout_text.contains("Command timed out after"),
            "control footer duplicated: {timeout_text}"
        );
        assert!(
            bg_text.contains(" Output ") && bg_text.contains("Backgrounded"),
            "{bg_text}"
        );
        if width >= 80 {
            assert!(
                bg_text.contains(
                    "⟦Backgrounded: 00000000-0000-7000-8000-000000000003 | Timeout: 300s⟧"
                ),
                "{bg_text}"
            );
        } else {
            assert!(bg_text.contains("Timeout:"), "{bg_text}");
        }
        assert!(
            !bg_text.contains("Process started:"),
            "background result duplicated: {bg_text}"
        );
        for text in [&ok_text, &ok_expanded_text] {
            let clusters: Vec<&str> = text.graphemes(true).collect();
            assert!(clusters.contains(&family), "ZWJ family missing: {text}");
            assert!(
                clusters.contains(&combining) || clusters.contains(&"é"),
                "combining cluster missing: {text}"
            );
        }
        for row in trimmed_rows(&ok)
            .into_iter()
            .chain(trimmed_rows(&ok_expanded_term))
        {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(row_clusters.contains(&family), "ZWJ family split: {row:?}");
            }
        }
    }
}

#[test]
fn production_glob_card_matches_omp_unboxed_tree() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let mut adapter = ToolCardPresentationAdapter::new();
    adapter.apply_agent_event(&start(
        "glob-ok",
        "glob",
        json!({"pattern": "*", "path": "<workspace>/fixture-dir"}),
    ));
    adapter.apply_agent_event(&end(
        "glob-ok",
        "glob",
        "data.tsv\ngreeting.txt\nnotes.md\nsample.txt",
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start(
        "glob-cjk",
        "glob",
        json!({"pattern": "**/*", "path": format!("<workspace>/界面{family}{combining}")}),
    ));
    adapter.apply_agent_event(&end(
        "glob-cjk",
        "glob",
        "notes.md\nsample.txt",
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start(
        "glob-run",
        "glob",
        json!({"pattern": "*", "path": "<workspace>/fixture-dir"}),
    ));
    adapter.apply_agent_event(&start(
        "glob-overflow",
        "glob",
        json!({"pattern": "*", "path": "<workspace>/fixture-dir", "limit": 12}),
    ));
    let overflow = (1..=12)
        .map(|index| format!("file-{index:02}.txt"))
        .collect::<Vec<_>>()
        .join("\n");
    adapter.apply_agent_event(&end(
        "glob-overflow",
        "glob",
        &format!(
            "{overflow}\n\n[12 results limit reached. Use limit=24 for more, or refine pattern]"
        ),
        json!({"resultLimitReached": 12}),
        false,
    ));
    adapter.apply_agent_event(&start(
        "glob-empty",
        "glob",
        json!({"pattern": "*.none", "path": "<workspace>/empty"}),
    ));
    adapter.apply_agent_event(&end(
        "glob-empty",
        "glob",
        "No files found matching pattern",
        Value::Null,
        false,
    ));
    adapter.apply_agent_event(&start(
        "glob-fail",
        "glob",
        json!({"pattern": "*.rs", "path": "<workspace>/missing"}),
    ));
    adapter.apply_agent_event(&end(
        "glob-fail",
        "glob",
        "Path not found",
        Value::Null,
        true,
    ));
    adapter.apply_agent_event(&start(
        "glob-file",
        "glob",
        json!({"pattern": "src/lib.rs", "path": "src/lib.rs"}),
    ));
    adapter.apply_agent_event(&end("glob-file", "glob", "src/lib.rs", Value::Null, false));

    let ok_compact = adapter.rows("glob-ok", false).expect("ok compact");
    let ok_expanded = adapter.rows("glob-ok", true).expect("ok expanded");
    let cjk_compact = adapter.rows("glob-cjk", false).expect("cjk compact");
    let cjk_expanded = adapter.rows("glob-cjk", true).expect("cjk expanded");
    let run_compact = adapter.rows("glob-run", false).expect("run compact");
    let run_expanded = adapter.rows("glob-run", true).expect("run expanded");
    let overflow_compact = adapter
        .rows("glob-overflow", false)
        .expect("overflow compact");
    let overflow_expanded = adapter
        .rows("glob-overflow", true)
        .expect("overflow expanded");
    let empty_compact = adapter.rows("glob-empty", false).expect("empty compact");
    let empty_expanded = adapter.rows("glob-empty", true).expect("empty expanded");
    let fail_compact = adapter.rows("glob-fail", false).expect("failure compact");
    let fail_expanded = adapter.rows("glob-fail", true).expect("failure expanded");
    let file_compact = adapter.rows("glob-file", false).expect("file compact");
    let file_expanded = adapter.rows("glob-file", true).expect("file expanded");

    assert_eq!(ok_compact.status_glyph(), "🔍");
    assert_eq!(ok_compact.summary, "Glob <workspace>/fixture-dir 4 files");
    assert_eq!(ok_compact.summary, ok_expanded.summary);
    assert_eq!(run_compact.summary, "Glob <workspace>/fixture-dir …");
    assert_eq!(run_compact.summary, run_expanded.summary);
    assert_eq!(empty_compact.summary, "Glob <workspace>/empty 0 files");
    assert_eq!(empty_compact.summary, empty_expanded.summary);
    assert_eq!(fail_compact.summary, "Glob <workspace>/missing");
    assert_eq!(fail_compact.summary, fail_expanded.summary);
    assert!(
        ok_compact
            .rows
            .iter()
            .any(|row| row.text.contains("├─ 📑 data.tsv"))
    );
    assert!(
        ok_compact
            .rows
            .iter()
            .any(|row| row.text.contains("├─ 🗒 greeting.txt"))
    );
    assert!(
        ok_compact
            .rows
            .iter()
            .any(|row| row.text.contains("├─ 📝 notes.md"))
    );
    assert!(
        ok_compact
            .rows
            .iter()
            .any(|row| row.text.contains("└─ 🗒 sample.txt"))
    );
    assert!(
        ok_compact
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Details)
    );
    assert!(
        ok_expanded
            .rows
            .iter()
            .all(|row| row.role != ToolCardRowRole::Details)
    );
    assert_eq!(
        overflow_compact
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        6
    );
    assert_eq!(
        overflow_expanded
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        12
    );
    assert!(overflow_compact.truncated);
    assert_eq!(overflow_compact.omitted_content_lines, 6);
    assert!(
        !overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("file-12"))
    );
    assert!(
        overflow_expanded
            .rows
            .iter()
            .any(|row| row.text.contains("file-12.txt"))
    );
    assert!(
        !overflow_compact
            .rows
            .iter()
            .any(|row| row.text.contains("results limit reached"))
    );
    assert_eq!(
        empty_compact
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .count(),
        0
    );
    assert_eq!(
        empty_expanded
            .rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>(),
        vec!["No files found"]
    );
    assert_eq!(run_compact.status, ToolCallViewStatus::Running);
    assert_eq!(run_compact.status_glyph(), "🔍");
    assert_eq!(fail_compact.status, ToolCallViewStatus::Failed);
    assert_eq!(fail_compact.status_glyph(), "✘");
    assert!(
        fail_compact
            .rows
            .iter()
            .any(|row| row.text == "Path not found")
    );
    assert!(
        fail_expanded
            .rows
            .iter()
            .any(|row| row.text == "Path not found")
    );
    let file_text = file_compact
        .rows
        .iter()
        .filter(|row| row.role == ToolCardRowRole::Content)
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(file_text.matches("src/lib.rs").count(), 1);
    assert!(!file_text.contains("lib.rs/src/lib.rs"));
    assert!(
        file_compact
            .rows
            .iter()
            .any(|row| row.text.contains("└─") && row.text.contains("src/lib.rs"))
    );

    for width in [20u16, 40, 80] {
        let ok = render_tool(&ok_compact, &ok_expanded, width);
        let ok_expanded_term = render_expanded_tool(&ok_compact, &ok_expanded, width);
        let cjk = render_tool(&cjk_compact, &cjk_expanded, width);
        let overflow = render_tool(&overflow_compact, &overflow_expanded, width);
        let overflow_expanded_term =
            render_expanded_tool(&overflow_compact, &overflow_expanded, width);
        let run = render_tool(&run_compact, &run_expanded, width);
        let run_expanded_term = render_expanded_tool(&run_compact, &run_expanded, width);
        let empty = render_tool(&empty_compact, &empty_expanded, width);
        let empty_expanded_term = render_expanded_tool(&empty_compact, &empty_expanded, width);
        let fail = render_tool(&fail_compact, &fail_expanded, width);
        let fail_expanded_term = render_expanded_tool(&fail_compact, &fail_expanded, width);
        let file = render_tool(&file_compact, &file_expanded, width);
        for terminal in [
            &ok,
            &ok_expanded_term,
            &cjk,
            &overflow,
            &overflow_expanded_term,
            &run,
            &run_expanded_term,
            &empty,
            &empty_expanded_term,
            &fail,
            &fail_expanded_term,
            &file,
        ] {
            assert_grid_width(terminal, width);
        }
        let ok_text = screen_text(&ok);
        let ok_expanded_text = screen_text(&ok_expanded_term);
        let overflow_text = screen_text(&overflow);
        let overflow_expanded_text = screen_text(&overflow_expanded_term);
        let cjk_text = screen_text(&cjk);
        let run_text = screen_text(&run);
        let run_expanded_text = screen_text(&run_expanded_term);
        let empty_text = screen_text(&empty);
        let empty_expanded_text = screen_text(&empty_expanded_term);
        let fail_text = screen_text(&fail);
        let fail_expanded_text = screen_text(&fail_expanded_term);
        let file_screen = screen_text(&file);
        for text in [
            &ok_text,
            &ok_expanded_text,
            &overflow_text,
            &overflow_expanded_text,
            &cjk_text,
            &run_text,
            &run_expanded_text,
            &empty_text,
            &empty_expanded_text,
            &fail_text,
            &fail_expanded_text,
            &file_screen,
        ] {
            assert!(
                !text.contains('╭') && !text.contains('╰'),
                "boxed chrome leaked: {text}"
            );
            assert!(
                !text.contains("Glob:"),
                "obsolete Glob colon terminology leaked: {text}"
            );
            assert!(!text.contains("Find"), "Find alias leaked: {text}");
            assert!(
                !text.contains("\"resultLimitReached\""),
                "raw JSON leaked: {text}"
            );
            assert!(
                !text.contains("\"pattern\"") && !text.contains("\"path\""),
                "raw arguments leaked: {text}"
            );
            assert!(
                !text.contains("No files found matching pattern"),
                "model-facing empty sentinel leaked: {text}"
            );
            assert!(
                !text.contains("☑") && !text.contains("☐"),
                "Todo chrome leaked: {text}"
            );
            assert!(
                !text.contains("$ ") && !text.contains(" Output "),
                "Bash chrome leaked: {text}"
            );
            assert!(
                !text.contains("⇶") && !text.contains("⟦task⟧"),
                "Task/Job chrome leaked: {text}"
            );
        }
        assert!(
            ok_text.contains("🔍") && ok_text.contains("Glob "),
            "{ok_text}"
        );
        assert!(
            ok_text.contains("📑") && ok_text.contains("🗒") && ok_text.contains("📝"),
            "{ok_text}"
        );
        assert!(
            ok_text.contains("├─") && ok_text.contains("└─"),
            "{ok_text}"
        );
        assert!(overflow_text.contains("… 6 more lines"), "{overflow_text}");
        assert!(!overflow_text.contains("file-12"), "{overflow_text}");
        assert!(
            overflow_expanded_text.contains("file-12.txt"),
            "{overflow_expanded_text}"
        );
        assert!(
            run_text.contains("🔍") && run_text.contains("Glob "),
            "{run_text}"
        );
        assert!(
            run_expanded_text.contains("🔍") && run_expanded_text.contains("Glob "),
            "{run_expanded_text}"
        );
        assert!(
            !run_text.contains("files") && !run_expanded_text.contains("files"),
            "incomplete glob claimed a count"
        );
        assert!(
            empty_text.contains("🔍") && empty_text.contains("Glob "),
            "{empty_text}"
        );
        assert!(
            !empty_text.contains("No files found"),
            "compact verified-empty body must stay empty: {empty_text}"
        );
        assert!(
            trimmed_rows(&empty_expanded_term)
                .iter()
                .any(|row| row.trim() == "No files found"),
            "expanded verified-empty card must paint the exact safe empty line: {empty_expanded_text}"
        );
        assert!(
            fail_text.contains("✘") && fail_text.contains("Glob "),
            "{fail_text}"
        );
        assert!(
            fail_expanded_text.contains("✘") && fail_expanded_text.contains("Glob "),
            "{fail_expanded_text}"
        );
        assert!(
            !fail_text.contains("files") && !fail_expanded_text.contains("files"),
            "failed glob claimed a count"
        );
        assert!(
            trimmed_rows(&fail)
                .iter()
                .any(|row| row.trim() == "Path not found"),
            "{fail_text}"
        );
        assert!(
            trimmed_rows(&fail_expanded_term)
                .iter()
                .any(|row| row.trim() == "Path not found"),
            "{fail_expanded_text}"
        );
        assert_eq!(
            file_screen.matches("src/lib.rs").count(),
            2,
            "title and tree row must each paint the target: {file_screen}"
        );
        assert!(!file_screen.contains("lib.rs/src/lib.rs"), "{file_screen}");
        let clusters: Vec<&str> = cjk_text.graphemes(true).collect();
        assert!(clusters.contains(&family), "ZWJ family missing: {cjk_text}");
        assert!(
            clusters.contains(&combining),
            "combining cluster missing: {cjk_text}"
        );
        assert!(
            clusters.contains(&"界") && clusters.contains(&"面"),
            "CJK graphemes missing: {cjk_text}"
        );
        for row in trimmed_rows(&cjk) {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(row_clusters.contains(&family), "ZWJ family split: {row:?}");
            }
            if row.contains('\u{301}') {
                assert!(
                    row_clusters.contains(&combining),
                    "combining split: {row:?}"
                );
            }
        }
        if width == 80 {
            assert!(ok_text.contains(&ok_compact.summary), "{ok_text}");
            assert!(
                ok_expanded_text.contains(&ok_expanded.summary),
                "{ok_expanded_text}"
            );
            assert!(run_text.contains(&run_compact.summary), "{run_text}");
            assert!(
                run_expanded_text.contains(&run_expanded.summary),
                "{run_expanded_text}"
            );
            assert!(empty_text.contains(&empty_compact.summary), "{empty_text}");
            assert!(
                empty_expanded_text.contains(&empty_expanded.summary),
                "{empty_expanded_text}"
            );
            assert!(fail_text.contains(&fail_compact.summary), "{fail_text}");
            assert!(
                fail_expanded_text.contains(&fail_expanded.summary),
                "{fail_expanded_text}"
            );
            assert!(
                trimmed_rows(&ok)
                    .iter()
                    .any(|row| row.contains("├─") && row.contains("data.tsv")),
                "{ok_text}"
            );
            assert!(
                trimmed_rows(&ok)
                    .iter()
                    .any(|row| row.contains("└─") && row.contains("sample.txt")),
                "{ok_text}"
            );
            assert!(
                file_screen.contains("└─") && file_screen.contains("src/lib.rs"),
                "{file_screen}"
            );
        }
    }
}

fn task_result(id: &str, output: &str, error: Option<&str>) -> TaskResult {
    TaskResult {
        index: 0,
        id: id.to_owned(),
        agent: "simple_task".to_owned(),
        status: AgentStatus::Idle,
        output: output.to_owned(),
        usage: pi_ai::Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..pi_ai::Usage::default()
        },
        error: error.map(str::to_owned),
        artifact_ref: format!("agent://{id}"),
        history_ref: format!("history://{id}"),
        artifact_uri: format!("artifact://{id}"),
        soft_budget_exhausted: false,
        structured_output: None,
    }
}

#[test]
fn production_task_card_matches_omp_running_and_settled() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let combining = "e\u{301}";
    let briefing = format!(
        "# Goal\nCount lines in 界面{family}{combining}.txt\n\n# Target\n<workspace>/fixture-dir/sample.txt\n\n# Contract\nReport the exact line count."
    );
    let mut running_adapter = JobCardPresentationAdapter::new();
    running_adapter.set_task_request(
        String::new(),
        [(
            Some("LineCounter".to_owned()),
            Some("simple_task".to_owned()),
            briefing.clone(),
        )],
    );
    let mut running = job("job-run", "LineCounter", JobStatus::Running);
    running.agent = "simple_task".to_owned();
    running.started_at = Some(1_100);
    running_adapter.set_now(12_100);
    running_adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: running,
    });
    running_adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated {
        group_id: "group".to_owned(),
        agent: AgentSnapshot {
            id: "LineCounter".to_owned(),
            display_name: "LineCounter".to_owned(),
            agent: "simple_task".to_owned(),
            parent_id: Some("Main".to_owned()),
            status: AgentStatus::Running,
            created_at: 1_000,
            last_activity: 1_000,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        },
    });
    running_adapter.apply_orchestration_event(&message(
        0,
        "LineCounter",
        "bash: Check if file ends with newline".to_owned(),
    ));
    let running_card = running_adapter.task_card().expect("running task");

    let mut done_adapter = JobCardPresentationAdapter::new();
    done_adapter.set_task_request(
        String::new(),
        [(
            Some("LineCounter".to_owned()),
            Some("simple_task".to_owned()),
            briefing.clone(),
        )],
    );
    let mut done = job("job-done", "LineCounter", JobStatus::Completed);
    done.agent = "simple_task".to_owned();
    done.started_at = Some(1_000);
    done.finished_at = Some(22_200);
    done.result = Some(task_result(
        "LineCounter",
        r#"{ "line_count": 24, "method": "wc -l" }"#,
        None,
    ));
    done_adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: done,
    });
    done_adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated {
        group_id: "group".to_owned(),
        agent: AgentSnapshot {
            id: "LineCounter".to_owned(),
            display_name: "LineCounter".to_owned(),
            agent: "simple_task".to_owned(),
            parent_id: Some("Main".to_owned()),
            status: AgentStatus::Idle,
            created_at: 1_000,
            last_activity: 1_000,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        },
    });
    let done_card = done_adapter.task_card().expect("completed task");

    let mut fail_adapter = JobCardPresentationAdapter::new();
    fail_adapter.set_task_request(
        String::new(),
        [(
            Some("Broken".to_owned()),
            Some("simple_task".to_owned()),
            "# Goal\nFail closed\n\n# Contract\nSurface the error".to_owned(),
        )],
    );
    let mut failed = job("job-fail", "Broken", JobStatus::Failed);
    failed.agent = "simple_task".to_owned();
    failed.result = Some(task_result("Broken", "", Some("provider failed")));
    fail_adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: failed,
    });
    let fail_card = fail_adapter.task_card().expect("failed task");

    let mut cancel_adapter = JobCardPresentationAdapter::new();
    cancel_adapter.set_task_request(
        String::new(),
        [(
            Some("Stopped".to_owned()),
            Some("simple_task".to_owned()),
            "# Goal\nStop early\n\n# Contract\nCancelled stays cancelled".to_owned(),
        )],
    );
    let mut cancelled = job("job-cancel", "Stopped", JobStatus::Cancelled);
    cancelled.agent = "simple_task".to_owned();
    cancel_adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
        group_id: "group".to_owned(),
        job: cancelled,
    });
    let cancel_card = cancel_adapter.task_card().expect("cancelled task");

    let theme = omp_default_theme();
    assert_eq!(
        first_border_color(&render_job_card_lines(&running_card, theme, 80)),
        Some(theme.border_accent)
    );
    assert_eq!(
        first_border_color(&render_job_card_lines(&fail_card, theme, 80)),
        Some(theme.error)
    );
    assert_eq!(
        first_border_color(&render_job_card_lines(&cancel_card, theme, 80)),
        Some(theme.warning)
    );

    for width in [20u16, 40, 80] {
        let running_lines = render_job_card_lines(&running_card, theme, width);
        let done_lines = render_job_card_lines(&done_card, theme, width);
        let fail_lines = render_job_card_lines(&fail_card, theme, width);
        let cancel_lines = render_job_card_lines(&cancel_card, theme, width);
        let mut running_term =
            Terminal::new(TestBackend::new(width, SCREEN_HEIGHT)).expect("terminal");
        let mut done_term =
            Terminal::new(TestBackend::new(width, SCREEN_HEIGHT)).expect("terminal");
        let mut fail_term =
            Terminal::new(TestBackend::new(width, SCREEN_HEIGHT)).expect("terminal");
        let mut cancel_term =
            Terminal::new(TestBackend::new(width, SCREEN_HEIGHT)).expect("terminal");
        render_lines(&mut running_term, running_lines.clone());
        render_lines(&mut done_term, done_lines.clone());
        render_lines(&mut fail_term, fail_lines);
        render_lines(&mut cancel_term, cancel_lines);
        assert_grid_width(&running_term, width);
        assert_grid_width(&done_term, width);
        assert_grid_width(&fail_term, width);
        assert_grid_width(&cancel_term, width);
        let running_text = screen_text(&running_term);
        let done_text = screen_text(&done_term);
        let fail_text = screen_text(&fail_term);
        let cancel_text = screen_text(&cancel_term);
        for text in [&running_text, &done_text, &fail_text, &cancel_text] {
            assert!(
                text.contains('╭') && text.contains('╰'),
                "Task frame missing: {text}"
            );
            assert!(
                !text.contains("⟦bash⟧") && !text.contains("⟦task⟧"),
                "Job chrome leaked: {text}"
            );
            assert!(
                !text.contains("☑ Todo") && !text.contains("🔍 Glob") && !text.contains("$ "),
                "other card chrome leaked: {text}"
            );
            assert!(
                !text.contains("$0.") && !text.contains("🛠") && !text.contains(" req"),
                "invented cost/tool badges: {text}"
            );
            assert!(
                !text.contains(" Agents "),
                "Agents separator leaked: {text}"
            );
            assert!(
                !text.contains("Constraints"),
                "Constraints label leaked: {text}"
            );
        }
        if width >= 40 {
            assert!(
                running_lines
                    .iter()
                    .any(|line| line_text(line).contains("╭─── ⇶ Task 1 agent")),
                "{running_text}"
            );
            assert!(
                !running_text.contains("│ ⇶ Task") && !running_text.contains("│ • Task"),
                "header must sit in the top border: {running_text}"
            );
        } else {
            assert!(
                running_lines.iter().any(|line| {
                    let text = line_text(line);
                    text.contains('╭') && text.contains('╮') && !text.contains("Task")
                }),
                "width 20 must use bounded top: {running_text}"
            );
            assert!(
                running_text.contains("│ ⇶ Task"),
                "width 20 title must wrap into a boxed body row: {running_text}"
            );
        }
        if width >= 40 {
            assert!(running_text.contains("Goal: Count lines"), "{running_text}");
            assert!(
                running_text.contains("Report the exact line count."),
                "{running_text}"
            );
            assert!(
                running_text.contains("• LineCounter ⟦simple_task⟧"),
                "{running_text}"
            );
            if width >= 80 {
                assert!(
                    running_text.contains("Target: <workspace>/fixture-dir/sample.txt"),
                    "{running_text}"
                );
                assert!(
                    running_text.contains("bash: Check if file ends with newline"),
                    "{running_text}"
                );
            } else {
                for fragment in [
                    "Target: <workspace>/fixture-dir/samp",
                    "le.txt",
                    "bash: Check if file ends with ne",
                    "wline · 11s",
                ] {
                    assert!(
                        running_text.contains(fragment),
                        "missing {fragment:?}: {running_text}"
                    );
                }
            }
        } else {
            for fragment in [
                "Goal: Count line",
                "Target: <workspa",
                "sample.txt",
                "Report the exact",
                "line count.",
                "• LineCounter ⟦s",
                "imple_task⟧",
                "└ bash: Check",
                "if file ends wit",
                "h newline · 11s",
            ] {
                assert!(
                    running_text.contains(fragment),
                    "missing {fragment:?}: {running_text}"
                );
            }
        }
        assert!(
            !running_text.contains("Jobs ·"),
            "Jobs aggregate leaked into Task: {running_text}"
        );
        if width >= 40 {
            assert!(
                done_lines
                    .iter()
                    .any(|line| line_text(line).contains("╭─── • Task 1 agent")),
                "{done_text}"
            );
        } else {
            assert!(
                done_lines.iter().any(|line| {
                    let text = line_text(line);
                    text.contains('╭') && text.contains('╮') && !text.contains("Task")
                }),
                "width 20 must use bounded top: {done_text}"
            );
            assert!(
                done_text.contains("│ • Task"),
                "width 20 title must wrap into a boxed body row: {done_text}"
            );
        }
        assert!(
            !done_text.contains("15 tokens")
                && !done_text.contains("runtime")
                && !done_text.contains("line_count"),
            "settled Task must not include Job-area rows: {done_text}"
        );
        assert!(
            !done_text.contains("Jobs ·"),
            "Jobs aggregate leaked into Task: {done_text}"
        );
        assert!(
            !done_text.contains("bash: Check if file ends with newline"),
            "settled must drop live activity: {done_text}"
        );
        if width >= 40 {
            assert!(
                fail_text.contains("• Task 1 agent")
                    && fail_text.contains("• Broken ⟦simple_task⟧"),
                "{fail_text}"
            );
            assert!(
                cancel_text.contains("• Task 1 agent")
                    && cancel_text.contains("• Stopped ⟦simple_task⟧"),
                "{cancel_text}"
            );
        } else {
            assert!(
                fail_text.contains("│ • Task 1 agent") && fail_text.contains("• Broken ⟦simple"),
                "{fail_text}"
            );
            assert!(
                cancel_text.contains("│ • Task 1 agent")
                    && cancel_text.contains("• Stopped ⟦simpl"),
                "{cancel_text}"
            );
        }
        assert!(
            !fail_text.contains("provider failed"),
            "error belongs to Job area: {fail_text}"
        );
        let clusters: Vec<&str> = running_text.graphemes(true).collect();

        assert!(
            clusters.contains(&family),
            "ZWJ family missing: {running_text}"
        );
        assert!(
            clusters.contains(&combining),
            "combining cluster missing: {running_text}"
        );
        for row in trimmed_rows(&running_term) {
            let row_clusters: Vec<&str> = row.graphemes(true).collect();
            if row.contains('\u{200d}') {
                assert!(row_clusters.contains(&family), "ZWJ family split: {row:?}");
            }
            if row.contains('\u{301}') {
                assert!(
                    row_clusters.contains(&combining),
                    "combining split: {row:?}"
                );
            }
        }
    }
}
