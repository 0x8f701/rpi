use std::path::Path;
use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_coding::{
    Application, ProcessEvent, ProcessId, ProcessInfo, ProcessKey, ProcessLogs, ProcessSignal,
    ProcessSpawnSpec, ProcessState, ProcessTerminalSize,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

use crate::theme::Theme;


#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveProcessCommand {
    Ps,
    Start { argv: Vec<String>, tty: bool },
    Describe { id: ProcessId },
    Logs { id: ProcessId, cursor: u64, follow: bool },
    Send { id: ProcessId, text: String },
    Resize { id: ProcessId, size: ProcessTerminalSize },
    Signal { id: ProcessId, signal: ProcessSignal },
    Stop { id: ProcessId },
    Wait { id: ProcessId },
}

pub fn parse_interactive_process_command(name: &str, argument: Option<&str>) -> Result<Option<InteractiveProcessCommand>> {
    if name == "ps" {
        return Ok(Some(InteractiveProcessCommand::Ps));
    }
    if name != "process" {
        return Ok(None);
    }
    let argument = argument.unwrap_or_default();
    let arguments = pi_coding::parse_command_args(argument);
    let mut parts = arguments.iter().map(String::as_str);
    let operation = parts.next().unwrap_or_default();
    let parse_id = |value: &str| -> Result<ProcessId> {
        Ok(serde_json::from_value(serde_json::Value::String(value.to_owned()))?)
    };
    Ok(Some(match operation {
        "start" => {
            let mut argv = parts.map(str::to_owned).collect::<Vec<_>>();
            let tty = argv.first().is_some_and(|value| value == "--tty");
            if tty { argv.remove(0); }
            if argv.is_empty() { return Err(anyhow!("process start requires [--tty] <program> [args...]")); }
            InteractiveProcessCommand::Start { argv, tty }
        }
        "describe" => InteractiveProcessCommand::Describe { id: parse_id(parts.next().ok_or_else(|| anyhow!("process describe requires an opaque process id"))?)? },
        "logs" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process logs requires an opaque process id"))?)?;
            let mut cursor = 0;
            let mut follow = false;
            while let Some(option) = parts.next() {
                match option {
                    "--follow" | "-f" => follow = true,
                    "--cursor" => cursor = parts.next().ok_or_else(|| anyhow!("process logs --cursor requires a byte offset"))?.parse()?,
                    value => return Err(anyhow!("unknown process logs option {value:?}")),
                }
            }
            InteractiveProcessCommand::Logs { id, cursor, follow }
        }
        "send" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process send requires an opaque process id"))?)?;
            let text = parts.collect::<Vec<_>>().join(" ");
            if text.is_empty() { return Err(anyhow!("process send requires text")); }
            InteractiveProcessCommand::Send { id, text }
        }
        "resize" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process resize requires an opaque process id"))?)?;
            let rows = parts.next().ok_or_else(|| anyhow!("process resize requires <rows> <cols>"))?.parse()?;
            let cols = parts.next().ok_or_else(|| anyhow!("process resize requires <rows> <cols>"))?.parse()?;
            InteractiveProcessCommand::Resize { id, size: ProcessTerminalSize { rows, cols } }
        }
        "signal" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process signal requires an opaque process id"))?)?;
            let signal = parts.next().ok_or_else(|| anyhow!("process signal requires SIGINT|SIGTERM|SIGHUP|SIGQUIT|SIGKILL"))?;
            InteractiveProcessCommand::Signal { id, signal: serde_json::from_value(serde_json::Value::String(signal.to_owned()))? }
        }
        "stop" => InteractiveProcessCommand::Stop { id: parse_id(parts.next().ok_or_else(|| anyhow!("process stop requires an opaque process id"))?)? },
        "wait" => InteractiveProcessCommand::Wait { id: parse_id(parts.next().ok_or_else(|| anyhow!("process wait requires an opaque process id"))?)? },
        _ => return Err(anyhow!("usage: /process <start|describe|logs|send|resize|signal|stop|wait> ...")),
    }))
}

pub async fn execute_interactive_process_command(
    application: &Application,
    command: InteractiveProcessCommand,
) -> Result<String> {
    match command {
        InteractiveProcessCommand::Ps => Ok(format_process_list(&application.process_list())),
        InteractiveProcessCommand::Start { argv, tty } => Ok(format_process_info(&start_process(application, argv, application.session().cwd(), tty, None).await?)),
        InteractiveProcessCommand::Describe { id } => Ok(format_process_info(&application.process_describe(&id)?)),
        InteractiveProcessCommand::Logs { id, cursor, follow } => Ok(format_process_logs(&application.process_logs(&id, cursor, None, follow, Some(Duration::from_secs(30))).await?)),
        InteractiveProcessCommand::Send { id, text } => { application.process_write(&id, text.into_bytes(), false).await?; Ok("input sent".to_owned()) }
        InteractiveProcessCommand::Resize { id, size } => { application.process_resize(&id, size)?; Ok("terminal resized".to_owned()) }
        InteractiveProcessCommand::Signal { id, signal } => { application.process_signal(&id, signal)?; Ok("signal sent".to_owned()) }
        InteractiveProcessCommand::Stop { id } => Ok(format_process_info(&application.process_stop(&id, None).await?)),
        InteractiveProcessCommand::Wait { id } => Ok(format_process_info(&application.process_wait(&id, None).await?)),
    }
}

pub async fn start_process(
    application: &Application,
    argv: Vec<String>,
    cwd: &Path,
    tty: bool,
    size: Option<ProcessTerminalSize>,
) -> Result<ProcessInfo> {
    application
        .process_spawn(ProcessSpawnSpec {
            argv,
            cwd: cwd.to_path_buf(),
            env: Default::default(),
            tty,
            terminal_size: size,
            label: None,
            timeout_ms: None,
            output_bytes: None,
        })
        .await
}

pub async fn send_process_keys(
    application: &Application,
    id: &ProcessId,
    keys: &[ProcessKey],
) -> Result<()> {
    application.process_send_keys(id, keys).await
}

#[must_use]
pub fn format_process_list(processes: &[ProcessInfo]) -> String {
    if processes.is_empty() {
        return "No supervised processes".to_owned();
    }
    processes
        .iter()
        .map(format_process_info)
        .collect::<Vec<_>>()
        .join("\n")
}

#[must_use]
pub fn format_process_info(process: &ProcessInfo) -> String {
    format!(
        "{}\t{:?}\t{}\tcursor {}..{}",
        process.id,
        process.state,
        process.label.as_deref().unwrap_or("(unlabeled)"),
        process.output_start_cursor,
        process.output_cursor
    )
}

#[must_use]
pub fn format_process_logs(logs: &ProcessLogs) -> String {
    let mut text = logs
        .chunks
        .iter()
        .filter_map(|chunk| base64::engine::general_purpose::STANDARD.decode(&chunk.data_base64).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    if logs.lost {
        text.insert_str(0, &format!("[{} output bytes lost before cursor {}]\n", logs.lost_bytes, logs.start_cursor));
    }
    text
}

const MAX_PANEL_LOG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessPanelView { List, Detail, Logs }

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessPanelInput {
    Text(String), Resize(String), Key { selected: usize }, Signal { selected: usize }, ConfirmStop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcessPanelAction {
    Close,
    Open(ProcessId),
    SendText { id: ProcessId, text: String },
    SendKeys { id: ProcessId, keys: Vec<ProcessKey> },
    Resize { id: ProcessId, size: ProcessTerminalSize },
    Signal { id: ProcessId, signal: ProcessSignal },
    Stop(ProcessId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcessKeyResult {
    Action(ProcessPanelAction),
    Handled,
    Unknown,
}

pub(crate) struct ProcessPanel {
    processes: Vec<ProcessInfo>,
    selected: usize,
    view: ProcessPanelView,
    active_id: Option<ProcessId>,
    log: VecDeque<u8>,
    log_cursor: u64,
    log_scroll: usize,
    follow: bool,
    input: Option<ProcessPanelInput>,
    notice: Option<String>,
}

impl ProcessPanel {
    pub(crate) fn new(processes: Vec<ProcessInfo>) -> Self {
        let mut panel = Self { processes, selected: 0, view: ProcessPanelView::List, active_id: None, log: VecDeque::new(), log_cursor: 0, log_scroll: 0, follow: true, input: None, notice: None };
        panel.sort_processes();
        panel
    }

    pub(crate) fn view(&self) -> ProcessPanelView { self.view }

    pub(crate) fn apply_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::ProcessStarted { process } | ProcessEvent::ProcessExited { process } => self.upsert(process),
            ProcessEvent::ProcessOutput { id, start_cursor, cursor, data_base64, .. } => {
                if let Some(process) = self.processes.iter_mut().find(|process| process.id == id) { process.output_cursor = process.output_cursor.max(cursor); }
                if self.active_id.as_ref() == Some(&id) && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_base64) { self.append_log(start_cursor, cursor, &bytes); }
            }
        }
    }

    pub(crate) fn set_logs(&mut self, id: &ProcessId, logs: &ProcessLogs) {
        if self.active_id.as_ref() != Some(id) { return; }
        self.log.clear();
        self.log_cursor = logs.start_cursor;
        if logs.lost { self.notice = Some(format!("{} earlier output bytes are no longer retained", logs.lost_bytes)); }
        for chunk in &logs.chunks { self.append_log(chunk.start_cursor, chunk.cursor, &chunk.bytes()); }
        self.log_cursor = self.log_cursor.max(logs.cursor);
        self.log_scroll = 0;
    }

    pub(crate) fn update_process(&mut self, process: ProcessInfo) { self.upsert(process); }
    pub(crate) fn fail(&mut self, message: impl Into<String>) { self.notice = Some(message.into()); }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ProcessKeyResult {
        if self.input.is_some() {
            return match self.handle_input_key(key) {
                Some(action) => ProcessKeyResult::Action(action),
                None => ProcessKeyResult::Handled,
            };
        }
        match self.view {
            ProcessPanelView::List => self.handle_list_key(key),
            ProcessPanelView::Detail | ProcessPanelView::Logs => self.handle_process_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> ProcessKeyResult {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ProcessKeyResult::Action(ProcessPanelAction::Close),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                ProcessKeyResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                ProcessKeyResult::Handled
            }
            KeyCode::Home => {
                self.selected = 0;
                ProcessKeyResult::Handled
            }
            KeyCode::End => {
                self.selected = self.processes.len().saturating_sub(1);
                ProcessKeyResult::Handled
            }
            KeyCode::Enter => {
                let Some(id) = self.processes.get(self.selected).map(|process| process.id.clone()) else {
                    return ProcessKeyResult::Handled;
                };
                self.active_id = Some(id.clone());
                self.view = ProcessPanelView::Detail;
                self.log.clear();
                self.log_scroll = 0;
                self.follow = true;
                self.notice = None;
                ProcessKeyResult::Action(ProcessPanelAction::Open(id))
            }
            _ => ProcessKeyResult::Unknown,
        }
    }

    fn handle_process_key(&mut self, key: KeyEvent) -> ProcessKeyResult {
        let Some(id) = self.active_id.clone() else {
            return ProcessKeyResult::Unknown;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') if self.active_process().is_some_and(|process| process.tty) => {
                    ProcessKeyResult::Action(ProcessPanelAction::SendKeys {
                        id,
                        keys: vec![ProcessKey::CtrlC],
                    })
                }
                KeyCode::Char('c') => ProcessKeyResult::Action(ProcessPanelAction::Signal {
                    id,
                    signal: ProcessSignal::Sigint,
                }),
                KeyCode::Char('d') => ProcessKeyResult::Action(ProcessPanelAction::SendKeys {
                    id,
                    keys: vec![ProcessKey::CtrlD],
                }),
                _ => ProcessKeyResult::Unknown,
            };
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left => {
                self.view = ProcessPanelView::List;
                self.input = None;
                ProcessKeyResult::Handled
            }
            KeyCode::Char('q') => ProcessKeyResult::Action(ProcessPanelAction::Close),
            KeyCode::Tab => {
                self.view = if self.view == ProcessPanelView::Detail {
                    ProcessPanelView::Logs
                } else {
                    ProcessPanelView::Detail
                };
                ProcessKeyResult::Handled
            }
            KeyCode::Char('d') => {
                self.view = ProcessPanelView::Detail;
                ProcessKeyResult::Handled
            }
            KeyCode::Char('l') => {
                self.view = ProcessPanelView::Logs;
                ProcessKeyResult::Handled
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.log_scroll = 0;
                }
                ProcessKeyResult::Handled
            }
            KeyCode::Char('t') | KeyCode::End => {
                self.follow = true;
                self.log_scroll = 0;
                ProcessKeyResult::Handled
            }
            KeyCode::Up if self.view == ProcessPanelView::Logs => {
                self.follow = false;
                self.log_scroll = self.log_scroll.saturating_add(1);
                ProcessKeyResult::Handled
            }
            KeyCode::Down if self.view == ProcessPanelView::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                if self.log_scroll == 0 {
                    self.follow = true;
                }
                ProcessKeyResult::Handled
            }
            KeyCode::PageUp if self.view == ProcessPanelView::Logs => {
                self.follow = false;
                self.log_scroll = self.log_scroll.saturating_add(10);
                ProcessKeyResult::Handled
            }
            KeyCode::PageDown if self.view == ProcessPanelView::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(10);
                if self.log_scroll == 0 {
                    self.follow = true;
                }
                ProcessKeyResult::Handled
            }
            KeyCode::Char('i') => {
                self.input = Some(ProcessPanelInput::Text(String::new()));
                ProcessKeyResult::Handled
            }
            KeyCode::Char('k') => {
                self.input = Some(ProcessPanelInput::Key { selected: 0 });
                ProcessKeyResult::Handled
            }
            KeyCode::Char('r') => {
                self.input = Some(ProcessPanelInput::Resize("24x80".to_owned()));
                ProcessKeyResult::Handled
            }
            KeyCode::Char('g') => {
                self.input = Some(ProcessPanelInput::Signal { selected: 0 });
                ProcessKeyResult::Handled
            }
            KeyCode::Char('x')
                if self
                    .active_process()
                    .is_some_and(|process| !process.state.is_terminal()) =>
            {
                self.input = Some(ProcessPanelInput::ConfirmStop);
                ProcessKeyResult::Handled
            }
            _ => ProcessKeyResult::Unknown,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Option<ProcessPanelAction> {
        if key.code == KeyCode::Esc { self.input = None; return None; }
        let id = self.active_id.clone()?;
        let mut action = None;
        let mut cancel = false;
        match self.input.as_mut()? {
            ProcessPanelInput::Text(text) => match key.code {
                KeyCode::Enter if !text.is_empty() => action = Some(ProcessPanelAction::SendText { id, text: std::mem::take(text) }),
                KeyCode::Backspace => { text.pop(); }
                KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => text.push(character),
                _ => {}
            },
            ProcessPanelInput::Resize(value) => match key.code {
                KeyCode::Enter => match parse_terminal_size(value) { Ok(size) => action = Some(ProcessPanelAction::Resize { id, size }), Err(error) => self.notice = Some(error.to_string()) },
                KeyCode::Backspace => { value.pop(); }
                KeyCode::Char(character) if character.is_ascii_digit() || matches!(character, 'x' | 'X' | ' ') => value.push(character),
                _ => {}
            },
            ProcessPanelInput::Key { selected } => match key.code { KeyCode::Up => *selected = selected.saturating_sub(1), KeyCode::Down => *selected = (*selected + 1).min(PROCESS_KEYS.len() - 1), KeyCode::Enter => action = Some(ProcessPanelAction::SendKeys { id, keys: vec![PROCESS_KEYS[*selected].1] }), _ => {} },
            ProcessPanelInput::Signal { selected } => match key.code { KeyCode::Up => *selected = selected.saturating_sub(1), KeyCode::Down => *selected = (*selected + 1).min(PROCESS_SIGNALS.len() - 1), KeyCode::Enter => action = Some(ProcessPanelAction::Signal { id, signal: PROCESS_SIGNALS[*selected].1 }), _ => {} },
            ProcessPanelInput::ConfirmStop => match key.code { KeyCode::Char('y' | 'Y') | KeyCode::Enter => action = Some(ProcessPanelAction::Stop(id)), KeyCode::Char('n' | 'N') => cancel = true, _ => {} },
        }
        if action.is_some() || cancel { self.input = None; }
        if action.is_some() { self.notice = None; }
        action
    }

    fn active_process(&self) -> Option<&ProcessInfo> { let id = self.active_id.as_ref()?; self.processes.iter().find(|process| &process.id == id) }
    fn move_selection(&mut self, delta: isize) { if !self.processes.is_empty() { self.selected = (self.selected as isize + delta).rem_euclid(self.processes.len() as isize) as usize; } }
    fn upsert(&mut self, process: ProcessInfo) {
        let selected_id = self.processes.get(self.selected).map(|process| process.id.clone());
        if let Some(existing) = self.processes.iter_mut().find(|existing| existing.id == process.id) { *existing = process; } else { self.processes.push(process); }
        self.sort_processes();
        self.selected = selected_id.and_then(|id| self.processes.iter().position(|process| process.id == id)).unwrap_or_else(|| self.selected.min(self.processes.len().saturating_sub(1)));
    }
    fn sort_processes(&mut self) { self.processes.sort_by(|left, right| right.started_at_ms.cmp(&left.started_at_ms).then_with(|| left.id.as_str().cmp(right.id.as_str()))); }
    fn append_log(&mut self, start_cursor: u64, cursor: u64, bytes: &[u8]) {
        if cursor <= self.log_cursor { return; }
        let offset = self.log_cursor.saturating_sub(start_cursor).min(bytes.len() as u64) as usize;
        if start_cursor > self.log_cursor { self.notice = Some(format!("{} output bytes were skipped", start_cursor - self.log_cursor)); }
        let appended_rows = if self.follow { 0 } else { display_row_breaks(&bytes[offset..]) };
        self.log.extend(&bytes[offset..]); self.log_cursor = cursor;
        while self.log.len() > MAX_PANEL_LOG_BYTES { self.log.pop_front(); }
        if self.follow { self.log_scroll = 0; } else { self.log_scroll = self.log_scroll.saturating_add(appended_rows); }
    }
}

const PROCESS_KEYS: &[(&str, ProcessKey)] = &[("Enter", ProcessKey::Enter), ("Tab", ProcessKey::Tab), ("Escape", ProcessKey::Escape), ("Ctrl-C", ProcessKey::CtrlC), ("Ctrl-D", ProcessKey::CtrlD), ("Up", ProcessKey::Up), ("Down", ProcessKey::Down), ("Left", ProcessKey::Left), ("Right", ProcessKey::Right)];
const PROCESS_SIGNALS: &[(&str, ProcessSignal)] = &[("SIGINT", ProcessSignal::Sigint), ("SIGTERM", ProcessSignal::Sigterm), ("SIGHUP", ProcessSignal::Sighup), ("SIGQUIT", ProcessSignal::Sigquit), ("SIGKILL", ProcessSignal::Sigkill)];

fn parse_terminal_size(value: &str) -> Result<ProcessTerminalSize> {
    let normalized = value.trim().replace(['x', 'X'], " ");
    let mut parts = normalized.split_whitespace();
    let rows = parts.next().ok_or_else(|| anyhow!("resize requires rows x columns"))?.parse::<u16>()?;
    let cols = parts.next().ok_or_else(|| anyhow!("resize requires rows x columns"))?.parse::<u16>()?;
    if parts.next().is_some() || rows == 0 || cols == 0 { return Err(anyhow!("terminal rows and columns must be non-zero")); }
    Ok(ProcessTerminalSize { rows, cols })
}

pub(crate) fn render_process_panel(frame: &mut ratatui::Frame<'_>, panel: &ProcessPanel, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(110).max(30);
    let height = frame.area().height.saturating_sub(4).min(34).max(10);
    let area = centered_rect(width, height, frame.area()); frame.render_widget(Clear, area);
    let title = match panel.view { ProcessPanelView::List => " Processes ", ProcessPanelView::Detail => " Process detail ", ProcessPanelView::Logs if panel.follow => " Process logs · following ", ProcessPanelView::Logs => " Process logs · paused " };
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent)).title(title);
    let inner = block.inner(area); frame.render_widget(block, area);
    match panel.view { ProcessPanelView::List => render_process_list(frame, panel, inner, theme), ProcessPanelView::Detail => render_process_detail(frame, panel, inner, theme), ProcessPanelView::Logs => render_process_logs_panel(frame, panel, inner, theme) }
    if let Some(input) = &panel.input { render_process_input(frame, input, area, theme); }
}

fn render_process_list(frame: &mut ratatui::Frame<'_>, panel: &ProcessPanel, area: Rect, theme: Theme) {
    let sections = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(1), Constraint::Length(2)]).split(area);
    let viewport = usize::from(sections[0].height).max(1);
    let start = panel.selected.saturating_add(1).saturating_sub(viewport).min(panel.processes.len().saturating_sub(viewport));
    let lines = if panel.processes.is_empty() { vec![Line::from(Span::styled(" No supervised processes. Start one with /process start --tty …", Style::default().fg(theme.muted)))] } else { panel.processes.iter().enumerate().skip(start).take(viewport).map(|(index, process)| {
        let marker = if index == panel.selected { "›" } else { " " }; let label = process.label.as_deref().unwrap_or("unlabeled"); let pid = process.pid.map_or_else(|| "—".to_owned(), |pid| pid.to_string());
        let style = if index == panel.selected { Style::default().fg(theme.text).bg(theme.selected_bg) } else { Style::default().fg(theme.text) };
        Line::from(vec![Span::styled(format!(" {marker} "), style), Span::styled(format!("{:<9}", state_label(process.state)), style.patch(process_state_style(process.state, theme))), Span::styled(format!(" pid {:<7} {:<28} {}", pid, truncate(label, 28), short_id(&process.id)), style)])
    }).collect() };
    frame.render_widget(Paragraph::new(lines), sections[0]);
    frame.render_widget(Paragraph::new(format!(" ↑/↓ select · Enter inspect · Esc close · {}/{} ", panel.selected.saturating_add(1).min(panel.processes.len()), panel.processes.len())).style(Style::default().fg(theme.dim)), sections[1]);
}

fn render_process_detail(frame: &mut ratatui::Frame<'_>, panel: &ProcessPanel, area: Rect, theme: Theme) {
    let Some(process) = panel.active_process() else { return; };
    let mut lines = vec![field_line("ID", process.id.as_str(), theme), field_line("Label", process.label.as_deref().unwrap_or("(unlabeled)"), theme), field_line("State", state_label(process.state), theme), field_line("PID", &process.pid.map_or_else(|| "—".to_owned(), |pid| pid.to_string()), theme), field_line("Terminal", if process.tty { "PTY" } else { "pipes" }, theme), field_line("Output", &format!("{}..{}", process.output_start_cursor, process.output_cursor), theme), field_line("Exit", &process.exit_code.map_or_else(|| "—".to_owned(), |code| code.to_string()), theme), Line::default(), Line::from(Span::styled(" Tab/l logs · i input · k key · r resize · g signal · Ctrl-C interrupt · x stop · Esc back ", Style::default().fg(theme.dim)))];
    if let Some(notice) = &panel.notice { lines.push(Line::from(Span::styled(sanitize(notice), Style::default().fg(theme.warning)))); }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_process_logs_panel(frame: &mut ratatui::Frame<'_>, panel: &ProcessPanel, area: Rect, theme: Theme) {
    let sections = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(1), Constraint::Length(2)]).split(area);
    let viewport = usize::from(sections[0].height).max(1);
    let width = usize::from(sections[0].width).max(1);
    let bytes = panel.log.iter().copied().collect::<Vec<_>>();
    let rows = wrap_log_rows(&sanitize(&String::from_utf8_lossy(&bytes)), width);
    let bottom = rows.len().saturating_sub(viewport);
    let start = bottom.saturating_sub(panel.log_scroll.min(bottom));
    let visible = rows.into_iter().skip(start).take(viewport).map(Line::from).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).style(Style::default().fg(theme.text)), sections[0]);
    let notice = panel.notice.as_deref().map_or(String::new(), |notice| format!(" · {}", sanitize(notice)));
    frame.render_widget(Paragraph::new(format!(" ↑/↓ scroll · f follow · t tail · Tab detail · i input · k key · r resize · g signal · x stop{notice} ")).style(Style::default().fg(theme.dim)), sections[1]);
}

fn render_process_input(frame: &mut ratatui::Frame<'_>, input: &ProcessPanelInput, parent: Rect, theme: Theme) {
    let (title, lines) = match input {
        ProcessPanelInput::Text(value) => (" Send text ", vec![Line::from(sanitize(value)), Line::from(Span::styled("Enter send · Esc cancel", Style::default().fg(theme.dim)))]),
        ProcessPanelInput::Resize(value) => (" Resize terminal ", vec![Line::from(sanitize(value)), Line::from(Span::styled("rows x columns · Enter apply · Esc cancel", Style::default().fg(theme.dim)))]),
        ProcessPanelInput::Key { selected } => (" Send special key ", picker_lines(PROCESS_KEYS.iter().map(|item| item.0), *selected, theme)),
        ProcessPanelInput::Signal { selected } => (" Send signal ", picker_lines(PROCESS_SIGNALS.iter().map(|item| item.0), *selected, theme)),
        ProcessPanelInput::ConfirmStop => (" Stop process ", vec![Line::from("Send SIGTERM, then SIGKILL if needed?"), Line::from(Span::styled("Y/Enter stop · N/Esc cancel", Style::default().fg(theme.warning)))]),
    };
    let height = u16::try_from(lines.len().saturating_add(2)).unwrap_or(u16::MAX).clamp(4, 14); let area = centered_rect(parent.width.saturating_sub(8).min(54).max(24), height, parent);
    frame.render_widget(Clear, area); frame.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.warning)).title(title)).wrap(Wrap { trim: false }), area);
}

fn picker_lines<'a>(labels: impl Iterator<Item = &'a str>, selected: usize, theme: Theme) -> Vec<Line<'static>> { labels.enumerate().map(|(index, label)| { let style = if index == selected { Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme.text) }; Line::from(Span::styled(format!(" {} {label}", if index == selected { "›" } else { " " }), style)) }).collect() }
fn process_state_style(state: ProcessState, theme: Theme) -> Style { Style::default().fg(match state { ProcessState::Running => theme.success, ProcessState::Starting | ProcessState::Stopping => theme.warning, ProcessState::Exited => theme.muted, ProcessState::TimedOut | ProcessState::Expired | ProcessState::Failed => theme.error }) }
const fn state_label(state: ProcessState) -> &'static str { match state { ProcessState::Starting => "starting", ProcessState::Running => "running", ProcessState::Stopping => "stopping", ProcessState::Exited => "exited", ProcessState::TimedOut => "timed out", ProcessState::Expired => "expired", ProcessState::Failed => "failed" } }
fn field_line(label: &str, value: &str, theme: Theme) -> Line<'static> { Line::from(vec![Span::styled(format!(" {label:<10}"), Style::default().fg(theme.dim)), Span::styled(sanitize(value), Style::default().fg(theme.text))]) }
fn short_id(id: &ProcessId) -> &str { id.as_str().get(..8).unwrap_or(id.as_str()) }
fn truncate(value: &str, width: usize) -> String { let clean = sanitize(value); if clean.chars().count() <= width { return clean; } clean.chars().take(width.saturating_sub(1)).collect::<String>() + "…" }
fn wrap_log_rows(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut row_width: usize = 0;
        for character in line.chars() {
            let character_width = character.width().unwrap_or(0);
            if row_width > 0 && row_width.saturating_add(character_width) > width {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(character);
            row_width = row_width.saturating_add(character_width);
        }
        rows.push(row);
    }
    if rows.is_empty() { rows.push(String::new()); }
    rows
}

fn display_row_breaks(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n' || **byte == b'\r').count()
}
fn sanitize(value: &str) -> String {
    let mut clean = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('@'..='~').contains(&next) { break; }
                    }
                }
                Some(']') => {
                    while let Some(next) = characters.next() {
                        if next == '\u{7}' { break; }
                        if next == '\u{1b}' && characters.peek() == Some(&'\\') { characters.next(); break; }
                    }
                }
                Some(_) | None => {}
            }
        } else if character == '\r' {
            if characters.peek() != Some(&'\n') { clean.push('\n'); }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            clean.push(character);
        }
    }
    clean
}
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect { Rect { x: area.x.saturating_add(area.width.saturating_sub(width) / 2), y: area.y.saturating_add(area.height.saturating_sub(height) / 2), width: width.min(area.width), height: height.min(area.height) } }
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_advertised_process_operation() {
        assert!(matches!(parse_interactive_process_command("ps", None).unwrap(), Some(InteractiveProcessCommand::Ps)));
        assert!(matches!(parse_interactive_process_command("process", Some("start --tty echo ok")).unwrap(), Some(InteractiveProcessCommand::Start { tty: true, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("describe id")).unwrap(), Some(InteractiveProcessCommand::Describe { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("logs id --cursor 42 --follow")).unwrap(), Some(InteractiveProcessCommand::Logs { cursor: 42, follow: true, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("send id \"hello world\"")).unwrap(), Some(InteractiveProcessCommand::Send { text, .. }) if text == "hello world"));
        assert!(matches!(parse_interactive_process_command("process", Some("resize id 40 120")).unwrap(), Some(InteractiveProcessCommand::Resize { size: ProcessTerminalSize { rows: 40, cols: 120 }, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("signal id SIGINT")).unwrap(), Some(InteractiveProcessCommand::Signal { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("stop id")).unwrap(), Some(InteractiveProcessCommand::Stop { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("wait id")).unwrap(), Some(InteractiveProcessCommand::Wait { .. })));
        assert!(parse_interactive_process_command("process", Some("send id")).is_err());
    }

    fn process(id: &str, state: ProcessState, started_at_ms: u64) -> ProcessInfo {
        ProcessInfo {
            id: serde_json::from_value(serde_json::Value::String(id.to_owned())).unwrap(),
            owner_id: pi_coding::ProcessOwnerId::new("test-owner"),
            label: Some(format!("job-{id}")),
            state,
            pid: Some(42),
            tty: true,
            started_at_ms,
            exited_at_ms: None,
            exit_code: None,
            output_start_cursor: 0,
            output_cursor: 0,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn list_selection_opens_newest_process_and_escape_returns_then_closes() {
        let mut panel = ProcessPanel::new(vec![
            process("00000000-0000-0000-0000-000000000001", ProcessState::Running, 1),
            process("00000000-0000-0000-0000-000000000002", ProcessState::Running, 2),
        ]);
        let newest = match panel.handle_key(key(KeyCode::Enter)) {
            ProcessKeyResult::Action(ProcessPanelAction::Open(id)) => id,
            action => panic!("expected open action, got {action:?}"),
        };
        assert_eq!(newest.as_str(), "00000000-0000-0000-0000-000000000002");
        assert_eq!(panel.view(), ProcessPanelView::Detail);
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), ProcessKeyResult::Handled);
        assert_eq!(panel.view(), ProcessPanelView::List);
        assert_eq!(
            panel.handle_key(key(KeyCode::Esc)),
            ProcessKeyResult::Action(ProcessPanelAction::Close)
        );
    }

    #[test]
    fn live_output_deduplicates_overlap_and_exit_updates_active_process() {
        let info = process("00000000-0000-0000-0000-000000000003", ProcessState::Running, 1);
        let id = info.id.clone();
        let mut panel = ProcessPanel::new(vec![info]);
        assert!(matches!(
            panel.handle_key(key(KeyCode::Enter)),
            ProcessKeyResult::Action(ProcessPanelAction::Open(_))
        ));
        panel.apply_event(ProcessEvent::ProcessOutput {
            id: id.clone(),
            owner_id: pi_coding::ProcessOwnerId::new("test-owner"),
            stream: pi_coding::ProcessStream::Combined,
            start_cursor: 0,
            cursor: 5,
            data_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
        });
        panel.apply_event(ProcessEvent::ProcessOutput {
            id: id.clone(),
            owner_id: pi_coding::ProcessOwnerId::new("test-owner"),
            stream: pi_coding::ProcessStream::Combined,
            start_cursor: 3,
            cursor: 7,
            data_base64: base64::engine::general_purpose::STANDARD.encode(b"lo!!"),
        });
        assert_eq!(panel.log.iter().copied().collect::<Vec<_>>(), b"hello!!");
        let mut exited = process(id.as_str(), ProcessState::Exited, 1);
        exited.exit_code = Some(0);
        panel.apply_event(ProcessEvent::ProcessExited { process: exited });
        assert_eq!(panel.active_process().unwrap().state, ProcessState::Exited);
        assert_eq!(panel.active_process().unwrap().exit_code, Some(0));
    }

    #[test]
    fn input_key_resize_signal_stop_and_follow_are_distinct_actions() {
        let info = process("00000000-0000-0000-0000-000000000004", ProcessState::Running, 1);
        let id = info.id.clone();
        let mut panel = ProcessPanel::new(vec![info]);
        panel.handle_key(key(KeyCode::Enter));

        panel.handle_key(key(KeyCode::Char('i')));
        panel.handle_key(key(KeyCode::Char('o')));
        panel.handle_key(key(KeyCode::Char('k')));
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ProcessKeyResult::Action(ProcessPanelAction::SendText {
                id: id.clone(),
                text: "ok".to_owned()
            })
        );

        panel.handle_key(key(KeyCode::Char('k')));
        panel.handle_key(key(KeyCode::Down));
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ProcessKeyResult::Action(ProcessPanelAction::SendKeys {
                id: id.clone(),
                keys: vec![ProcessKey::Tab]
            })
        );

        panel.handle_key(key(KeyCode::Char('r')));
        for _ in 0..5 { panel.handle_key(key(KeyCode::Backspace)); }
        for character in "40x120".chars() { panel.handle_key(key(KeyCode::Char(character))); }
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ProcessKeyResult::Action(ProcessPanelAction::Resize {
                id: id.clone(),
                size: ProcessTerminalSize { rows: 40, cols: 120 }
            })
        );

        panel.handle_key(key(KeyCode::Char('g')));
        panel.handle_key(key(KeyCode::Down));
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ProcessKeyResult::Action(ProcessPanelAction::Signal {
                id: id.clone(),
                signal: ProcessSignal::Sigterm
            })
        );

        panel.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            panel.handle_key(key(KeyCode::Char('y'))),
            ProcessKeyResult::Action(ProcessPanelAction::Stop(id))
        );

        panel.handle_key(key(KeyCode::Char('l')));
        panel.handle_key(key(KeyCode::Up));
        assert!(!panel.follow);
        panel.handle_key(key(KeyCode::Char('t')));
        assert!(panel.follow);
        assert_eq!(panel.log_scroll, 0);
    }


    #[test]
    fn ctrl_c_signals_pipe_process_but_writes_etx_to_pty() {
        let mut pipe = process("00000000-0000-0000-0000-000000000006", ProcessState::Running, 1);
        pipe.tty = false;
        let id = pipe.id.clone();
        let mut panel = ProcessPanel::new(vec![pipe]);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ProcessKeyResult::Action(ProcessPanelAction::Signal {
                id,
                signal: ProcessSignal::Sigint
            })
        );

        let pty = process("00000000-0000-0000-0000-000000000007", ProcessState::Running, 1);
        let id = pty.id.clone();
        let mut panel = ProcessPanel::new(vec![pty]);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ProcessKeyResult::Action(ProcessPanelAction::SendKeys {
                id,
                keys: vec![ProcessKey::CtrlC]
            })
        );
    }

    #[test]
    fn wrapped_rows_preserve_tail_and_crlf_is_one_line_break() {
        assert_eq!(wrap_log_rows("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(sanitize("one\r\ntwo\rthree"), "one\ntwo\nthree");
    }

    #[test]
    fn opening_another_process_resets_follow_and_paused_append_preserves_viewport() {
        let first = process("00000000-0000-0000-0000-000000000008", ProcessState::Running, 2);
        let second = process("00000000-0000-0000-0000-000000000009", ProcessState::Running, 1);
        let mut panel = ProcessPanel::new(vec![first, second]);
        panel.handle_key(key(KeyCode::Enter));
        panel.handle_key(key(KeyCode::Char('l')));
        panel.handle_key(key(KeyCode::Up));
        assert!(!panel.follow);
        panel.append_log(0, 12, b"one\ntwo\nnew\n");
        assert_eq!(panel.log_scroll, 4);
        panel.handle_key(key(KeyCode::Esc));
        panel.handle_key(key(KeyCode::Down));
        panel.handle_key(key(KeyCode::Enter));
        assert!(panel.follow);
        assert_eq!(panel.log_scroll, 0);
    }

    #[test]
    fn wrapped_rows_use_terminal_display_width() {
        assert_eq!(wrap_log_rows("ab🙂c", 4), vec!["ab🙂", "c"]);
    }
    #[test]
    fn invalid_resize_stays_open_and_reports_error() {
        let info = process("00000000-0000-0000-0000-000000000005", ProcessState::Running, 1);
        let mut panel = ProcessPanel::new(vec![info]);
        panel.handle_key(key(KeyCode::Enter));
        panel.handle_key(key(KeyCode::Char('r')));
        for _ in 0..5 { panel.handle_key(key(KeyCode::Backspace)); }
        panel.handle_key(key(KeyCode::Char('0')));
        panel.handle_key(key(KeyCode::Char('x')));
        panel.handle_key(key(KeyCode::Char('0')));
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), ProcessKeyResult::Handled);
        assert!(matches!(panel.input, Some(ProcessPanelInput::Resize(_))));
        assert!(panel.notice.as_deref().unwrap().contains("non-zero"));
    }

    #[test]
    fn terminal_output_strips_csi_and_osc_sequences() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m\rnext"), "red\nnext");
        assert_eq!(sanitize("a\u{1b}]0;title\u{7}b"), "ab");
    }
}
