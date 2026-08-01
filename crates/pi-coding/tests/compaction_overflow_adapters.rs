//! Deterministic faux coverage for context-overflow compaction adapters.
//!
//! Contracts defended:
//! - overflow triggers exactly one compact + one retry (no duplicate user turn)
//! - the post-compact provider call retains a coherent recent tail
//! - Application lifecycle projects compact start/end without internal reminders
//! - active goal + todo state survive overflow recovery and manual compact
//! - a second overflow fails with an actionable error
//! - manual compact leaves a usable prompt path

use std::sync::{
    Arc, LazyLock, Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering},
};

use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason,
    new_assistant_message_event_stream,
};
use pi_coding::{
    Application, ApplicationEvent, CompactionReason, CompactionSettings, GoalLifecycle,
    ResourceDiscovery, Session, SessionEvent, SessionOptions, TodoInitPhase, TodoOp, ToolSelection,
};

static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn registry_guard() -> MutexGuard<'static, ()> {
    REGISTRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Canonical overflow phrasing that matches CONTEXT_OVERFLOW_PATTERNS.
const OVERFLOW_ERROR: &str = "input exceeds the context window";

fn unique_model(prefix: &str) -> Model {
    let suffix = uuid::Uuid::now_v7();
    Model {
        id: format!("{prefix}-model-{suffix}"),
        name: format!("{prefix} Model"),
        api: format!("{prefix}-api-{suffix}"),
        provider: format!("{prefix}-provider-{suffix}"),
        base_url: "http://localhost:0".into(),
        context_window: 100,
        max_tokens: 32,
        ..Model::default()
    }
}

fn compact_settings() -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 4,
    }
}

/// History large enough to cut, with an explicit recent tail marker retained by
/// `keep_recent_tokens` when the cut walks backward from the end.
fn overflow_history_with_tail(model: &Model) -> Vec<Message> {
    let mut older_answer = AssistantMessage::pending(model);
    older_answer.content = vec![ContentBlock::text("older answer body ".repeat(20))];
    older_answer.stop_reason = StopReason::Stop;
    older_answer.timestamp = 2;
    vec![
        Message::user_text("older context that should be summarized ".repeat(30), 1),
        Message::Assistant(older_answer),
        Message::user_text("recent retained tail marker", 3),
    ]
}

/// Minimal overflow seed used by the baseline session_compaction suite.
fn overflow_history_minimal() -> Vec<Message> {
    vec![Message::user_text("older context ".repeat(30), 1)]
}

fn session_with_responses(
    model: Model,
    responses: Vec<FauxResponse>,
    compaction: Option<CompactionSettings>,
) -> (Session, FauxProviderRegistration, tempfile::TempDir) {
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(responses);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    // Disable resource discovery so deterministic skill selection cannot inject
    // selection_recommendations into provider contexts under test.
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("build session");
    (session, registration, cwd)
}

fn session_with_context_capture(
    model: Model,
    responses: Vec<FauxResponse>,
    contexts: Arc<Mutex<Vec<Context>>>,
    compaction: Option<CompactionSettings>,
) -> (Session, FauxProviderRegistration, tempfile::TempDir) {
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(responses);
    let stream_fn: StreamFn = Arc::new(move |model, context, options| {
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("build context-capturing session");
    (session, registration, cwd)
}

fn context_text_blobs(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => message.content.iter().collect::<Vec<_>>(),
            Message::Assistant(message) => message.content.iter().collect::<Vec<_>>(),
            Message::ToolResult(message) => message.content.iter().collect::<Vec<_>>(),
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                panic!("provider context contains unprojected session message: {message:?}")
            }
        })
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn drain_session_events(rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn drain_application_events(
    rx: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
) -> Vec<ApplicationEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

async fn collect_until_session_end(
    rx: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
    reason: CompactionReason,
) -> Vec<ApplicationEvent> {
    let mut lifecycle = drain_application_events(rx);
    for _ in 0..64 {
        if lifecycle.iter().any(|event| {
            matches!(
                event,
                ApplicationEvent::Session(SessionEvent::CompactionEnd { reason: r, .. }) if *r == reason
            )
        }) {
            break;
        }
        if let Ok(event) = rx.try_recv() {
            lifecycle.push(event);
        } else {
            tokio::task::yield_now().await;
        }
    }
    lifecycle
}

fn serialized_event_blob(events: &[ApplicationEvent]) -> String {
    events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize application event"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn count_user_turns(history: &[Message], text: &str) -> usize {
    history
        .iter()
        .filter(|message| {
            matches!(
                message,
                Message::User(user)
                    if user.content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text: body, .. } if body == text
                    ))
            )
        })
        .count()
}

fn last_assistant_text(history: &[Message]) -> Option<String> {
    history.iter().rev().find_map(|message| match message {
        Message::Assistant(assistant) => {
            let text = assistant.text();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn history_has_compaction_summary(history: &[Message]) -> bool {
    matches!(history.first(), Some(Message::CompactionSummary(_)))
}

/// Overflow recovery must compact exactly once, retry the same user turn once,
/// and leave a single user message for that prompt.
#[tokio::test]
async fn overflow_triggers_exactly_one_compact_and_retry_without_duplicate_user_turn() {
    let _guard = registry_guard();
    let model = unique_model("overflow-once");
    let (session, registration, _cwd) = session_with_responses(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("overflow checkpoint summary"),
            FauxResponse::text("recovered answer after one compact"),
        ],
        Some(compact_settings()),
    );
    session
        .load_history(overflow_history_minimal())
        .await
        .expect("load overflow history");
    let mut events = session.subscribe_session_events();

    let result = session
        .run("single overflow turn", Vec::new())
        .await
        .expect("overflow recovery succeeds once");

    // After overflow compact, finish_run may slice new messages against the
    // pre-run count and leave RunResult.text empty. The durable contract is the
    // transcript / last assistant text.
    assert_eq!(
        last_assistant_text(&session.history()).as_deref(),
        Some("recovered answer after one compact"),
        "history must end with the recovered answer; run_result.text={:?} history={:?}",
        result.text,
        session.history()
    );
    assert_eq!(
        count_user_turns(&session.history(), "single overflow turn"),
        1,
        "overflow recovery must not duplicate the user turn: {:?}",
        session.history()
    );
    assert!(
        history_has_compaction_summary(&session.history()),
        "history must start with the compaction checkpoint: {:?}",
        session.history()
    );

    let lifecycle = drain_session_events(&mut events);
    let starts = lifecycle
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::CompactionStart {
                    reason: CompactionReason::Overflow
                }
            )
        })
        .count();
    let ends = lifecycle
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    aborted: false,
                    will_retry: true,
                    result: Some(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(starts, 1, "expected exactly one overflow CompactionStart: {lifecycle:?}");
    assert_eq!(ends, 1, "expected exactly one overflow CompactionEnd(will_retry): {lifecycle:?}");
    assert!(!session.is_compacting(), "activity flag must clear after recovery");

    registration.unregister();
}

/// After overflow compact, the next provider call must keep the recent retained
/// tail (and the user turn) while projecting the checkpoint summary as user text.
#[tokio::test]
async fn overflow_retry_provider_context_retains_coherent_tail() {
    let _guard = registry_guard();
    let model = unique_model("overflow-tail");
    let history = overflow_history_with_tail(&model);
    let contexts = Arc::new(Mutex::new(Vec::<Context>::new()));
    // keep_recent_tokens large enough to retain the short tail marker after cut.
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 20,
    };
    let (session, registration, _cwd) = session_with_context_capture(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("tail checkpoint"),
            FauxResponse::text("answer with retained tail"),
        ],
        contexts.clone(),
        Some(settings),
    );
    session
        .load_history(history)
        .await
        .expect("load history with retained marker");

    session
        .run("continue with retained tail", Vec::new())
        .await
        .expect("overflow compact + retry");
    assert_eq!(
        last_assistant_text(&session.history()).as_deref(),
        Some("answer with retained tail")
    );
    assert!(
        session.history().iter().any(|message| matches!(
            message,
            Message::User(user)
                if user.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. } if text == "recent retained tail marker"
                ))
        )),
        "session history must retain the recent tail marker: {:?}",
        session.history()
    );

    let captured = contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // Call 0: overflow attempt. Call 1: summarization. Call 2: post-compact retry.
    assert!(
        captured.len() >= 3,
        "expected overflow + summary + retry provider calls, got {}",
        captured.len()
    );

    let retry_blobs = context_text_blobs(&captured[2]);
    let retry_text = retry_blobs.join("\n");
    assert!(
        retry_blobs.iter().any(|text| text.contains("tail checkpoint")),
        "retry context must project the compaction summary: {retry_text}"
    );
    assert!(
        retry_blobs
            .iter()
            .any(|text| text.contains("recent retained tail marker")),
        "retry context must keep the recent retained tail marker: {retry_text}"
    );
    assert!(
        retry_blobs
            .iter()
            .any(|text| text.contains("continue with retained tail")),
        "retry context must include the original user turn: {retry_text}"
    );
    assert!(
        !retry_text.contains("older context that should be summarized"),
        "retry context must not keep the pre-cut bulk history: {retry_text}"
    );
    assert!(
        !retry_text.contains("todo-error-reminder"),
        "retry context must not expose internal todo reminders: {retry_text}"
    );
    assert!(
        !retry_text.contains("selection_recommendations"),
        "retry context must not inject selection recommendations with discovery disabled: {retry_text}"
    );

    registration.unregister();
}

/// Application must project overflow compact lifecycle for TUI consumers, clear
/// is_compacting when idle, and never leak internal reminder custom types into
/// the public event stream / transcript.
#[tokio::test]
async fn application_projects_overflow_compact_lifecycle_without_internal_reminders() {
    let _guard = registry_guard();
    let model = unique_model("app-overflow-lifecycle");
    let (session, registration, _cwd) = session_with_responses(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("application overflow checkpoint"),
            FauxResponse::text("application recovered"),
        ],
        Some(compact_settings()),
    );
    session
        .load_history(overflow_history_minimal())
        .await
        .expect("load history");
    let application = Application::new(session).await;
    let mut events = application.subscribe();

    application
        .prompt("app overflow turn".into(), Vec::new(), None)
        .await
        .expect("start prompt");
    application.wait_for_idle().await;

    assert_eq!(
        application.last_assistant_text().as_deref(),
        Some("application recovered")
    );
    let state = application.state().await;
    assert!(!state.is_streaming, "must be idle after recovery");
    assert!(!state.is_compacting, "must not remain compacting after recovery");
    assert!(
        state.auto_compaction_enabled,
        "overflow session keeps auto-compaction enabled"
    );

    let lifecycle = collect_until_session_end(&mut events, CompactionReason::Overflow).await;
    let starts = lifecycle
        .iter()
        .filter(|event| {
            matches!(
                event,
                ApplicationEvent::Session(SessionEvent::CompactionStart {
                    reason: CompactionReason::Overflow
                })
            )
        })
        .count();
    let ends = lifecycle
        .iter()
        .filter(|event| {
            matches!(
                event,
                ApplicationEvent::Session(SessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    aborted: false,
                    will_retry: true,
                    result: Some(_),
                    ..
                })
            )
        })
        .count();
    assert_eq!(starts, 1, "TUI projection missing CompactionStart: {lifecycle:?}");
    assert_eq!(ends, 1, "TUI projection missing CompactionEnd: {lifecycle:?}");

    let blob = serialized_event_blob(&lifecycle);
    for forbidden in [
        "todo-error-reminder",
        "SUMMARIZATION_PROMPT",
        "SUMMARIZATION_SYSTEM_PROMPT",
        "A previous todo operation failed",
    ] {
        assert!(
            !blob.contains(forbidden),
            "lifecycle events must not expose internal reminder {forbidden}: {blob}"
        );
    }

    let transcript = serde_json::to_string(&application.messages()).expect("transcript");
    assert!(
        !transcript.contains("todo-error-reminder"),
        "transcript must not store internal todo reminders: {transcript}"
    );
    assert!(
        history_has_compaction_summary(&application.messages()),
        "application transcript must begin with CompactionSummary"
    );

    application.cleanup().await;
    registration.unregister();
}

/// Active goal + todo DAG must survive overflow recovery unchanged.
#[tokio::test]
async fn overflow_recovery_preserves_active_goal_and_todo_state() {
    let _guard = registry_guard();
    let model = unique_model("overflow-goal-todo");
    let (session, registration, _cwd) = session_with_responses(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("goal-safe checkpoint"),
            FauxResponse::text("goal-safe answer"),
        ],
        Some(compact_settings()),
    );
    session
        .load_history(overflow_history_minimal())
        .await
        .expect("load history");
    let application = Application::new(session).await;

    let goal = application
        .goal_create("keep shipping through compaction", Some(250))
        .expect("create goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    application
        .apply_todo(TodoOp::Init {
            list: Some(vec![TodoInitPhase {
                phase: "Recovery".into(),
                items: vec!["preserve dag".into(), "finish turn".into()],
            }]),
            items: None,
            phase: None,
        })
        .expect("init todos");
    let todos_before = application.todo_state();
    assert_eq!(todos_before.phases.len(), 1);
    assert_eq!(todos_before.phases[0].name, "Recovery");
    assert_eq!(todos_before.phases[0].tasks.len(), 2);
    let goal_before = application.goal_state();

    application
        .prompt("recover while goal and todos stay live".into(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    assert_eq!(
        application.last_assistant_text().as_deref(),
        Some("goal-safe answer")
    );
    let goal_after = application.goal_state();
    assert_eq!(
        goal_after.current.as_ref().map(|goal| goal.objective.as_str()),
        goal_before
            .current
            .as_ref()
            .map(|goal| goal.objective.as_str())
    );
    assert_eq!(
        goal_after.current.as_ref().map(|goal| goal.lifecycle),
        Some(GoalLifecycle::Active)
    );
    assert_eq!(
        goal_after.revision,
        goal_before.revision,
        "overflow recovery must not rewrite goal revision"
    );

    let todos_after = application.todo_state();
    assert_eq!(todos_after.phases, todos_before.phases);
    let state = application.state().await;
    assert_eq!(state.todo_phases, todos_before.phases);
    assert_eq!(
        state.goal.current.as_ref().map(|goal| goal.objective.as_str()),
        Some("keep shipping through compaction")
    );

    application.cleanup().await;
    registration.unregister();
}

/// A second overflow after the single recovery attempt must fail with an
/// actionable operator-facing error (not a silent hang or opaque provider text).
#[tokio::test]
async fn second_overflow_fails_with_actionable_error_and_clears_compacting() {
    let _guard = registry_guard();
    let model = unique_model("overflow-second");
    let (session, registration, _cwd) = session_with_responses(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("first overflow checkpoint"),
            FauxResponse::error(OVERFLOW_ERROR),
        ],
        Some(compact_settings()),
    );
    session
        .load_history(overflow_history_minimal())
        .await
        .expect("load history");
    let mut events = session.subscribe_session_events();

    let error = session
        .run("second overflow turn", Vec::new())
        .await
        .expect_err("second overflow must stop");
    let message = error.to_string();
    assert!(
        message.contains("Context overflow persisted after automatic compaction"),
        "error must name the persistent overflow condition: {message}"
    );
    assert!(
        message.contains("Reduce the prompt or start a new session"),
        "error must give an actionable next step: {message}"
    );
    assert!(!session.is_compacting(), "failed recovery must clear compacting");

    let lifecycle = drain_session_events(&mut events);
    assert!(
        lifecycle.iter().any(|event| matches!(
            event,
            SessionEvent::CompactionEnd {
                reason: CompactionReason::Overflow,
                aborted: false,
                will_retry: true,
                result: Some(_),
                ..
            }
        )),
        "first overflow still emits a successful compact-for-retry end: {lifecycle:?}"
    );
    let starts = lifecycle
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::CompactionStart {
                    reason: CompactionReason::Overflow
                }
            )
        })
        .count();
    assert_eq!(starts, 1, "second overflow must not re-enter compaction: {lifecycle:?}");

    registration.unregister();
}

/// Application surfaces the second-overflow failure through RunFailed with the
/// same actionable guidance, then returns to a non-compacting idle state.
#[tokio::test]
async fn application_second_overflow_emits_actionable_run_failed() {
    let _guard = registry_guard();
    let model = unique_model("app-overflow-second");
    let (session, registration, _cwd) = session_with_responses(
        model,
        vec![
            FauxResponse::error(OVERFLOW_ERROR),
            FauxResponse::text("app second checkpoint"),
            FauxResponse::error(OVERFLOW_ERROR),
        ],
        Some(compact_settings()),
    );
    session
        .load_history(overflow_history_minimal())
        .await
        .expect("load history");
    let application = Application::new(session).await;
    let mut events = application.subscribe();

    application
        .prompt("app second overflow".into(), Vec::new(), None)
        .await
        .expect("start prompt");
    application.wait_for_idle().await;

    let state = application.state().await;
    assert!(!state.is_streaming);
    assert!(!state.is_compacting);

    let mut lifecycle = drain_application_events(&mut events);
    for _ in 0..64 {
        if lifecycle
            .iter()
            .any(|event| matches!(event, ApplicationEvent::RunFailed { .. }))
        {
            break;
        }
        if let Ok(event) = events.try_recv() {
            lifecycle.push(event);
        } else {
            tokio::task::yield_now().await;
        }
    }

    let failed = lifecycle.iter().find_map(|event| match event {
        ApplicationEvent::RunFailed { message } => Some(message.clone()),
        _ => None,
    });
    let message = failed.expect("expected RunFailed event");
    assert!(
        message.contains("Reduce the prompt or start a new session"),
        "RunFailed must stay actionable: {message}"
    );
    assert!(
        message.contains("Context overflow persisted after automatic compaction"),
        "RunFailed must identify overflow recovery exhaustion: {message}"
    );

    application.cleanup().await;
    registration.unregister();
}

/// Manual compact emits a paired lifecycle, preserves goal/todo state, and
/// leaves the session ready for a subsequent prompt that sees the checkpoint.
#[tokio::test]
async fn manual_compact_then_prompt_is_usable_and_preserves_goal_todo() {
    let _guard = registry_guard();
    // Large context window so the post-compact prompt does not re-enter
    // threshold compaction (which would steal the next faux response).
    let mut model = unique_model("manual-compact-usable");
    model.context_window = 200_000;
    let history = overflow_history_with_tail(&model);
    let contexts = Arc::new(Mutex::new(Vec::<Context>::new()));
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 20,
    };
    let (session, registration, session_dir) = session_with_context_capture(
        model.clone(),
        vec![
            FauxResponse::text("manual usable checkpoint"),
            FauxResponse::text("post-manual answer"),
        ],
        contexts.clone(),
        Some(settings),
    );

    let recorder = pi_coding::start_session_in(
        session_dir.path(),
        Some(&model),
        Some("off"),
        Some(session_dir.path()),
        Some("manual-compact-usable"),
        None,
    )
    .expect("recorder");
    for message in &history {
        recorder
            .record_message(message)
            .expect("record seed history");
    }
    session.record(recorder).expect("attach recorder");
    session.load_history(history).await.expect("load history");

    let application = Application::new(session).await;
    let mut events = application.subscribe();

    application
        .goal_create("manual compact survival", Some(80))
        .expect("create goal");
    application
        .apply_todo(TodoOp::Init {
            list: Some(vec![TodoInitPhase {
                phase: "Manual".into(),
                items: vec!["compact".into(), "continue".into()],
            }]),
            items: None,
            phase: None,
        })
        .expect("init todos");
    let todos_before = application.todo_state();
    let goal_before = application.goal_state();

    let result = application
        .compact(Some("keep decisions"))
        .await
        .expect("manual compact");
    assert!(
        result.summary.contains("manual usable checkpoint"),
        "summary must include faux checkpoint text: {}",
        result.summary
    );
    assert!(result.tokens_before > 0, "tokens_before must reflect pre-compact size");
    assert!(
        history_has_compaction_summary(&application.messages()),
        "manual compact must install CompactionSummary at head"
    );
    assert!(
        application.messages().iter().any(|message| matches!(
            message,
            Message::User(user)
                if user.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. } if text == "recent retained tail marker"
                ))
        )),
        "manual compact must retain the recent tail marker: {:?}",
        application.messages()
    );

    let lifecycle = collect_until_session_end(&mut events, CompactionReason::Manual).await;
    assert!(
        lifecycle.iter().any(|event| matches!(
            event,
            ApplicationEvent::Session(SessionEvent::CompactionStart {
                reason: CompactionReason::Manual
            })
        )),
        "manual compact must project CompactionStart: {lifecycle:?}"
    );
    assert!(
        lifecycle.iter().any(|event| matches!(
            event,
            ApplicationEvent::Session(SessionEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                aborted: false,
                will_retry: false,
                result: Some(_),
                ..
            })
        )),
        "manual compact must project successful CompactionEnd without retry: {lifecycle:?}"
    );
    let blob = serialized_event_blob(&lifecycle);
    assert!(
        !blob.contains("todo-error-reminder"),
        "manual compact events must not expose internal reminders: {blob}"
    );

    assert_eq!(application.todo_state().phases, todos_before.phases);
    assert_eq!(
        (
            application
                .goal_state()
                .current
                .as_ref()
                .map(|goal| (goal.objective.as_str(), goal.lifecycle)),
            application.goal_state().revision,
        ),
        (
            goal_before
                .current
                .as_ref()
                .map(|goal| (goal.objective.as_str(), goal.lifecycle)),
            goal_before.revision,
        )
    );
    assert!(!application.state().await.is_compacting);

    application
        .prompt("continue after manual compact".into(), Vec::new(), None)
        .await
        .expect("post-compact prompt");
    application.wait_for_idle().await;
    assert_eq!(
        application.last_assistant_text().as_deref(),
        Some("post-manual answer")
    );

    let captured = contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // Call 0: manual summary. Call 1: post-compact user prompt.
    assert_eq!(
        captured.len(),
        2,
        "manual compact + prompt must each hit the provider once, got {}",
        captured.len()
    );
    let prompt_text = context_text_blobs(&captured[1]).join("\n");
    assert!(
        prompt_text.contains("manual usable checkpoint"),
        "post-compact prompt context must include checkpoint: {prompt_text}"
    );
    assert!(
        prompt_text.contains("recent retained tail marker"),
        "post-compact prompt context must keep retained tail: {prompt_text}"
    );
    assert!(
        prompt_text.contains("continue after manual compact"),
        "post-compact prompt context must include the new user turn: {prompt_text}"
    );
    assert!(
        !prompt_text.contains("older context that should be summarized"),
        "post-compact prompt must not restore bulk pre-cut history: {prompt_text}"
    );
    assert!(
        !prompt_text.contains("selection_recommendations"),
        "post-compact prompt must not inject selection recommendations: {prompt_text}"
    );

    assert_eq!(application.todo_state().phases, todos_before.phases);
    assert_eq!(
        application
            .goal_state()
            .current
            .as_ref()
            .map(|goal| goal.lifecycle),
        Some(GoalLifecycle::Active)
    );

    application.cleanup().await;
    registration.unregister();
}

/// Compacting activity is true only while the summary stream runs, and clears
/// even when observed through Application.state (TUI projection).
#[tokio::test]
async fn application_is_compacting_only_while_manual_summary_runs() {
    let _guard = registry_guard();
    let summary_started = Arc::new(tokio::sync::Notify::new());
    let release_summary = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_fn: StreamFn = {
        let summary_started = summary_started.clone();
        let release_summary = release_summary.clone();
        let calls = calls.clone();
        Arc::new(move |model, _context, _options| {
            let summary_started = summary_started.clone();
            let release_summary = release_summary.clone();
            let call = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    summary_started.notify_one();
                    release_summary.notified().await;
                }
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text("gated manual checkpoint")];
                message.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                stream
                    .push(pi_ai::AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: message.clone(),
                    })
                    .await;
                stream.end(Some(message)).await;
                stream
            })
        })
    };

    let model = Model {
        id: format!("gated-compact-{}", uuid::Uuid::now_v7()),
        name: "Gated Compact".into(),
        context_window: 200_000,
        max_tokens: 32,
        ..Model::default()
    };
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: Some(CompactionSettings {
                enabled: true,
                reserve_tokens: 20,
                keep_recent_tokens: 20,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("session");
    session
        .load_history(overflow_history_with_tail(&model))
        .await
        .expect("history");
    let application = Application::new(session).await;

    let running = application.clone();
    let compact = tokio::spawn(async move { running.compact(None).await });
    summary_started.notified().await;
    assert!(
        application.state().await.is_compacting,
        "Application.state must report compacting while summary is in flight"
    );
    release_summary.notify_one();
    let result = compact.await.expect("join").expect("compact ok");
    assert!(result.summary.contains("gated manual checkpoint"));
    assert!(
        !application.state().await.is_compacting,
        "Application.state must clear compacting after manual compact"
    );

    application.cleanup().await;
}
