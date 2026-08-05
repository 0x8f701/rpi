//! Built-in `/btw` side-chat integration contracts.
//!
//! Drives the public `Application` + `SideChatController` boundaries only.
//! No production modules are modified. Tests stay offline via injected stream
//! functions and never touch real provider networks or credentials.
//!
//! TUI overlay open/close helpers are private on `TuiState`; reopen/cleanup are
//! validated through the controller contracts those helpers call
//! (`CloseOverlay` keeps state; `shutdown` is TUI-exit cleanup).

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_agent::{StreamFn, ThinkingLevel, ToolCapability};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, Model, SimpleStreamOptions,
    StopReason, ToolCall, Transport, new_assistant_message_event_stream,
};
use pi_cli::interactive_commands::{
    BUILTIN_COMMANDS, PRIMARY_COMMAND_NAMES, builtin, executable_catalog,
};
use pi_cli::side_chat::{
    SideChatAction, SideChatAsyncRequest, SideChatController, SideChatRole, SideChatToolMode,
};
use pi_cli::side_chat_panel::render_side_chat_panel;
use pi_cli::theme;
use pi_coding::{
    Application, RequestAuth, Session, SessionAuthResolver, SessionOptions, TodoItem, TodoPhase,
    TodoStatus, create_all_tools, create_coding_tools, tools_include_mutation,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;

fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::now_v7().simple())
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn message_text(message: &Message) -> String {
    match message {
        Message::User(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Message::Assistant(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Message::ToolResult(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn immediate_reply_stream(text: impl Into<String>) -> StreamFn {
    let text = text.into();
    Arc::new(move |model, _context, _options| {
        let text = text.clone();
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text(text));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    })
}

fn scripted_assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&Model::default());
    message.content = content;
    message.stop_reason = stop_reason;
    message.api = "scripted".into();
    message.provider = "scripted".into();
    message.model = "scripted".into();
    message
}

/// Multi-turn scripted stream: each agent prompt pops the next assistant message.
/// Used to force the installed side-chat `write` tool to run via the controller.
fn scripted_reply_stream(messages: Vec<AssistantMessage>) -> StreamFn {
    let messages = Arc::new(Mutex::new(VecDeque::from(messages)));
    Arc::new(move |model, _context, _options| {
        let messages = messages.clone();
        Box::pin(async move {
            let message = messages
                .lock()
                .expect("scripted stream lock")
                .pop_front()
                .unwrap_or_else(|| {
                    scripted_assistant(vec![ContentBlock::text("done")], StopReason::Stop)
                });
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut partial = AssistantMessage::pending(&model);
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    })
                    .await;
                // Keep provider/model fields aligned with the live turn model.
                partial.api = model.api.clone();
                partial.provider = model.provider.clone();
                partial.model = model.id.clone();
                let mut terminal = message;
                terminal.api = model.api.clone();
                terminal.provider = model.provider.clone();
                terminal.model = model.id.clone();
                producer
                    .push(AssistantMessageEvent::Done {
                        reason: terminal.stop_reason,
                        message: terminal.clone(),
                    })
                    .await;
                producer.end(Some(terminal)).await;
            });
            stream
        })
    })
}

/// Stream that signals start, emits a text delta, then waits for abort/release.
fn gated_reply_stream(
    text: impl Into<String>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    call_count: Arc<AtomicUsize>,
) -> StreamFn {
    let text = text.into();
    Arc::new(move |model, _context, options| {
        let text = text.clone();
        let started = started.clone();
        let release = release.clone();
        let call_count = call_count.clone();
        Box::pin(async move {
            call_count.fetch_add(1, Ordering::SeqCst);
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text(String::new()));
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: message.clone(),
                    })
                    .await;
                producer
                    .push(AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: message.clone(),
                    })
                    .await;
                producer
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: text.clone(),
                        partial: message.clone(),
                    })
                    .await;
                started.notify_one();

                // Prefer abort signal when present; otherwise wait for test release.
                if let Some(abort) = options.stream.abort_signal.clone() {
                    tokio::select! {
                        _ = abort.cancelled() => {
                            message.stop_reason = StopReason::Aborted;
                            message.error_message = Some("aborted".into());
                            producer
                                .push(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: message.clone(),
                                })
                                .await;
                            producer.end(Some(message)).await;
                        }
                        _ = release.notified() => {
                            if let Some(ContentBlock::Text { text: body, .. }) =
                                message.content.get_mut(0)
                            {
                                *body = text.clone();
                            }
                            message.stop_reason = StopReason::Stop;
                            producer
                                .push(AssistantMessageEvent::TextEnd {
                                    content_index: 0,
                                    content: text.clone(),
                                    partial: message.clone(),
                                })
                                .await;
                            producer
                                .push(AssistantMessageEvent::Done {
                                    reason: StopReason::Stop,
                                    message: message.clone(),
                                })
                                .await;
                            producer.end(Some(message)).await;
                        }
                    }
                } else {
                    release.notified().await;
                    if let Some(ContentBlock::Text { text: body, .. }) = message.content.get_mut(0)
                    {
                        *body = text.clone();
                    }
                    message.stop_reason = StopReason::Stop;
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    producer.end(Some(message)).await;
                }
            });
            stream
        })
    })
}

fn session_options(model: Model, cwd: &Path, stream_fn: Option<StreamFn>) -> SessionOptions {
    SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: "main system prompt".to_owned(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux-side-chat".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(create_coding_tools(&cwd.to_string_lossy())),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn,
        auth_resolver: None,
    }
}

fn test_model(label: &str) -> Model {
    Model {
        id: format!("{label}-model"),
        name: format!("{label} Model"),
        api: unique(&format!("{label}-api")),
        provider: unique(&format!("{label}-provider")),
        base_url: "http://localhost:0".into(),
        ..Model::default()
    }
}

async fn application_with_history(
    cwd: &Path,
    history: Vec<Message>,
    stream_fn: Option<StreamFn>,
) -> Application {
    let session =
        Session::new(session_options(test_model("side"), cwd, stream_fn)).expect("session");
    if !history.is_empty() {
        session.load_history(history).await.expect("load history");
    }
    Application::new(session).await
}

async fn recorded_application(
    cwd: &Path,
    session_dir: &Path,
    session_id: &str,
    history: Vec<Message>,
    stream_fn: Option<StreamFn>,
) -> (Application, PathBuf) {
    let model = test_model("recorded-side");
    let session = Session::new(session_options(model.clone(), cwd, stream_fn)).expect("session");
    let recorder = pi_coding::start_session_in(
        cwd,
        Some(&model),
        Some("off"),
        Some(session_dir),
        Some(session_id),
        None,
    )
    .expect("start recorder");
    let path = recorder.path();
    session.record(recorder).expect("attach recorder");
    if !history.is_empty() {
        session.load_history(history).await.expect("load history");
    }
    (Application::new(session).await, path)
}

async fn wait_until<F>(mut predicate: F, timeout: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate()
}

/// Snapshot of main identity used to prove side chat never mutates main.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MainIdentity {
    session_id: Option<String>,
    session_file: Option<String>,
    message_count: usize,
    messages: Vec<String>,
}

async fn capture_main(application: &Application) -> MainIdentity {
    let state = application.state().await;
    MainIdentity {
        session_id: state.session_id,
        session_file: state.session_file,
        message_count: state.message_count,
        messages: application.messages().iter().map(message_text).collect(),
    }
}

fn assert_main_unchanged(before: &MainIdentity, after: &MainIdentity, context: &str) {
    assert_eq!(
        after.session_id, before.session_id,
        "{context}: main session_id must not change"
    );
    assert_eq!(
        after.session_file, before.session_file,
        "{context}: main session_file must not change"
    );
    assert_eq!(
        after.message_count, before.message_count,
        "{context}: main message_count must not change"
    );
    assert_eq!(
        after.messages, before.messages,
        "{context}: main messages must not change"
    );
}

#[test]
fn btw_is_registered_primary_slash_command() {
    assert!(
        PRIMARY_COMMAND_NAMES.contains(&"btw"),
        "PRIMARY_COMMAND_NAMES must advertise /btw: {PRIMARY_COMMAND_NAMES:?}"
    );
    assert!(
        BUILTIN_COMMANDS.iter().any(|command| command.name == "btw"),
        "BUILTIN_COMMANDS must include btw"
    );
    let btw = builtin("btw").expect("/btw builtin missing");
    assert!(
        btw.description.to_ascii_lowercase().contains("side"),
        "btw description should mention side chat: {}",
        btw.description
    );
    assert_eq!(btw.argument_hint, Some("[prompt]"));
    assert!(!btw.requires_arguments);
}

#[tokio::test]
async fn fork_copies_main_context_without_mutating_main_identity_or_file() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-main");
    let history = vec![
        Message::user_text("main alpha", 1),
        Message::user_text("main beta", 2),
    ];
    let (application, session_path) = recorded_application(
        cwd.path(),
        session_dir.path(),
        &session_id,
        history,
        Some(immediate_reply_stream("side should not hit main")),
    )
    .await;
    let before = capture_main(&application).await;
    let before_bytes = fs::read(&session_path).expect("read session file before");

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork side chat");
    assert_eq!(side.cwd(), cwd.path());
    assert_eq!(side.tool_mode(), SideChatToolMode::ReadOnly);

    // Forked agent starts from the same branch messages.
    let side_messages = side.agent_messages().await;
    assert!(
        side_messages
            .iter()
            .any(|message| message_text(message) == "main alpha"),
        "fork must include main branch context: {side_messages:?}"
    );
    assert!(
        side_messages
            .iter()
            .any(|message| message_text(message) == "main beta"),
        "fork must include full main branch: {side_messages:?}"
    );

    // Side prompt + editor activity stay local.
    side.handle_paste("editor only");
    assert_eq!(side.editor_text(), "editor only");
    side.submit_prompt("side-only prompt");
    assert!(
        side.entries()
            .iter()
            .any(|entry| entry.role == SideChatRole::User && entry.text == "side-only prompt")
    );

    let after = capture_main(&application).await;
    assert_main_unchanged(&before, &after, "after side prompt");
    let after_bytes = fs::read(&session_path).expect("read session file after");
    assert_eq!(
        after_bytes, before_bytes,
        "side chat must not append structured session JSONL / stdout records"
    );
    assert!(
        !String::from_utf8_lossy(&after_bytes).contains("side-only prompt"),
        "side prompt leaked into main session file"
    );

    side.shutdown().await;
    let after_shutdown = capture_main(&application).await;
    assert_main_unchanged(&before, &after_shutdown, "after side shutdown");
}

#[tokio::test]
async fn independent_transcript_editor_stream_events_and_abort() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let stream = gated_reply_stream(
        "side streaming text",
        started.clone(),
        release.clone(),
        calls.clone(),
    );
    let application = application_with_history(
        cwd.path(),
        vec![Message::user_text("shared root", 1)],
        Some(stream),
    )
    .await;
    let before = capture_main(&application).await;

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    // Independent editor.
    side.handle_paste("draft stays in side");
    assert_eq!(side.editor_text(), "draft stays in side");
    assert_eq!(
        capture_main(&application).await.messages,
        before.messages,
        "editor paste must not touch main"
    );

    // Independent stream + events.
    side.submit_prompt("stream please");
    assert!(side.is_streaming(), "submit should mark side streaming");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                side.is_streaming() || calls.load(Ordering::SeqCst) > 0
            },
            Duration::from_secs(2)
        )
        .await,
        "side stream never started; calls={}",
        calls.load(Ordering::SeqCst)
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), started.notified()).await;
    let _ = side.poll_events();
    assert!(
        !side.streaming_text().is_empty() || side.is_streaming() || !side.entries().is_empty(),
        "side must own streaming/transcript state after prompt"
    );
    assert!(
        application
            .messages()
            .iter()
            .all(|message| message_text(message) != "stream please"),
        "side user prompt must not enter main transcript"
    );
    assert!(
        side.entries()
            .iter()
            .any(|entry| entry.role == SideChatRole::User && entry.text == "stream please"),
        "side transcript must own the user turn"
    );

    // Independent abort: stops side stream only.
    side.abort_streaming().await;
    assert!(!side.is_streaming(), "abort must clear side streaming flag");
    assert!(
        side.status().to_ascii_lowercase().contains("abort"),
        "abort status missing: {}",
        side.status()
    );
    let after_abort = capture_main(&application).await;
    assert_main_unchanged(&before, &after_abort, "after side abort");

    release.notify_waiters();
    side.shutdown().await;
}

#[tokio::test]
async fn read_only_default_exposes_peek_main_without_write_or_exec() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(cwd.path(), Vec::new(), None).await;
    let side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    assert_eq!(side.tool_mode(), SideChatToolMode::ReadOnly);
    assert!(!side.show_edit_warning());
    assert!(
        side.status().to_ascii_lowercase().contains("read-only")
            || side.status().to_ascii_lowercase().contains("read only"),
        "status should advertise read-only default: {}",
        side.status()
    );

    let caps = side.tool_capabilities().await;
    assert!(
        caps.iter()
            .all(|(_, capability)| *capability == ToolCapability::Read),
        "default side tools must be Read-only by capability: {caps:?}"
    );
    assert!(
        caps.iter().any(|(name, _)| name == "peek_main"),
        "default tool set must include peek_main: {caps:?}"
    );
    assert!(
        caps.iter()
            .all(|(name, _)| name != "write" && name != "bash" && name != "edit"),
        "mutation tools must stay out of the default set: {caps:?}"
    );

    // Application-level open helper matches the same contract.
    let (_fork, agent) = application
        .open_side_chat_agent()
        .await
        .expect("open_side_chat_agent");
    let agent_tools = agent.state().await.tools;
    assert!(
        !tools_include_mutation(&agent_tools),
        "open_side_chat_agent must default to non-mutating tools"
    );
}

#[tokio::test]
async fn ctrl_t_toggles_edit_mode_with_warning_then_restores_readonly() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(cwd.path(), Vec::new(), None).await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    let before = capture_main(&application).await;

    let ctrl_t = key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(side.handle_key(ctrl_t), SideChatAction::Handled);
    assert_eq!(
        side.key_needs_async(ctrl_t),
        SideChatAsyncRequest::ToggleTools,
        "Ctrl+T must request tool-mode toggle"
    );

    side.toggle_tool_mode().await.expect("enter edit mode");
    assert!(side.tool_mode().is_edit());
    assert!(
        side.show_edit_warning(),
        "edit mode must raise visible warning flag"
    );
    assert!(
        side.status().to_ascii_lowercase().contains("edit"),
        "edit status missing: {}",
        side.status()
    );
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::System
                && (entry.text.contains("Edit mode")
                    || entry.text.contains("Write/Exec")
                    || entry.text.contains("overlap"))
        }),
        "edit mode must append an advisory system warning: {:?}",
        side.entries()
    );
    let edit_caps = side.tool_capabilities().await;
    assert!(
        edit_caps.iter().any(|(_, capability)| matches!(
            capability,
            ToolCapability::Write | ToolCapability::Exec
        )),
        "edit mode must enable Write/Exec capabilities: {edit_caps:?}"
    );
    assert!(
        edit_caps.iter().any(|(name, _)| name == "peek_main"),
        "peek_main must remain available in edit mode"
    );

    // Toggle back to read-only.
    assert_eq!(
        side.key_needs_async(ctrl_t),
        SideChatAsyncRequest::ToggleTools
    );
    side.toggle_tool_mode().await.expect("restore read-only");
    assert_eq!(side.tool_mode(), SideChatToolMode::ReadOnly);
    assert!(!side.show_edit_warning());
    let restored = side.tool_capabilities().await;
    assert!(
        restored
            .iter()
            .all(|(_, capability)| *capability == ToolCapability::Read),
        "restored tools must be Read-only: {restored:?}"
    );
    assert!(
        side.entries()
            .iter()
            .any(|entry| entry.text.contains("Read-only mode restored")),
        "restoring read-only should record a system note"
    );

    assert_main_unchanged(&before, &capture_main(&application).await, "mode toggle");
    side.shutdown().await;
}

#[tokio::test]
async fn ctrl_t_is_rejected_while_streaming_and_tools_stay_in_sync_with_status() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let application = application_with_history(
        cwd.path(),
        Vec::new(),
        Some(gated_reply_stream(
            "edit turn remains active",
            started.clone(),
            release.clone(),
            calls.clone(),
        )),
    )
    .await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    side.toggle_tool_mode().await.expect("enter edit mode");
    assert!(side.tool_mode().is_edit());
    side.submit_prompt("hold edit tools");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                side.is_streaming() && calls.load(Ordering::SeqCst) > 0
            },
            Duration::from_secs(2),
        )
        .await,
        "side turn never became active"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), started.notified()).await;

    let ctrl_t = key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(side.handle_key(ctrl_t), SideChatAction::Handled);
    assert_eq!(side.key_needs_async(ctrl_t), SideChatAsyncRequest::None);
    side.toggle_tool_mode().await.expect("guarded direct toggle");
    assert!(side.tool_mode().is_edit(), "active turn must retain edit mode");
    let active_caps = side.tool_capabilities().await;
    assert!(
        active_caps
            .iter()
            .any(|(_, capability)| matches!(capability, ToolCapability::Write | ToolCapability::Exec)),
        "active turn tools changed behind its captured context: {active_caps:?}"
    );
    assert!(
        side.status().to_ascii_lowercase().contains("abort")
            && side.status().to_ascii_lowercase().contains("stream"),
        "rejected toggle must explain the required action: {}",
        side.status()
    );

    side.abort_streaming().await;
    assert!(!side.is_streaming());
    side.toggle_tool_mode().await.expect("toggle after idle");
    assert_eq!(side.tool_mode(), SideChatToolMode::ReadOnly);
    assert!(
        side.tool_capabilities()
            .await
            .iter()
            .all(|(_, capability)| *capability == ToolCapability::Read)
    );
    release.notify_waiters();
    side.shutdown().await;
}

#[tokio::test]
async fn peek_main_is_visible_and_read_only() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(
        cwd.path(),
        vec![
            Message::user_text("visible-one", 1),
            Message::user_text("visible-two", 2),
        ],
        None,
    )
    .await;
    let before = capture_main(&application).await;
    let side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    let names = side.tool_names().await;
    assert!(
        names.iter().any(|name| name == "peek_main"),
        "peek_main tool must be registered: {names:?}"
    );

    let peek = side.peek_main(false).expect("peek_main full");
    assert!(
        peek.messages.len() >= 2,
        "peek_main must surface main history: {:?}",
        peek.messages.len()
    );
    let texts: Vec<String> = peek.messages.iter().map(message_text).collect();
    assert!(
        texts.iter().any(|text| text == "visible-one"),
        "missing visible-one in peek: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "visible-two"),
        "missing visible-two in peek: {texts:?}"
    );

    // Direct Application seam stays read-only too.
    let app_peek = application.peek_main_history(None).expect("app peek");
    assert_eq!(app_peek.messages.len(), peek.messages.len());

    assert_main_unchanged(&before, &capture_main(&application).await, "peek_main");
}

#[tokio::test]
async fn peek_main_unrecorded_since_fork_returns_new_active_history() {
    let cwd = TempDir::new().expect("cwd");
    let session = Session::new(session_options(test_model("unrecorded-peek"), cwd.path(), None))
        .expect("session");
    session
        .load_history(vec![Message::user_text("before fork", 1)])
        .await
        .expect("initial history");
    let application = Application::new(session.clone()).await;
    let side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    assert!(side.fork_leaf_id().is_none());
    session
        .load_history(vec![
            Message::user_text("before fork", 1),
            Message::user_text("after fork", 2),
        ])
        .await
        .expect("updated history");

    let peek = side.peek_main(true).expect("unrecorded since-fork peek");
    let texts = peek.messages.iter().map(message_text).collect::<Vec<_>>();
    assert_eq!(texts, vec!["after fork"]);
    assert!(peek.session_id.is_none());
    assert!(peek.session_file.is_none());
}

#[tokio::test]
async fn esc_aborts_while_streaming_and_closes_when_idle() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let application = application_with_history(
        cwd.path(),
        Vec::new(),
        Some(gated_reply_stream(
            "partial side",
            started.clone(),
            release.clone(),
            calls.clone(),
        )),
    )
    .await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    // Idle Esc closes overlay (controller stays alive for reopen).
    let esc = key(KeyCode::Esc);
    assert_eq!(side.handle_key(esc), SideChatAction::CloseOverlay);
    assert_eq!(side.key_needs_async(esc), SideChatAsyncRequest::None);

    // Streaming Esc aborts rather than closing.
    side.submit_prompt("hold this stream");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                side.is_streaming() || calls.load(Ordering::SeqCst) > 0
            },
            Duration::from_secs(2)
        )
        .await,
        "stream did not start"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), started.notified()).await;
    let _ = side.poll_events();

    assert!(
        side.is_streaming(),
        "controller must still be streaming for Esc-abort routing"
    );
    assert_eq!(side.handle_key(esc), SideChatAction::Handled);
    assert_eq!(side.key_needs_async(esc), SideChatAsyncRequest::Abort);
    side.abort_streaming().await;
    assert!(!side.is_streaming());
    assert!(
        side.status().to_ascii_lowercase().contains("abort"),
        "abort status missing: {}",
        side.status()
    );

    // After abort/idle, Esc closes again.
    assert_eq!(side.handle_key(esc), SideChatAction::CloseOverlay);
    assert_eq!(side.key_needs_async(esc), SideChatAsyncRequest::None);

    release.notify_waiters();
    side.shutdown().await;
}

#[tokio::test]
async fn abort_preserves_unpolled_stream_text_and_reaches_idle() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let application = application_with_history(
        cwd.path(),
        Vec::new(),
        Some(gated_reply_stream(
            "queued partial",
            started.clone(),
            release.clone(),
            calls.clone(),
        )),
    )
    .await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    side.submit_prompt("abort before poll");
    let _ = tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("stream start");

    side.abort_streaming().await;
    assert!(!side.is_streaming());
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Assistant
                && entry.is_partial
                && entry.text.contains("queued partial")
        }),
        "abort should retain already-emitted unpolled text: {:?}",
        side.entries()
    );
    assert!(!side.poll_events(), "abort must drain terminal events after idle");
    release.notify_waiters();
    side.shutdown().await;
}

#[tokio::test]
async fn alt_r_reforks_from_main_and_discards_side_transcript() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(
        cwd.path(),
        vec![Message::user_text("root before side", 1)],
        Some(immediate_reply_stream("side reply body")),
    )
    .await;
    let before = capture_main(&application).await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    side.submit_prompt("discard me on refork");
    assert!(
        side.entries()
            .iter()
            .any(|entry| entry.text == "discard me on refork")
    );

    // Grow main after the original fork so refork picks up newer context.
    application
        .session()
        .load_history(vec![
            Message::user_text("root before side", 1),
            Message::user_text("main grew after fork", 2),
        ])
        .await
        .expect("grow main history");
    let main_after_growth = capture_main(&application).await;

    let alt_r = key_mod(KeyCode::Char('r'), KeyModifiers::ALT);
    assert_eq!(side.handle_key(alt_r), SideChatAction::Handled);
    assert_eq!(side.key_needs_async(alt_r), SideChatAsyncRequest::Refork);
    side.refork_from_main().await.expect("refork");

    assert!(
        side.entries().is_empty(),
        "refork must discard side transcript entries: {:?}",
        side.entries()
    );
    assert!(
        side.status().to_ascii_lowercase().contains("refork"),
        "refork status missing: {}",
        side.status()
    );
    let reforked_messages = side.agent_messages().await;
    assert!(
        reforked_messages
            .iter()
            .any(|message| message_text(message) == "main grew after fork"),
        "refork must capture the current main leaf: {reforked_messages:?}"
    );
    assert!(
        reforked_messages
            .iter()
            .all(|message| message_text(message) != "discard me on refork"),
        "reforked agent must not keep discarded side user text"
    );

    // Main identity remains the post-growth main; side never rewrote it.
    assert_eq!(
        capture_main(&application).await.messages,
        main_after_growth.messages
    );
    assert_ne!(
        before.messages, main_after_growth.messages,
        "precondition: main actually grew"
    );
    side.shutdown().await;
}

#[tokio::test]
async fn alt_n_clears_side_conversation_while_keeping_mode() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(
        cwd.path(),
        vec![Message::user_text("fork root", 1)],
        Some(immediate_reply_stream("cleared later")),
    )
    .await;
    let before = capture_main(&application).await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    side.toggle_tool_mode().await.expect("enter edit");
    assert!(side.tool_mode().is_edit());
    side.submit_prompt("clear this turn");
    assert!(!side.entries().is_empty());

    let alt_n = key_mod(KeyCode::Char('n'), KeyModifiers::ALT);
    assert_eq!(side.handle_key(alt_n), SideChatAction::Handled);
    assert_eq!(side.key_needs_async(alt_n), SideChatAsyncRequest::Clear);
    side.clear_conversation().await.expect("clear");

    assert!(
        side.entries().is_empty(),
        "clear must wipe side transcript: {:?}",
        side.entries()
    );
    assert!(
        side.status().to_ascii_lowercase().contains("clear"),
        "clear status missing: {}",
        side.status()
    );
    assert!(
        side.tool_mode().is_edit(),
        "clear must preserve the active tool mode"
    );
    let cleared_agent = side.agent_messages().await;
    assert!(
        cleared_agent
            .iter()
            .any(|message| message_text(message) == "fork root"),
        "clear resets to fork snapshot, not an empty agent: {cleared_agent:?}"
    );
    assert!(
        cleared_agent
            .iter()
            .all(|message| message_text(message) != "clear this turn"),
        "cleared agent must drop side-only turns"
    );

    assert_main_unchanged(&before, &capture_main(&application).await, "clear");
    side.shutdown().await;
}

#[tokio::test]
async fn overlay_close_keeps_controller_state_for_reopen_within_process() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(
        cwd.path(),
        Vec::new(),
        Some(immediate_reply_stream("persisted reply")),
    )
    .await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    side.handle_paste("reopen-draft");
    side.submit_prompt("remember across overlay close");
    let deadline = Instant::now() + Duration::from_secs(5);
    while side.is_streaming() && Instant::now() < deadline {
        side.poll_events();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    side.poll_events();
    assert!(!side.is_streaming(), "side chat must settle before testing idle Esc");
    let entries_snapshot: Vec<(SideChatRole, String)> = side
        .entries()
        .iter()
        .map(|entry| (entry.role, entry.text.clone()))
        .collect();
    assert!(
        entries_snapshot
            .iter()
            .any(|(_, text)| text == "remember across overlay close")
    );

    // Overlay close is CloseOverlay only — not shutdown. TUI keeps the same
    // controller instance in Option<SideChatController> across reopen.
    let esc = key(KeyCode::Esc);
    assert_eq!(
        side.handle_key(esc),
        SideChatAction::CloseOverlay,
        "idle Esc closes overlay without dropping controller"
    );
    let entries_after_close: Vec<(SideChatRole, String)> = side
        .entries()
        .iter()
        .map(|entry| (entry.role, entry.text.clone()))
        .collect();
    assert_eq!(
        entries_after_close, entries_snapshot,
        "closing the overlay must not wipe side transcript"
    );
    // Editor was cleared on submit; paste a new draft to prove editor still works.
    side.handle_paste("second open draft");
    assert_eq!(side.editor_text(), "second open draft");

    side.shutdown().await;
}

#[tokio::test]
async fn shutdown_cleans_streaming_state_like_tui_exit() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let application = application_with_history(
        cwd.path(),
        Vec::new(),
        Some(gated_reply_stream(
            "exit cleanup",
            started.clone(),
            release.clone(),
            calls.clone(),
        )),
    )
    .await;
    let before = capture_main(&application).await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    side.submit_prompt("still running at exit");
    let _ = wait_until(
        || {
            let _ = side.poll_events();
            side.is_streaming() || calls.load(Ordering::SeqCst) > 0
        },
        Duration::from_secs(2),
    )
    .await;

    // TUI exit path calls shutdown_side_chat → controller.shutdown().
    side.shutdown().await;
    assert!(
        !side.is_streaming(),
        "TUI-exit shutdown must stop side streaming"
    );
    assert_main_unchanged(
        &before,
        &capture_main(&application).await,
        "shutdown cleanup",
    );

    release.notify_waiters();
}

#[tokio::test]
async fn abort_then_clear_or_refork_cannot_replay_old_stream_events() {
    let cwd = TempDir::new().expect("cwd");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let application = application_with_history(
        cwd.path(),
        vec![Message::user_text("main root", 1)],
        Some(gated_reply_stream(
            "stale side output",
            started.clone(),
            release.clone(),
            calls.clone(),
        )),
    )
    .await;
    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");

    side.submit_prompt("clear old turn");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                side.is_streaming() && calls.load(Ordering::SeqCst) > 0
            },
            Duration::from_secs(2),
        )
        .await,
        "clear stream never started"
    );
    side.clear_conversation().await.expect("clear after abort");
    assert!(side.entries().is_empty());
    assert_eq!(side.status(), "Side chat cleared");
    assert!(!side.poll_events(), "old clear events remained queued");
    assert!(side.entries().is_empty());
    assert_eq!(side.status(), "Side chat cleared");

    side.submit_prompt("refork old turn");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                side.is_streaming() && calls.load(Ordering::SeqCst) > 1
            },
            Duration::from_secs(2),
        )
        .await,
        "refork stream never started"
    );
    side.refork_from_main().await.expect("refork after abort");
    assert!(side.entries().is_empty());
    assert!(side.status().to_ascii_lowercase().contains("refork"));
    assert!(!side.poll_events(), "old refork events remained queued");
    assert!(side.entries().is_empty());
    assert!(side.status().to_ascii_lowercase().contains("refork"));

    release.notify_waiters();
    side.shutdown().await;
}
#[tokio::test]
async fn side_chat_never_mutates_main_session_id_file_messages_or_structured_stdout() {

    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-isolation");
    let (application, session_path) = recorded_application(
        cwd.path(),
        session_dir.path(),
        &session_id,
        vec![Message::user_text("structured root", 1)],
        Some(immediate_reply_stream("side structured reply")),
    )
    .await;
    let before = capture_main(&application).await;
    assert_eq!(before.session_id.as_deref(), Some(session_id.as_str()));
    assert!(
        before
            .session_file
            .as_ref()
            .is_some_and(|path| path == &session_path.to_string_lossy()),
        "expected recorded session file {:?}",
        before.session_file
    );
    let before_file = fs::read_to_string(&session_path).expect("session file before");

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    side.toggle_tool_mode().await.expect("edit");
    side.handle_paste("no main leak");
    side.submit_prompt("isolation probe");
    let _ = side.peek_main(false).expect("peek");
    side.clear_conversation().await.expect("clear");
    side.refork_from_main().await.expect("refork");
    side.shutdown().await;

    let after = capture_main(&application).await;
    assert_main_unchanged(&before, &after, "full side lifecycle");
    let after_file = fs::read_to_string(&session_path).expect("session file after");
    assert_eq!(
        after_file, before_file,
        "side lifecycle must not mutate structured session JSONL"
    );
    for needle in [
        "isolation probe",
        "no main leak",
        "side structured reply",
        "Edit mode enabled",
    ] {
        assert!(
            !after_file.contains(needle),
            "session file unexpectedly contains side artifact {needle:?}: {after_file}"
        );
    }
}

#[tokio::test]
async fn executable_catalog_lists_btw_without_requiring_credentials() {
    let cwd = TempDir::new().expect("cwd");
    let application = application_with_history(cwd.path(), Vec::new(), None).await;
    let (commands, _diagnostics) = executable_catalog(&application);
    assert!(
        commands.iter().any(|command| command.name == "btw"),
        "executable catalog must include btw: {:?}",
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>()
    );
}

/// Edit mode must install fresh workspace tools only — never main-runtime
/// control tools (todo/goal/task/hub/process/extension). Driving a side prompt
/// that emits a `write` tool call must mutate the temp workspace via the
/// controller-installed tool and leave main session JSONL/messages/todo/goal/
/// orchestration untouched.
#[tokio::test]
async fn edit_mode_excludes_main_control_tools_and_write_stays_in_workspace() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-edit-isolation");
    let relative = "side-edit-only.txt";
    let marker = "side-chat workspace write marker";
    let written_path = cwd.path().join(relative);

    let write_call = ContentBlock::ToolCall(ToolCall {
        id: "side-write-1".to_owned(),
        name: "write".to_owned(),
        arguments: json!({
            "path": relative,
            "content": marker,
        }),
        thought_signature: None,
    });
    let stream = scripted_reply_stream(vec![
        scripted_assistant(vec![write_call], StopReason::ToolUse),
        scripted_assistant(
            vec![ContentBlock::text("wrote workspace file")],
            StopReason::Stop,
        ),
    ]);

    let (application, session_path) = recorded_application(
        cwd.path(),
        session_dir.path(),
        &session_id,
        vec![Message::user_text("edit isolation root", 1)],
        Some(stream),
    )
    .await;

    // Seed main-owned control plane state the side agent must not share.
    application
        .set_todos(vec![TodoPhase {
            name: "Main".to_owned(),
            tasks: vec![TodoItem {
                id: "main-task".to_owned(),
                content: "main todo must survive side write".to_owned(),
                status: TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                blocked_by: Vec::new(),
            }],
        }])
        .expect("seed main todos");
    let goal = application
        .goal_create("main goal must survive side write", Some(1_000))
        .expect("seed main goal");
    let before_main = capture_main(&application).await;
    let before_todos = application.todo_state();
    let before_goal = application.goal_state();
    let before_orchestration = application.orchestration_runtime().is_some();
    let before_file = fs::read_to_string(&session_path).expect("session file before write");
    assert_eq!(
        before_goal.current.as_ref().map(|g| g.id.as_str()),
        Some(goal.id.as_str())
    );
    assert!(
        !written_path.exists(),
        "precondition: workspace file must not exist yet"
    );

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    side.toggle_tool_mode().await.expect("enter edit mode");
    assert!(side.tool_mode().is_edit());

    let names = side.tool_names().await;
    let forbidden = ["todo", "goal", "task", "hub", "process", "extension"];
    for name in forbidden {
        assert!(
            !names.iter().any(|tool| tool == name),
            "edit mode must not expose main-runtime control tool {name:?}; got {names:?}"
        );
    }
    // Fresh all-builtin workspace tools + peek_main only (no Session/extension closures).
    let expected_workspace: Vec<String> = create_all_tools(&cwd.path().to_string_lossy())
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        {
            let mut names = expected_workspace.clone();
            names.sort();
            names
        },
        {
            let mut expected = vec![
                "bash".to_owned(),
                "edit".to_owned(),
                "find".to_owned(),
                "glob".to_owned(),
                "grep".to_owned(),
                "ls".to_owned(),
                "read".to_owned(),
                "write".to_owned(),
            ];
            expected.sort();
            expected
        },
        "create_all_tools contract drifted"
    );
    let mut expected = expected_workspace;
    expected.push("peek_main".to_owned());
    expected.sort();
    let mut actual = names.clone();
    actual.sort();
    assert_eq!(
        actual, expected,
        "edit mode tool set must be create_all_tools + peek_main only"
    );
    let caps = side.tool_capabilities().await;
    assert!(
        caps.iter()
            .any(|(name, cap)| name == "write" && *cap == ToolCapability::Write),
        "write must keep Write capability: {caps:?}"
    );

    // Drive the controller-installed write tool via a real side prompt/turn.
    side.submit_prompt("please write the workspace marker file");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                !side.is_streaming() && written_path.is_file()
            },
            Duration::from_secs(5)
        )
        .await,
        "side write turn did not complete; streaming={} file_exists={} entries={:?} status={}",
        side.is_streaming(),
        written_path.exists(),
        side.entries(),
        side.status()
    );
    assert_eq!(
        fs::read_to_string(&written_path).expect("read workspace file"),
        marker,
        "controller-installed write must create the workspace marker"
    );
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Tool
                && (entry.text.contains("write")
                    || entry.text.contains(relative)
                    || entry.text.contains("Successfully wrote"))
        }),
        "side transcript should record the write tool execution: {:?}",
        side.entries()
    );

    // Main control-plane / session identity must be byte-stable.
    assert_main_unchanged(
        &before_main,
        &capture_main(&application).await,
        "after side edit write turn",
    );
    assert_eq!(
        application.todo_state(),
        before_todos,
        "side write must not mutate main todo state"
    );
    assert_eq!(
        application.goal_state(),
        before_goal,
        "side write must not mutate main goal state"
    );
    assert_eq!(
        application.orchestration_runtime().is_some(),
        before_orchestration,
        "side write must not attach or alter main orchestration"
    );
    let after_file = fs::read_to_string(&session_path).expect("session file after write");
    assert_eq!(
        after_file, before_file,
        "side write must not mutate main session JSONL"
    );
    assert!(
        !after_file.contains(marker),
        "workspace write marker leaked into main session JSONL"
    );
    assert!(
        !after_file.contains(relative),
        "workspace path leaked into main session JSONL"
    );
    assert!(
        application
            .messages()
            .iter()
            .all(|message| message_text(message) != marker),
        "workspace marker must not enter main messages"
    );

    side.shutdown().await;
}

/// Capture of request-time stream options observed by an injected provider stream.
#[derive(Clone, Debug, Default)]
struct CapturedStreamAuth {
    api_key: Option<String>,
    session_id: Option<String>,
    headers: HashMap<String, String>,
    env: HashMap<String, String>,
    hooks_present: bool,
}

fn header_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
}

fn tool_rows(side: &SideChatController) -> Vec<&pi_cli::side_chat::SideChatEntry> {
    side.entries()
        .iter()
        .filter(|entry| entry.role == SideChatRole::Tool)
        .collect()
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

/// Public controller path: fork must scrub main provider hooks, mint a distinct
/// provider session id, and refresh request-time auth (key/headers/env) into the
/// side stream without reusing stale main credentials.
#[tokio::test]
async fn side_fork_refreshes_auth_and_keeps_independent_provider_session() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-auth");
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(Mutex::new(None::<CapturedStreamAuth>));

    let mut stream_options = SimpleStreamOptions::default();
    stream_options.stream.transport = Transport::WebSocket;
    stream_options.stream.timeout_ms = Some(12_345);
    stream_options.stream.api_key = Some("stale-key".to_owned());
    stream_options.stream.headers = HashMap::from([
        ("X-Static".to_owned(), "static".to_owned()),
        ("X-Refresh".to_owned(), "stale".to_owned()),
    ]);
    stream_options
        .stream
        .env
        .insert("STATIC_ENV".to_owned(), "static".to_owned());
    let payload_calls = hook_calls.clone();
    stream_options.stream.on_payload = Some(Arc::new(move |payload, _| {
        payload_calls.fetch_add(1, Ordering::SeqCst);
        Ok(payload)
    }));
    let response_calls = hook_calls.clone();
    stream_options.stream.on_response = Some(Arc::new(move |_, _| {
        response_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let request_calls = hook_calls.clone();
    stream_options.stream.before_provider_request = Some(Arc::new(move |payload, _| {
        request_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(payload) })
    }));
    let header_calls = hook_calls.clone();
    stream_options.stream.before_provider_headers = Some(Arc::new(move |headers, _| {
        header_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(headers) })
    }));
    let after_calls = hook_calls.clone();
    stream_options.stream.after_provider_response = Some(Arc::new(move |_, _| {
        after_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(()) })
    }));

    let captured_options = captured.clone();
    let stream_fn: StreamFn = Arc::new(move |model, _context, options| {
        *captured_options.lock().expect("capture options") = Some(CapturedStreamAuth {
            api_key: options.stream.api_key.clone(),
            session_id: options.stream.session_id.clone(),
            headers: options.stream.headers.clone(),
            env: options.stream.env.clone(),
            hooks_present: options.stream.on_payload.is_some()
                || options.stream.on_response.is_some()
                || options.stream.before_provider_request.is_some()
                || options.stream.before_provider_headers.is_some()
                || options.stream.after_provider_response.is_some(),
        });
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("auth refresh ok"));
                message.stop_reason = StopReason::Stop;
                message.api = model.api.clone();
                message.provider = model.provider.clone();
                message.model = model.id.clone();
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

    let refresh_calls = resolver_calls.clone();
    let resolver: SessionAuthResolver = Arc::new(move |_model| {
        refresh_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(RequestAuth {
                api_key: "refreshed-key".to_owned(),
                headers: HashMap::from([
                    ("x-refresh".to_owned(), "fresh".to_owned()),
                    ("X-Auth".to_owned(), "auth".to_owned()),
                ]),
                env: HashMap::from([("REFRESHED_ENV".to_owned(), "fresh".to_owned())]),
                available_model_ids: None,
            })
        })
    });

    let model = test_model("auth-side");
    let session = Session::new(SessionOptions {
        model: model.clone(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: "main system prompt".to_owned(),
        thinking_level: ThinkingLevel::Off,
        api_key: "stale-key".into(),
        compaction: None,
        stream_options,
        tools: Some(create_coding_tools(&cwd.path().to_string_lossy())),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: Some(resolver),
    })
    .expect("session");
    let main_stream = session.stream_options();
    let main_provider_session = main_stream
        .stream
        .session_id
        .clone()
        .expect("main provider session id");
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        Some(&model),
        Some("off"),
        Some(session_dir.path()),
        Some(&session_id),
        None,
    )
    .expect("start recorder");
    let session_path = recorder.path();
    session.record(recorder).expect("attach recorder");
    session
        .load_history(vec![Message::user_text("auth root", 1)])
        .await
        .expect("load history");
    let application = Application::new(session).await;
    let before = capture_main(&application).await;
    let before_file = fs::read_to_string(&session_path).expect("session file before");

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    side.submit_prompt("refresh auth on side stream");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                !side.is_streaming() && captured.lock().expect("capture lock").is_some()
            },
            Duration::from_secs(5)
        )
        .await,
        "side auth turn did not complete; streaming={} captured={:?} status={} entries={:?}",
        side.is_streaming(),
        *captured.lock().expect("capture lock"),
        side.status(),
        side.entries()
    );

    let observed = captured
        .lock()
        .expect("capture lock")
        .clone()
        .expect("side stream options");
    assert_eq!(
        resolver_calls.load(Ordering::SeqCst),
        1,
        "request-time auth resolver must run once for the side prompt"
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        0,
        "main provider hooks must be scrubbed from the side stream"
    );
    assert!(
        !observed.hooks_present,
        "side stream options must not carry main provider hooks: {observed:?}"
    );
    assert_eq!(observed.api_key.as_deref(), Some("refreshed-key"));
    assert_ne!(
        observed.api_key.as_deref(),
        Some("stale-key"),
        "stale main api key must not reach the side stream"
    );
    assert_eq!(header_value(&observed.headers, "X-Static"), Some("static"));
    assert_eq!(header_value(&observed.headers, "x-refresh"), Some("fresh"));
    assert_eq!(header_value(&observed.headers, "X-Auth"), Some("auth"));
    assert!(
        header_value(&observed.headers, "X-Refresh") != Some("stale"),
        "stale refresh header must be replaced: {:?}",
        observed.headers
    );
    assert_eq!(observed.env.get("STATIC_ENV").map(String::as_str), Some("static"));
    assert_eq!(
        observed.env.get("REFRESHED_ENV").map(String::as_str),
        Some("fresh")
    );
    let side_provider_session = observed
        .session_id
        .clone()
        .expect("side provider session id");
    assert_ne!(
        side_provider_session, main_provider_session,
        "side provider session id must differ from main"
    );
    assert_ne!(
        side_provider_session.as_str(),
        session_id.as_str(),
        "side provider session id must not reuse the main recorder id"
    );
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Assistant && entry.text.contains("auth refresh ok")
        }),
        "side transcript should record the refreshed-auth reply: {:?}",
        side.entries()
    );

    assert_main_unchanged(
        &before,
        &capture_main(&application).await,
        "after side auth refresh turn",
    );
    let after_file = fs::read_to_string(&session_path).expect("session file after");
    assert_eq!(
        after_file, before_file,
        "side auth turn must not mutate main session JSONL"
    );
    for needle in ["refresh auth on side stream", "auth refresh ok", "refreshed-key"] {
        assert!(
            !after_file.contains(needle),
            "main session file unexpectedly contains side artifact {needle:?}: {after_file}"
        );
    }

    side.shutdown().await;
}

/// Public controller stream path: a turn with two tool calls plus trailing
/// ToolResult echoes must finalize exactly one row per tool_call_id even when
/// completions arrive out of order. Causal B-before-A is test-controlled: A
/// blocks on a FIFO it creates; B prints and exits without releasing A; the
/// wait predicate opens the FIFO only after observing finalized B.
#[tokio::test]
async fn side_streamed_parallel_tool_calls_dedup_by_tool_call_id() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-tools");
    let marker_a = "result-a-unique-marker";
    let marker_b = "result-b-unique-marker";
    // Relative paths under the side workspace cwd (tools run there).
    let gate = "side-tool-gate.fifo";
    let ready = "side-tool-gate.ready";
    let gate_abs = cwd.path().join(gate);

    // call-a: create FIFO + ready marker, block reading FIFO, then print A.
    // Release is performed by the test after B is observed — not by B and not
    // by wall-clock sleep.
    let call_a = ContentBlock::ToolCall(ToolCall {
        id: "call-a".to_owned(),
        name: "bash".to_owned(),
        arguments: json!({
            "command": format!(
                "rm -f '{gate}' '{ready}'; mkfifo '{gate}'; : > '{ready}'; cat '{gate}' >/dev/null; printf '%s' '{marker_a}'"
            ),
            "timeout": 5,
        }),
        thought_signature: None,
    });
    // call-b: bounded-wait for ready/FIFO, print B, exit WITHOUT releasing A.
    let call_b = ContentBlock::ToolCall(ToolCall {
        id: "call-b".to_owned(),
        name: "bash".to_owned(),
        arguments: json!({
            "command": format!(
                "for i in $(seq 1 50); do [ -f '{ready}' ] && [ -p '{gate}' ] && break; sleep 0.05; done; \
[ -f '{ready}' ] && [ -p '{gate}' ] || {{ echo 'gate missing' >&2; exit 1; }}; \
printf '%s' '{marker_b}'"
            ),
            "timeout": 5,
        }),
        thought_signature: None,
    });
    let stream = scripted_reply_stream(vec![
        scripted_assistant(vec![call_a, call_b], StopReason::ToolUse),
        scripted_assistant(
            vec![ContentBlock::text("both tools complete")],
            StopReason::Stop,
        ),
    ]);

    let (application, session_path) = recorded_application(
        cwd.path(),
        session_dir.path(),
        &session_id,
        vec![Message::user_text("dual tool root", 1)],
        Some(stream),
    )
    .await;
    let before = capture_main(&application).await;
    let before_file = fs::read_to_string(&session_path).expect("session file before");

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    // Fresh side-scoped workspace tools (includes bash); never main Session tools.
    side.toggle_tool_mode().await.expect("edit mode for bash tools");
    assert!(
        side.tool_names()
            .await
            .iter()
            .any(|name| name == "bash"),
        "edit mode must expose side-scoped bash"
    );

    side.submit_prompt("run both gated tools");
    let mut saw_b_before_a = false;
    let mut released_a = false;
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                let rows = tool_rows(&side);
                let b_done = rows
                    .iter()
                    .any(|row| !row.is_partial && row.text.contains(marker_b));
                let a_done = rows
                    .iter()
                    .any(|row| !row.is_partial && row.text.contains(marker_a));
                // Test-controlled causal release: only after finalized B is
                // observed (and A is still pending) open/write the FIFO once.
                // Non-blocking open retries until A is blocked on cat — no
                // wall-clock sleep ordering and no helper-thread leak.
                if b_done && !a_done {
                    saw_b_before_a = true;
                    if !released_a {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        // Linux O_NONBLOCK: fail fast if the reader is not yet
                        // attached instead of blocking the test reactor.
                        match std::fs::OpenOptions::new()
                            .write(true)
                            .custom_flags(0o4000)
                            .open(&gate_abs)
                        {
                            Ok(mut file) => {
                                let _ = file.write_all(b"x");
                                released_a = true;
                            }
                            Err(_) => {
                                // A has not entered cat yet; retry next poll.
                            }
                        }
                    }
                }
                !side.is_streaming()
                    && a_done
                    && b_done
                    && side.entries().iter().any(|entry| {
                        entry.role == SideChatRole::Assistant
                            && entry.text.contains("both tools complete")
                    })
            },
            Duration::from_secs(8)
        )
        .await,
        "dual tool turn did not complete; streaming={} status={} entries={:?} b_before_a={saw_b_before_a} released={released_a}",
        side.is_streaming(),
        side.status(),
        side.entries()
    );
    assert!(
        saw_b_before_a,
        "expected call-b to finalize before call-a (test-controlled FIFO release); entries={:?}",
        side.entries()
    );
    assert!(
        released_a,
        "test must have released call-a after observing finalized B"
    );

    let rows = tool_rows(&side);
    assert_eq!(
        rows.len(),
        2,
        "exactly one final tool row per tool_call_id; got {rows:?}"
    );
    for row in &rows {
        assert!(!row.is_partial, "tool row must be finalized: {row:?}");
        assert!(!row.is_error, "tool row must not be error: {row:?}");
        assert!(
            row.text.contains("[bash]"),
            "tool row must keep bash identity: {row:?}"
        );
    }
    let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
    let a_matches = texts.iter().filter(|text| text.contains(marker_a)).count();
    let b_matches = texts.iter().filter(|text| text.contains(marker_b)).count();
    assert_eq!(
        a_matches, 1,
        "call-a result must appear exactly once: {texts:?}"
    );
    assert_eq!(
        b_matches, 1,
        "call-b result must appear exactly once: {texts:?}"
    );
    // No duplicate rows from trailing ToolResult MessageEnd echoes.
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains(marker_a) || text.contains(marker_b))
            .count(),
        2,
        "ToolResult echoes must not create extra rows: {texts:?}"
    );
    // Association: each marker lives on its own single bash row (not swapped/merged).
    assert!(
        rows.iter().any(|row| row.text.contains(marker_a) && !row.text.contains(marker_b)),
        "call-a marker must own a dedicated row: {texts:?}"
    );
    assert!(
        rows.iter().any(|row| row.text.contains(marker_b) && !row.text.contains(marker_a)),
        "call-b marker must own a dedicated row: {texts:?}"
    );
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Assistant && entry.text.contains("both tools complete")
        }),
        "final assistant text missing: {:?}",
        side.entries()
    );

    assert_main_unchanged(
        &before,
        &capture_main(&application).await,
        "after dual tool side turn",
    );
    let after_file = fs::read_to_string(&session_path).expect("session file after");
    assert_eq!(
        after_file, before_file,
        "dual tool side turn must not mutate main session JSONL"
    );
    for needle in [
        marker_a,
        marker_b,
        "run both gated tools",
        "both tools complete",
    ] {
        assert!(
            !after_file.contains(needle),
            "main session file unexpectedly contains side artifact {needle:?}: {after_file}"
        );
    }

    side.shutdown().await;
}

/// Model/tool payloads carrying CSI SGR/cursor and OSC8 BEL/ST must flow through
/// the public controller prompt/stream path and render via the side panel without
/// leaking ESC or unsafe control cells while preserving visible text. The tool
/// payload is returned by a real read-only `read` of a prewritten file (no edit
/// mode), so tool-result sanitization is exercised end-to-end.
#[tokio::test]
async fn side_chat_render_strips_csi_osc_from_streamed_payloads() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session_id = unique("btw-ansi");
    let relative = "ansi-side.txt";
    let visible_tool = "tool-visible-label";
    let tool_payload = format!(
        "pre\u{1b}[31m{visible_tool}\u{1b}[0m\u{1b}[2;5H\u{1b}]8;;https://tool.example\u{7}link\u{1b}]8;;\u{7}\u{1b}]8;;https://tool.example\u{1b}\\st\u{1b}]8;;\u{1b}\\"
    );
    fs::write(cwd.path().join(relative), &tool_payload).expect("write ansi fixture");
    let assistant_payload = "see \u{1b}[31mcolor\u{1b}[0m \u{1b}[1;1Hhere \u{1b}]8;;https://assist.example\u{7}label\u{1b}]8;;\u{7} \u{1b}]8;;https://assist.example\u{1b}\\st\u{1b}]8;;\u{1b}\\ end";

    let read_call = ContentBlock::ToolCall(ToolCall {
        id: "ansi-read-1".to_owned(),
        name: "read".to_owned(),
        arguments: json!({ "path": relative }),
        thought_signature: None,
    });
    let stream = scripted_reply_stream(vec![
        scripted_assistant(vec![read_call], StopReason::ToolUse),
        scripted_assistant(
            vec![ContentBlock::text(assistant_payload)],
            StopReason::Stop,
        ),
    ]);

    let (application, session_path) = recorded_application(
        cwd.path(),
        session_dir.path(),
        &session_id,
        vec![Message::user_text("ansi root", 1)],
        Some(stream),
    )
    .await;
    let before = capture_main(&application).await;
    let before_file = fs::read_to_string(&session_path).expect("session file before");

    let mut side = SideChatController::fork_from(&application)
        .await
        .expect("fork");
    // Default read-only tools already include `read` — no edit toggle.
    assert!(!side.tool_mode().is_edit());
    side.submit_prompt("stream ansi payload through side chat");
    assert!(
        wait_until(
            || {
                let _ = side.poll_events();
                !side.is_streaming()
                    && side.entries().iter().any(|entry| {
                        entry.role == SideChatRole::Tool
                            && (entry.text.contains(visible_tool) || entry.text.contains('\u{1b}'))
                    })
                    && side.entries().iter().any(|entry| {
                        entry.role == SideChatRole::Assistant
                            && (entry.text.contains("color") || entry.text.contains("label"))
                    })
            },
            Duration::from_secs(5)
        )
        .await,
        "ansi stream turn did not complete; streaming={} status={} entries={:?}",
        side.is_streaming(),
        side.status(),
        side.entries()
    );

    // Controller retains raw model/tool text; the panel sanitizes on render.
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Tool
                && entry.text.contains(visible_tool)
                && entry.text.contains('\u{1b}')
        }),
        "tool payload with CSI/OSC must reach controller entries: {:?}",
        side.entries()
    );
    assert!(
        side.entries().iter().any(|entry| {
            entry.role == SideChatRole::Assistant
                && (entry.text.contains('\u{1b}') || entry.text.contains("color"))
        }),
        "assistant payload should reach controller entries: {:?}",
        side.entries()
    );

    // Editor also carries OSC8 so the render path covers transcript + editor.
    side.handle_paste(
        "edit \u{1b}]8;;https://edit.example\u{7}editlabel\u{1b}]8;;\u{7} \u{1b}]8;;https://edit.example\u{1b}\\editst\u{1b}]8;;\u{1b}\\",
    );

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_side_chat_panel(frame, &side, theme::DARK))
        .expect("draw side panel");
    let text = buffer_text(&terminal);

    for visible in [
        visible_tool,
        "pre",
        "link",
        "st",
        "color",
        "here",
        "label",
        "end",
        "editlabel",
        "editst",
        "stream ansi payload",
    ] {
        assert!(
            text.contains(visible),
            "visible payload {visible:?} missing from rendered panel: {text:?}"
        );
    }
    assert!(
        text.contains("read") || text.contains(relative),
        "tool execution should render visible read chrome: {text:?}"
    );
    assert_no_unsafe_controls(&text);
    for leak in [
        "https://assist.example",
        "https://edit.example",
        "https://tool.example",
        "[31m",
        "[0m",
        "[2;5H",
        "[1;1H",
    ] {
        assert!(
            !text.contains(leak),
            "control/hyperlink payload leaked into panel buffer ({leak}): {text:?}"
        );
    }

    assert_main_unchanged(
        &before,
        &capture_main(&application).await,
        "after ansi side stream + render",
    );
    let after_file = fs::read_to_string(&session_path).expect("session file after");
    assert_eq!(
        after_file, before_file,
        "ansi side turn must not mutate main session JSONL"
    );
    assert!(
        !after_file.contains("stream ansi payload through side chat"),
        "side prompt leaked into main session JSONL"
    );
    assert!(
        !after_file.contains(visible_tool),
        "tool CSI payload leaked into main session JSONL"
    );

    side.shutdown().await;
}
