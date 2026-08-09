//! Shared human-readable rendering for `Application` turns.

use std::future::Future;
use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, anyhow, bail};
use pi_agent::AgentEvent;
use pi_ai::{AssistantMessageEvent, Message};
use pi_coding::{Application, ApplicationEvent, SessionEvent};
use pi_coding::redact::redact_secrets;
use pi_coding::markdown::{
    MarkdownRenderOptions, StreamingMarkdownRenderer, render_markdown,
    render_markdown_streaming,
};

use crate::output::{DIM, RED, RESET};
use crate::orchestration_message::orchestration_irc_view;

/// Stateful renderer for the human-facing `ApplicationEvent` stream.
///
/// The renderer owns no session or agent state. Print mode and the line REPL
/// can therefore consume the same ordered event stream without creating a
/// second, renderer-specific session.

const FALLBACK_MARKDOWN_WIDTH: usize = 80;
const MAX_HEADLESS_MARKDOWN_WIDTH: usize = 1_000;
#[derive(Clone, Copy, Default)]
enum TerminalControlState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
    Suppressed,
}

pub struct HumanEventRenderer<'a, W> {
    writer: &'a mut W,
    ansi: bool,
    wrote_output: bool,
    ends_with_newline: bool,
    thinking_open: bool,
    current_assistant_streamed_text: bool,
    untrusted_state: TerminalControlState,
    assistant_markdown: StreamingMarkdownRenderer,
    markdown_width: usize,
}

impl<'a, W: Write> HumanEventRenderer<'a, W> {
    #[must_use]
    pub fn new(writer: &'a mut W, ansi: bool) -> Self {
        Self::with_width(writer, ansi, human_markdown_width())
    }

    /// Construct with an explicit terminal column width.
    ///
    /// Print mode and the REPL use [`Self::new`]; tests and embedders may use
    /// this seam to guarantee output agreement with neutral/TUI adapters.
    #[must_use]
    pub fn with_width(writer: &'a mut W, ansi: bool, width: usize) -> Self {
        let markdown_width = width.clamp(1, MAX_HEADLESS_MARKDOWN_WIDTH);
        Self {
            writer,
            ansi,
            wrote_output: false,
            ends_with_newline: true,
            thinking_open: false,
            current_assistant_streamed_text: false,
            untrusted_state: TerminalControlState::Ground,
            assistant_markdown: new_streaming_markdown(markdown_width),
            markdown_width,
        }
    }

    /// Render one event in the order it was published by the application.
    pub fn render(&mut self, event: &ApplicationEvent) -> io::Result<()> {
        match event {
            ApplicationEvent::Agent(event) => self.render_agent(event),
            ApplicationEvent::Session(event) => self.render_session(event),
            ApplicationEvent::RunFailed { message } => self.render_failure(message),
            _ => Ok(()),
        }
    }

    /// Append the mode adapter's historical trailing newline and flush.
    pub fn finish_turn(&mut self) -> io::Result<()> {
        self.close_thinking()?;
        self.write_rendered_assistant_markdown()?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.wrote_output = true;
        self.ends_with_newline = true;
        Ok(())
    }

    fn render_agent(&mut self, event: &AgentEvent) -> io::Result<()> {
        match event {
            AgentEvent::MessageStart {
                message: Message::Assistant(_),
            } => {
                self.current_assistant_streamed_text = false;
                self.untrusted_state = TerminalControlState::Ground;
                self.assistant_markdown = new_streaming_markdown(self.markdown_width);
                Ok(())
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingStart { .. },
                ..
            } => self.open_thinking(),
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingDelta { delta, .. },
                ..
            } => {
                if !self.thinking_open {
                    self.open_thinking()?;
                }
                self.write_untrusted(delta)
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingEnd { .. },
                ..
            } => {
                self.close_thinking()?;
                self.untrusted_state = TerminalControlState::Ground;
                self.ensure_line_boundary()
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                let followed_thinking = self.thinking_open;
                self.close_thinking()?;
                if followed_thinking {
                    self.ensure_line_boundary()?;
                }
                self.current_assistant_streamed_text = true;
                self.append_assistant_markdown(delta);
                Ok(())
            }
            AgentEvent::MessageEnd {
                message: Message::Custom(message),
            } if orchestration_irc_view(message).is_some() => {
                let irc = orchestration_irc_view(message).expect("guarded orchestration message");
                self.ensure_line_boundary()?;
                // Human print has no live agent roster; fall back to stable ids.
                let label = irc.label(irc.from.as_ref(), irc.to.as_ref());
                self.write_styled_line(DIM, &label)?;
                if !irc.body.is_empty() {
                    self.write_markdown(irc.body.as_ref())?;
                }
                if let Some(metadata) = irc.reply_metadata() {
                    self.write_styled_line(DIM, &metadata)?;
                }
                Ok(())
            }
            AgentEvent::MessageEnd {
                message: Message::Custom(message),
            } if pi_coding::loop_message_view(message).is_some() => {
                let loop_message =
                    pi_coding::loop_message_view(message).expect("guarded loop message");
                self.ensure_line_boundary()?;
                self.write_styled_line(
                    DIM,
                    &format!("Loop {} · {}", loop_message.task_id, loop_message.schedule),
                )?;
                if loop_message.prompt.is_empty() {
                    Ok(())
                } else {
                    self.write_markdown(loop_message.prompt)
                }
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => {
                self.close_thinking()?;
                if self.current_assistant_streamed_text {
                    self.write_rendered_assistant_markdown()?;
                } else {
                    let text = message.text();
                    if !text.is_empty() {
                        self.write_markdown(&text)?;
                    }
                }
                self.untrusted_state = TerminalControlState::Ground;
                self.current_assistant_streamed_text = false;
                Ok(())
            }
            AgentEvent::ToolExecutionStart {
                tool_name,
                arguments,
                ..
            } => {
                self.close_thinking()?;
                self.write_rendered_assistant_markdown()?;
                self.ensure_line_boundary()?;
                let arguments = compact_tool_arguments(arguments);
                self.write_styled_line(DIM, &format!("· {tool_name}({arguments})"))
            }
            AgentEvent::ToolExecutionEnd { is_error, .. } => self.write_styled_line(
                DIM,
                &format!("  └ {}", if *is_error { "error" } else { "ok" }),
            ),
            _ => Ok(()),
        }
    }

    fn render_session(&mut self, event: &SessionEvent) -> io::Result<()> {
        match event {
            // Foreground bash is explicitly user-invoked terminal output, so
            // preserve its byte stream. Model/provider-derived text is sanitized.
            SessionEvent::BashExecutionUpdate { delta, .. } => self.write_trusted_raw(delta),
            SessionEvent::BashExecutionEnd { message } => {
                self.ensure_line_boundary()?;
                if message.cancelled {
                    self.write_styled_line(DIM, "bash cancelled")
                } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
                    self.write_styled_line(RED, &format!("bash exited with status {code}"))
                } else {
                    Ok(())
                }
            }
            SessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                self.ensure_line_boundary()?;
                self.write_styled_line(
                    DIM,
                    &format!("retry {attempt}/{max_attempts} in {delay_ms}ms: {error_message}"),
                )
            }
            SessionEvent::AutoRetryEnd {
                success,
                final_error,
                ..
            } => {
                if *success {
                    self.write_styled_line(DIM, "retry succeeded")
                } else if let Some(error) = final_error {
                    self.write_styled_line(RED, &format!("retry failed: {error}"))
                } else {
                    Ok(())
                }
            }
            SessionEvent::CompactionStart { reason } => {
                self.ensure_line_boundary()?;
                self.write_styled_line(DIM, &format!("compacting context ({reason:?})"))
            }
            SessionEvent::CompactionEnd {
                aborted,
                will_retry,
                error_message,
                ..
            } => {
                if *aborted {
                    self.write_styled_line(DIM, "compaction aborted")
                } else if *will_retry {
                    self.write_styled_line(DIM, "compaction will retry")
                } else if let Some(error) = error_message {
                    self.write_styled_line(RED, &format!("compaction failed: {error}"))
                } else {
                    self.write_styled_line(DIM, "compaction complete")
                }
            }
            SessionEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => self.write_styled_line(
                DIM,
                &format!("summary retry {attempt}/{max_attempts} in {delay_ms}ms: {error_message}"),
            ),
            SessionEvent::SummarizationRetryAttemptStart { .. } => {
                self.write_styled_line(DIM, "retrying summary")
            }
            SessionEvent::SummarizationRetryFinished => {
                self.write_styled_line(DIM, "summary retry finished")
            }
            _ => Ok(()),
        }
    }

    fn render_failure(&mut self, message: &str) -> io::Result<()> {
        self.close_thinking()?;
        self.ensure_line_boundary()?;
        self.write_styled_line(RED, &format!("error: {message}"))
    }

    fn open_thinking(&mut self) -> io::Result<()> {
        if self.thinking_open {
            return Ok(());
        }
        self.ensure_line_boundary()?;
        if self.ansi {
            self.writer.write_all(DIM.as_bytes())?;
        }
        self.thinking_open = true;
        Ok(())
    }

    fn close_thinking(&mut self) -> io::Result<()> {
        if !self.thinking_open {
            return Ok(());
        }
        if self.ansi {
            self.writer.write_all(RESET.as_bytes())?;
        }
        self.thinking_open = false;
        Ok(())
    }

    fn ensure_line_boundary(&mut self) -> io::Result<()> {
        if self.wrote_output && !self.ends_with_newline {
            self.writer.write_all(b"\n")?;
            self.ends_with_newline = true;
        }
        Ok(())
    }

    fn write_styled_line(&mut self, style: &str, text: &str) -> io::Result<()> {
        // Status/error lines may embed provider or tool diagnostics that echo
        // tokens; redact obvious credential shapes before they reach stdout.
        let text = sanitize_terminal_text(&redact_secrets(text));
        if self.ansi {
            write!(self.writer, "{style}{text}{RESET}\n")?;
        } else {
            writeln!(self.writer, "{text}")?;
        }
        self.wrote_output = true;
        self.ends_with_newline = true;
        Ok(())
    }

    fn append_assistant_markdown(&mut self, text: &str) {
        let mut sanitized = String::with_capacity(text.len());
        sanitize_terminal_chunk(
            &redact_secrets(text),
            &mut self.untrusted_state,
            &mut sanitized,
        );
        self.assistant_markdown.push_str(&sanitized);
    }

    fn write_rendered_assistant_markdown(&mut self) -> io::Result<()> {
        let rendered = self.assistant_markdown.output().plain_text();
        if !rendered.is_empty() {
            self.write_trusted_raw(&rendered)?;
        }
        self.assistant_markdown = new_streaming_markdown(self.markdown_width);
        Ok(())
    }

    fn write_markdown(&mut self, text: &str) -> io::Result<()> {
        let sanitized = sanitize_terminal_text(&redact_secrets(text));
        let rendered = render_markdown(
            &sanitized,
            &MarkdownRenderOptions {
                width: self.markdown_width,
                ..MarkdownRenderOptions::default()
            },
        );
        self.write_trusted_raw(&rendered.plain_text())
    }

    fn write_untrusted(&mut self, text: &str) -> io::Result<()> {
        let mut sanitized = String::with_capacity(text.len());
        sanitize_terminal_chunk(
            &redact_secrets(text),
            &mut self.untrusted_state,
            &mut sanitized,
        );
        self.write_trusted_raw(&sanitized)
    }

    fn write_trusted_raw(&mut self, text: &str) -> io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        if !text.is_empty() {
            self.wrote_output = true;
            self.ends_with_newline = text.ends_with('\n');
        }
        Ok(())
    }
}

fn human_markdown_width() -> usize {
    if !io::stdout().is_terminal() {
        return FALLBACK_MARKDOWN_WIDTH;
    }
    crossterm::terminal::size()
        .map(|(columns, _)| usize::from(columns).clamp(1, MAX_HEADLESS_MARKDOWN_WIDTH))
        .unwrap_or(FALLBACK_MARKDOWN_WIDTH)
}

fn new_streaming_markdown(width: usize) -> StreamingMarkdownRenderer {
    StreamingMarkdownRenderer::new(MarkdownRenderOptions {
        width: width.clamp(1, MAX_HEADLESS_MARKDOWN_WIDTH),
        ..MarkdownRenderOptions::default()
    })
}

fn sanitize_terminal_text(text: &str) -> String {
    let mut state = TerminalControlState::Ground;
    let mut sanitized = String::with_capacity(text.len());
    sanitize_terminal_chunk(text, &mut state, &mut sanitized);
    sanitized
}

fn sanitize_terminal_chunk(
    text: &str,
    state: &mut TerminalControlState,
    sanitized: &mut String,
) {
    for character in text.chars() {
        *state = match *state {
            TerminalControlState::Ground => match character {
                '\u{1b}' => TerminalControlState::Escape,
                '\u{9b}' => TerminalControlState::Csi,
                '\u{9d}' => TerminalControlState::Osc,
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => TerminalControlState::String,
                '\n' | '\t' => {
                    sanitized.push(character);
                    TerminalControlState::Ground
                }
                character if character.is_control() => TerminalControlState::Ground,
                character => {
                    sanitized.push(character);
                    TerminalControlState::Ground
                }
            },
            TerminalControlState::Escape => match character {
                '[' => TerminalControlState::Csi,
                ']' => TerminalControlState::Osc,
                'P' | 'X' | '^' | '_' => TerminalControlState::String,
                _ => TerminalControlState::Ground,
            },
            TerminalControlState::Csi => {
                if character == 'm' {
                    // SGR can make following text deceptive even after the
                    // escape bytes are stripped. Suppress the styled tail;
                    // message boundaries reset the sanitizer state.
                    TerminalControlState::Suppressed
                } else if ('@'..='~').contains(&character) {
                    TerminalControlState::Ground
                } else {
                    TerminalControlState::Csi
                }
            }
            TerminalControlState::Osc => match character {
                '\u{7}' => TerminalControlState::Ground,
                '\u{1b}' => TerminalControlState::OscEscape,
                _ => TerminalControlState::Osc,
            },
            TerminalControlState::OscEscape => {
                if character == '\\' {
                    TerminalControlState::Ground
                } else {
                    TerminalControlState::Osc
                }
            }
            TerminalControlState::String => {
                if character == '\u{1b}' {
                    TerminalControlState::StringEscape
                } else {
                    TerminalControlState::String
                }
            }
            TerminalControlState::StringEscape => {
                if character == '\\' {
                    TerminalControlState::Ground
                } else {
                    TerminalControlState::String
                }
            }
            TerminalControlState::Suppressed => TerminalControlState::Suppressed,
        };
    }
}

/// Expand a human prompt, execute it through [`Application`], render events
/// until `AgentSettled`, and return the final assistant text.
pub async fn run_human_turn_to<W: Write>(
    application: &Application,
    prompt: &str,
    writer: &mut W,
    ansi: bool,
) -> Result<String> {
    run_human_turn_to_with_interrupt(application, prompt, writer, ansi, tokio::signal::ctrl_c())
        .await
}

/// Render application events until `AgentSettled`, with Ctrl-C aborting the
/// active application operation.
pub async fn render_until_settled<W: Write>(
    application: &Application,
    events: tokio::sync::broadcast::Receiver<ApplicationEvent>,
    writer: &mut W,
    ansi: bool,
) -> Result<()> {
    render_until_settled_with_interrupt(
        application,
        events,
        writer,
        ansi,
        tokio::signal::ctrl_c(),
    )
    .await
}

async fn render_until_settled_with_interrupt<W, F>(
    application: &Application,
    mut events: tokio::sync::broadcast::Receiver<ApplicationEvent>,
    writer: &mut W,
    ansi: bool,
    interrupt: F,
) -> Result<()>
where
    W: Write,
    F: Future<Output = io::Result<()>>,
{
    let mut renderer = HumanEventRenderer::new(writer, ansi);
    let mut interrupt = Box::pin(interrupt);
    let mut interrupted = false;
    let mut run_failure = None;
    let mut terminal_failure = None;

    loop {
        tokio::select! {
            event = events.recv() => {
                let event = event.context("application event stream closed")?;
                if let ApplicationEvent::RunFailed { message } = &event {
                    run_failure = Some(message.clone());
                }
                if let ApplicationEvent::Agent(AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                }) = &event
                    && matches!(message.stop_reason, pi_ai::StopReason::Error | pi_ai::StopReason::Aborted)
                {
                    terminal_failure = Some(message.error_message.clone().unwrap_or_else(|| {
                        format!("Request {:?}", message.stop_reason).to_ascii_lowercase()
                    }));
                }
                let settled = matches!(event, ApplicationEvent::AgentSettled);
                if let Err(error) = renderer.render(&event) {
                    application.abort().await;
                    application.abort_bash();
                    application.wait_for_idle().await;
                    return Err(error).context("rendering application event");
                }
                if settled {
                    break;
                }
            }
            signal = &mut interrupt, if !interrupted => {
                signal.context("listening for Ctrl-C")?;
                interrupted = true;
                application.abort().await;
                application.abort_bash();
            }
        }
    }

    application.wait_for_idle().await;
    renderer.finish_turn().context("finishing human turn output")?;
    if interrupted {
        return Ok(());
    }
    if let Some(message) = run_failure {
        return Err(anyhow!(message));
    }
    if let Some(message) = terminal_failure {
        return Err(anyhow!(message));
    }
    Ok(())
}

async fn run_human_turn_to_with_interrupt<W, F>(
    application: &Application,
    prompt: &str,
    writer: &mut W,
    ansi: bool,
    interrupt: F,
) -> Result<String>
where
    W: Write,
    F: Future<Output = io::Result<()>>,
{
    if prompt.trim().is_empty() {
        bail!("prompt must not be empty");
    }

    let session = application.session();
    let expanded = crate::file_args::expand_prompt_in_workspace(prompt, session.workspace_roots())?;
    let events = application.subscribe();
    application
        .prompt(expanded.prompt, expanded.images, None)
        .await?;
    render_until_settled_with_interrupt(application, events, writer, ansi, interrupt).await?;
    Ok(application.last_assistant_text().unwrap_or_default())
}

/// Execute foreground bash through [`Application`] and render its session
/// events with the same human renderer used for prompt turns.
pub async fn run_human_bash_to<W: Write>(
    application: &Application,
    command: &str,
    exclude_from_context: bool,
    writer: &mut W,
    ansi: bool,
) -> Result<()> {
    let mut events = application.subscribe();
    let mut operation = Box::pin(application.execute_bash(
        command.to_owned(),
        exclude_from_context,
    ));
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let mut interrupted = false;
    let mut renderer = HumanEventRenderer::new(writer, ansi);
    let mut saw_end = false;

    let operation_result = loop {
        tokio::select! {
            result = &mut operation => break result,
            event = events.recv() => {
                let event = event.context("application event stream closed")?;
                saw_end |= matches!(
                    event,
                    ApplicationEvent::Session(SessionEvent::BashExecutionEnd { .. })
                );
                if let Err(error) = renderer.render(&event) {
                    application.abort_bash();
                    let _ = (&mut operation).await;
                    return Err(error).context("rendering bash application event");
                }
            }
            signal = &mut interrupt, if !interrupted => {
                signal.context("listening for Ctrl-C")?;
                interrupted = true;
                application.abort_bash();
            }
        }
    };

    while !saw_end {
        let event = events
            .recv()
            .await
            .context("application event stream closed before bash settled")?;
        saw_end = matches!(
            event,
            ApplicationEvent::Session(SessionEvent::BashExecutionEnd { .. })
        );
        renderer
            .render(&event)
            .context("rendering final bash application event")?;
    }
    renderer.finish_turn().context("finishing bash output")?;
    operation_result?;
    Ok(())
}

fn compact_tool_arguments(arguments: &serde_json::Value) -> String {
    let value = ["command", "path", "pattern"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    if value.chars().count() <= 60 {
        return value.to_owned();
    }
    let prefix: String = value.chars().take(57).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use pi_agent::{AgentToolResult, StreamFn, ThinkingLevel};
    use pi_ai::{
        AssistantMessage, BashExecutionMessage, ContentBlock, Context, Model, StopReason, Usage,
        new_assistant_message_event_stream,
    };
    use pi_coding::{Session, SessionOptions};
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    fn assistant(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(text)],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 1,
        })
    }

    fn partial() -> AssistantMessage {
        match assistant("") {
            Message::Assistant(message) => message,
            _ => unreachable!(),
        }
    }

    async fn application_with_stream(stream_fn: StreamFn) -> (Application, tempfile::TempDir) {
        let cwd = tempfile::tempdir().expect("tempdir");
        let model = Model {
            id: "human-renderer".into(),
            name: "Human Renderer".into(),
            api: "human-renderer".into(),
            provider: "test".into(),
            ..Model::default()
        };
        let session = Session::new(SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        })
        .expect("session");
        (Application::new(session).await, cwd)
    }

    #[test]
    fn scheduled_turn_renders_public_loop_card_without_internal_wrapper() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: Message::Custom(pi_ai::CustomMessage {
                    custom_type: "loop_scheduled_turn".to_owned(),
                    content: "<system-reminder>internal</system-reminder>\n\necho hello".into(),
                    display: false,
                    details: Some(json!({
                        "taskId": "abc123",
                        "prompt": "echo hello",
                        "schedule": "every 3 seconds",
                    })),
                    timestamp: 1,
                }),
            }))
            .expect("render loop turn");
        drop(renderer);
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("Loop abc123 · every 3 seconds"));
        assert!(output.contains("echo hello"));
        assert!(!output.contains("system-reminder"));
    }

    #[test]
    fn orchestration_irc_renders_named_label_body_reply_without_raw_xml() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        let events = [
            ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: Message::Custom(pi_ai::CustomMessage {
                    custom_type: pi_coding::ORCHESTRATION_MESSAGE_TYPE.to_owned(),
                    content: "<orchestration-message id=\"m1\" from=\"Main\">\nhello child\n</orchestration-message>".into(),
                    display: true,
                    details: Some(json!({
                        "id": "m1",
                        "from": "Main",
                        "to": "Child",
                        "body": "hello child",
                    })),
                    timestamp: 1,
                }),
            }),
            ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: Message::Custom(pi_ai::CustomMessage {
                    custom_type: pi_coding::ORCHESTRATION_MESSAGE_TYPE.to_owned(),
                    content: "<orchestration-message id=\"m2\" from=\"Child\">\nchild ack\nReplying to message: m1\n</orchestration-message>".into(),
                    display: true,
                    details: Some(json!({
                        "id": "m2",
                        "from": "Child",
                        "to": "Sibling",
                        "body": "child ack",
                        "replyTo": "m1",
                    })),
                    timestamp: 2,
                }),
            }),
        ];
        for event in &events {
            renderer.render(event).expect("render irc");
        }
        drop(renderer);
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("IRC · Main → Child"));
        assert!(output.contains("hello child"));
        assert!(output.contains("IRC · Child → Sibling"));
        assert!(output.contains("child ack"));
        assert!(output.contains("reply to m1"));
        assert!(!output.contains("<orchestration-message"));
        assert!(!output.contains("Replying to message"));
        // Body sits on its own line beneath the label.
        let main_idx = output.find("IRC · Main → Child").expect("main label");
        let body_idx = output.find("hello child").expect("body");
        assert!(body_idx > main_idx);
    }


    #[test]
    fn renders_thinking_tools_and_assistant_in_event_order_without_ansi() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        let events = [
            ApplicationEvent::Agent(AgentEvent::MessageStart {
                message: assistant(""),
            }),
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                message: assistant("reason"),
                assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "reason".into(),
                    partial: partial(),
                },
            }),
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                message: assistant("before"),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "before".into(),
                    partial: partial(),
                },
            }),
            ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: json!({"path": "src/lib.rs"}),
            }),
            ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                result: AgentToolResult::text("contents"),
                is_error: false,
            }),
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                message: assistant("after"),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "after".into(),
                    partial: partial(),
                },
            }),
        ];
        for event in &events {
            renderer.render(event).expect("render event");
        }
        renderer.finish_turn().expect("finish turn");
        drop(renderer);

        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "reason\nbefore\n· read(src/lib.rs)\n  └ ok\nafter\n"
        );
    }

    #[test]
    fn ansi_is_opt_in() {
        let event = ApplicationEvent::RunFailed {
            message: "broken".into(),
        };
        let mut plain = Vec::new();
        HumanEventRenderer::new(&mut plain, false)
            .render(&event)
            .expect("plain render");
        assert!(!plain.contains(&0x1b));

        let mut styled = Vec::new();
        HumanEventRenderer::new(&mut styled, true)
            .render(&event)
            .expect("styled render");
        assert!(styled.contains(&0x1b));
    }

    #[test]
    fn strips_fragmented_terminal_controls_from_untrusted_text() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, true);
        for delta in ["safe\u{1b}]52;c;", "payload\u{1b}\\after\u{1b}[", "31mred"] {
            renderer
                .render(&ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                    message: assistant(""),
                    assistant_message_event: AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: delta.into(),
                        partial: partial(),
                    },
                }))
                .expect("render sanitized delta");
        }
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant("safeafter"),
            }))
            .expect("end sanitized stream");
        drop(renderer);

        assert_eq!(String::from_utf8(output).expect("utf8"), "safeafter");
    }

    #[test]
    fn preserves_foreground_bash_terminal_bytes() {
        let mut output = Vec::new();
        HumanEventRenderer::new(&mut output, false)
            .render(&ApplicationEvent::Session(SessionEvent::BashExecutionUpdate {
                id: None,
                delta: "\u{1b}[31muser-red\u{1b}[0m".into(),
            }))
            .expect("render bash bytes");
        assert_eq!(output, b"\x1b[31muser-red\x1b[0m");
    }

    #[test]
    fn print_mode_redacts_credential_shapes_but_keeps_plain_text() {
        let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant(&format!("deploy with {ghp} and token=abc123")),
            }))
            .expect("render assistant text");
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_call_id: "t".into(),
                tool_name: "bash".into(),
                arguments: json!({"command": "curl -H 'Authorization: Bearer xyz789' api"}),
            }))
            .expect("render tool line");
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                message: assistant(""),
                assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: format!("thinking about {sk}").into(),
                    partial: partial(),
                },
            }))
            .expect("render thinking");
        drop(renderer);
        let text = String::from_utf8(output).expect("utf8");
        for leaked in [ghp.as_str(), "abc123", "xyz789", sk.as_str()] {
            assert!(!text.contains(leaked), "{leaked:?} leaked into print mode: {text}");
        }
        assert_eq!(text.matches("[REDACTED]").count(), 4);
    }

    #[test]
    fn print_mode_leaves_plain_text_unchanged() {
        let mut output = Vec::new();
        HumanEventRenderer::new(&mut output, false)
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant("ordinary prose without secrets"),
            }))
            .expect("render");
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "ordinary prose without secrets"
        );
    }

    #[test]
    fn tool_argument_summary_truncates_on_character_boundaries() {
        let arguments = json!({"command": "é".repeat(61)});
        let summary = compact_tool_arguments(&arguments);
        assert_eq!(summary.chars().count(), 60);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn message_end_falls_back_when_provider_emits_no_text_delta() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageStart {
                message: assistant(""),
            }))
            .expect("start");
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant("complete"),
            }))
            .expect("end");
        drop(renderer);
        assert_eq!(String::from_utf8(output).expect("utf8"), "complete");
    }

    #[test]
    fn print_output_matches_shared_neutral_markdown() {
        let source = "# Heading\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: nope\n```";
        let width = 48;
        let expected = render_markdown(
            source,
            &MarkdownRenderOptions {
                width,
                ..MarkdownRenderOptions::default()
            },
        )
        .plain_text();
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::with_width(&mut output, false, width);
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant(source),
            }))
            .expect("render markdown");
        drop(renderer);
        assert_eq!(String::from_utf8(output).expect("utf8"), expected);
    }

    #[test]
    fn streamed_markdown_is_emitted_once_at_message_end() {
        let source = "# Stable\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
        let expected = render_markdown_streaming(
            source,
            &MarkdownRenderOptions {
                width: 40,
                ..MarkdownRenderOptions::default()
            },
        )
        .plain_text();
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::with_width(&mut output, false, 40);
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageStart {
                message: assistant(""),
            }))
            .expect("start");
        for delta in ["# Stable\n\n| a |", " b |\n| --- | --- |\n", "| 1 | 2 |"] {
            renderer
                .render(&ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                    message: assistant(""),
                    assistant_message_event: AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: delta.to_owned(),
                        partial: partial(),
                    },
                }))
                .expect("delta");
        }
        renderer
            .render(&ApplicationEvent::Agent(AgentEvent::MessageEnd {
                message: assistant(source),
            }))
            .expect("end");
        drop(renderer);
        assert_eq!(String::from_utf8(output).expect("utf8"), expected);
    }

    #[test]
    fn failure_is_rendered_without_structured_output() {
        let mut output = Vec::new();
        HumanEventRenderer::new(&mut output, false)
            .render(&ApplicationEvent::RunFailed {
                message: "provider unavailable".into(),
            })
            .expect("render failure");
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "error: provider unavailable\n"
        );
    }


    #[test]
    fn renders_bash_retry_and_compaction_events_without_ansi() {
        let mut output = Vec::new();
        let mut renderer = HumanEventRenderer::new(&mut output, false);
        let events = [
            ApplicationEvent::Session(SessionEvent::BashExecutionUpdate {
                id: None,
                delta: "command output".into(),
            }),
            ApplicationEvent::Session(SessionEvent::BashExecutionEnd {
                message: BashExecutionMessage {
                    command: "false".into(),
                    output: "command output".into(),
                    exit_code: Some(1),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 1,
                    exclude_from_context: None,
                },
            }),
            ApplicationEvent::Session(SessionEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 500,
                error_message: "busy".into(),
            }),
            ApplicationEvent::Session(SessionEvent::CompactionStart {
                reason: pi_coding::CompactionReason::Threshold,
            }),
        ];
        for event in &events {
            renderer.render(event).expect("render session event");
        }
        drop(renderer);

        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "command output\nbash exited with status 1\nretry 1/3 in 500ms: busy\ncompacting context (Threshold)\n"
        );
    }
    #[tokio::test]
    async fn human_turn_prompts_application_once_and_returns_final_text() {
        let calls = Arc::new(Mutex::new(0usize));
        let observed_calls = calls.clone();
        let stream_fn: StreamFn = Arc::new(move |model, _context: Context, _options| {
            let observed_calls = observed_calls.clone();
            Box::pin(async move {
                *observed_calls.lock().expect("calls lock") += 1;
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    producer
                        .push(AssistantMessageEvent::Start {
                            partial: message.clone(),
                        })
                        .await;
                    message.content = vec![ContentBlock::text("one run")];
                    message.stop_reason = StopReason::Stop;
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    producer.end(Some(message)).await;
                });
                stream
            })
        });
        let (application, _cwd) = application_with_stream(stream_fn).await;
        let mut output = Vec::new();

        let text = run_human_turn_to_with_interrupt(
            &application,
            "go",
            &mut output,
            false,
            std::future::pending(),
        )
        .await
        .expect("turn");

        assert_eq!(text, "one run");
        assert_eq!(*calls.lock().expect("calls lock"), 1);
        assert_eq!(String::from_utf8(output).expect("utf8"), "one run\n");
    }

    #[tokio::test]
    async fn interrupt_aborts_application_and_waits_for_settled() {
        let started = Arc::new(Notify::new());
        let abort_observed = Arc::new(AtomicBool::new(false));
        let stream_started = started.clone();
        let stream_abort_observed = abort_observed.clone();
        let stream_fn: StreamFn = Arc::new(move |model, _context: Context, options| {
            let stream_started = stream_started.clone();
            let stream_abort_observed = stream_abort_observed.clone();
            Box::pin(async move {
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    producer
                        .push(AssistantMessageEvent::Start {
                            partial: message.clone(),
                        })
                        .await;
                    stream_started.notify_one();
                    if let Some(abort) = options.stream.abort_signal {
                        abort.cancelled().await;
                        stream_abort_observed.store(true, Ordering::Release);
                    }
                    message.stop_reason = StopReason::Aborted;
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: message.clone(),
                        })
                        .await;
                    producer.end(Some(message)).await;
                });
                stream
            })
        });
        let (application, _cwd) = application_with_stream(stream_fn).await;
        let interrupt = async move {
            started.notified().await;
            Ok(())
        };
        let mut output = Vec::new();

        let result =
            run_human_turn_to_with_interrupt(&application, "wait", &mut output, false, interrupt)
                .await;

        assert!(
            result.is_ok(),
            "aborted turn should settle cleanly: {result:?}"
        );
        assert!(!application.is_streaming());
        assert!(abort_observed.load(Ordering::Acquire));
        assert!(String::from_utf8(output).expect("utf8").ends_with('\n'));
    }
}
