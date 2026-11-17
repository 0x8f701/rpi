use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, StopReason,
    Usage, new_assistant_message_event_stream,
};
use pi_coding::{
    BeforeCompactionResult, CompactionDetails, CompactionReason, CompactionResult,
    CompactionSettings, ResourceDiscovery, RetrySettings, Session, SessionEvent, SessionOptions,
    ToolSelection, estimate_context_tokens, estimate_context_tokens_usage_aware,
    estimate_live_context_tokens, estimate_message_tokens, should_compact,
};


static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn registry_guard() -> MutexGuard<'static, ()> {
    REGISTRY_LOCK.lock().expect("provider registry test lock")
}
fn hermetic_session(options: SessionOptions) -> Session {
    Session::new_with_additional_tools_filtered_and_discovery(
        options,
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("build hermetic session")
}


#[tokio::test]
async fn enabled_compaction_summarizes_after_completed_prompt() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "compaction-faux".into();
    model.name = "Compaction Faux".into();
    model.api = "compaction-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 60;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    registration.set_responses(vec![
        FauxResponse::text("checkpoint summary"),
        FauxResponse::text("final answer"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    let answer = session
        .run_print(&mut Vec::new(), "continue")
        .await
        .expect("run prompt after compaction");
    assert_eq!(answer, "final answer", "history: {:?}", session.history());
    assert!(matches!(
        session.history().first(),
        Some(Message::CompactionSummary(_))
    ));
    registration.unregister();
}

#[tokio::test]
async fn disabled_compaction_does_not_consume_summary_response() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "no-compaction-faux".into();
    model.name = "No Compaction Faux".into();
    model.api = "no-compaction-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 10;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    registration.set_responses(vec![FauxResponse::text("direct answer")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![Message::user_text("large history ".repeat(20), 1)])
        .await
        .expect("load history");
    let answer = session
        .run_print(&mut Vec::new(), "continue")
        .await
        .expect("run prompt without compaction");
    assert_eq!(answer, "direct answer");
    registration.unregister();
}

#[test]
fn auto_compaction_toggle_is_truthful_without_initial_settings() {
    let mut model = Model::default();
    model.id = "compaction-toggle".into();
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });

    assert!(!session.auto_compaction_enabled());
    session.set_auto_compaction_enabled(true);
    assert!(session.auto_compaction_enabled());
    session.set_auto_compaction_enabled(false);
    assert!(!session.auto_compaction_enabled());
    assert!(!session.is_compacting());
}

#[tokio::test]
async fn compaction_activity_is_set_only_while_summary_runs() {
    let summary_started = Arc::new(tokio::sync::Notify::new());
    let release_summary = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_fn: StreamFn = {
        let summary_started = summary_started.clone();
        let release_summary = release_summary.clone();
        let calls = calls.clone();
        Arc::new(move |model, _context, _options| {
            let summary_started = summary_started.clone();
            let release_summary = release_summary.clone();
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    summary_started.notify_one();
                    release_summary.notified().await;
                }
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text(if call == 0 {
                    "checkpoint summary"
                } else {
                    "final answer"
                })];
                message.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                stream
                    .push(AssistantMessageEvent::Done {
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
        id: "compaction-activity".into(),
        name: "Compaction Activity".into(),
        context_window: 60,
        max_tokens: 32,
        ..Model::default()
    };
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");

    let running = session.clone();
    let run = tokio::spawn(async move { running.run("continue", Vec::new()).await });
    summary_started.notified().await;
    assert!(session.is_compacting());
    release_summary.notify_one();
    run.await.expect("join run").expect("complete run");
    assert!(!session.is_compacting());
}

#[tokio::test]
async fn manual_compaction_emits_paired_success_events() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "manual-compaction-faux".into();
    model.api = "manual-compaction-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 200;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(vec![FauxResponse::text("manual checkpoint")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    let history = vec![
        Message::user_text("older request ".repeat(20), 1),
        Message::Assistant({
            let mut message = AssistantMessage::pending(&session.model().expect("model"));
            message.content = vec![ContentBlock::text("older answer")];
            message.stop_reason = StopReason::Stop;
            message
        }),
        Message::user_text("recent request", 3),
    ];
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        None,
        None,
        Some(cwd.path()),
        Some("manual-compaction-metadata"),
        None,
    )
    .expect("start recorder");
    for message in &history {
        recorder.record_message(message).expect("record history");
    }
    let session_path = recorder.path();
    session.record(recorder).expect("attach recorder");
    session.load_history(history).await.expect("load history");
    let mut events = session.subscribe_session_events();
    let result = session.compact(None).await.expect("manual compact");
    assert_eq!(result.summary, "manual checkpoint");
    assert!(matches!(
        events.recv().await.expect("start"),
        SessionEvent::CompactionStart {
            reason: CompactionReason::Manual
        }
    ));
    assert!(matches!(
        events.recv().await.expect("end"),
        SessionEvent::CompactionEnd {
            reason: CompactionReason::Manual,
            aborted: false,
            result: Some(_),
            ..
        }
    ));
    let tree = pi_coding::load_session_tree(&session_path).expect("load recorded compaction");
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.entry_type == "compaction")
        .expect("compaction entry");
    assert_eq!(
        entry.details,
        serde_json::to_value(result.details.as_ref().expect("generated details")).ok()
    );
    assert_eq!(entry.usage, None);
    assert_eq!(entry.from_hook, None);
    registration.unregister();
}

#[tokio::test]
async fn hook_compaction_persists_details_usage_and_source() {
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(Model::default()),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        None,
        None,
        Some(cwd.path()),
        Some("hook-compaction-metadata"),
        None,
    )
    .expect("start recorder");
    let first = Message::user_text("older request", 1);
    let first_id = recorder.record_message(&first).expect("record first");
    let assistant = Message::Assistant({
        let mut message = AssistantMessage::pending(&session.model().expect("model"));
        message.content = vec![ContentBlock::text("older answer")];
        message.stop_reason = StopReason::Stop;
        message
    });
    recorder
        .record_message(&assistant)
        .expect("record assistant");
    let recent = Message::user_text("recent request", 3);
    recorder.record_message(&recent).expect("record recent");
    let session_path = recorder.path();
    session.record(recorder).expect("attach recorder");
    session
        .load_history(vec![first, assistant, recent])
        .await
        .expect("load history");

    let details = CompactionDetails {
        read_files: vec!["src/lib.rs".to_owned()],
        modified_files: vec!["src/main.rs".to_owned()],
    };
    let usage = Usage {
        input: 17,
        output: 5,
        total_tokens: 22,
        ..Usage::default()
    };
    let hook_result = CompactionResult {
        summary: "hook checkpoint".to_owned(),
        first_kept_entry_id: first_id,
        tokens_before: 777,
        estimated_tokens_after: Some(12),
        usage: Some(usage.clone()),
        details: Some(details.clone()),
    };
    session.set_before_compaction(Some(Arc::new(move |_| {
        let hook_result = hook_result.clone();
        Box::pin(async move {
            Ok(BeforeCompactionResult {
                cancel: false,
                compaction: Some(hook_result),
            })
        })
    })));
    session.compact(None).await.expect("hook compaction");

    let tree = pi_coding::load_session_tree(&session_path).expect("load hook compaction");
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.entry_type == "compaction")
        .expect("compaction entry");
    assert_eq!(entry.details, serde_json::to_value(details).ok());
    assert_eq!(entry.usage, Some(usage));
    assert_eq!(entry.from_hook, Some(true));
}

#[tokio::test]
async fn overflow_compacts_once_then_retries_without_duplicate_user_turn() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "overflow-faux".into();
    model.api = "overflow-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 100;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(vec![
        FauxResponse::error("input exceeds the context window"),
        FauxResponse::text("overflow checkpoint"),
        FauxResponse::text("recovered answer"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![Message::user_text("older context ".repeat(30), 1)])
        .await
        .expect("load history");
    let mut events = session.subscribe_session_events();
    let result = session
        .run("one user turn", Vec::new())
        .await
        .expect("overflow recovery");
    assert_eq!(result.text, "recovered answer");
    assert_eq!(session.history().iter().filter(|message| matches!(message, Message::User(user) if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "one user turn")))).count(), 1);
    let mut saw_start = false;
    let mut saw_end = false;
    while let Ok(event) = events.try_recv() {
        saw_start |= matches!(
            event,
            SessionEvent::CompactionStart {
                reason: CompactionReason::Overflow
            }
        );
        saw_end |= matches!(
            event,
            SessionEvent::CompactionEnd {
                reason: CompactionReason::Overflow,
                aborted: false,
                will_retry: true,
                result: Some(_),
                ..
            }
        );
    }
    assert!(saw_start && saw_end);
    registration.unregister();
}

#[tokio::test]
async fn second_overflow_stops_with_actionable_error() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "overflow-stop-faux".into();
    model.api = "overflow-stop-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 100;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(vec![
        FauxResponse::error("input exceeds the context window"),
        FauxResponse::text("overflow checkpoint"),
        FauxResponse::error("input exceeds the context window"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![Message::user_text("older context ".repeat(30), 1)])
        .await
        .expect("load history");
    let error = session
        .run("one user turn", Vec::new())
        .await
        .expect_err("second overflow stops");
    assert!(
        error
            .to_string()
            .contains("Reduce the prompt or start a new session")
    );
    registration.unregister();
}

#[tokio::test]
async fn compaction_resolves_auth_into_request_options_without_mutating_model() {
    let captured = Arc::new(Mutex::new(None));
    let captured_request = captured.clone();
    let stream_fn: StreamFn = Arc::new(move |model, _context, options| {
        let captured = captured_request.clone();
        Box::pin(async move {
            *captured.lock().expect("capture lock") = Some((model.clone(), options));
            let mut message = AssistantMessage::pending(&model);
            message.content = vec![ContentBlock::text("secure checkpoint")];
            message.stop_reason = StopReason::Stop;
            let stream = new_assistant_message_event_stream();
            stream
                .push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: message.clone(),
                })
                .await;
            stream.end(Some(message)).await;
            stream
        })
    });
    let resolver: pi_coding::SessionAuthResolver = Arc::new(|_model: Model| {
        Box::pin(async {
            Ok(pi_coding::RequestAuth {
                api_key: "compaction-api-secret".to_owned(),
                headers: std::collections::HashMap::from([(
                    "X-Probe".to_owned(),
                    "compaction-header-secret".to_owned(),
                )]),
                env: std::collections::HashMap::from([(
                    "COMPACTION_AUTH_ENV".to_owned(),
                    "compaction-env-secret".to_owned(),
                )]),
                ..pi_coding::RequestAuth::default()
            })
        }) as pi_agent::BoxFuture<anyhow::Result<pi_coding::RequestAuth>>
    });
    let model = Model {
        id: "secure-compaction".into(),
        name: "Secure Compaction".into(),
        context_window: 200,
        max_tokens: 64,
        ..Model::default()
    };
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model.clone()),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: Some(resolver),
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::Assistant({
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text("older response ".repeat(20))];
                message.stop_reason = StopReason::Stop;
                message.timestamp = 2;
                message
            }),
            Message::user_text("recent request", 3),
        ])
        .await
        .expect("load history");

    session.compact(None).await.expect("manual compact");
    assert_eq!(session.model(), Some(model));
    let (request_model, request_options) = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("captured compaction request");
    assert!(
        request_model
            .headers
            .as_ref()
            .is_none_or(std::collections::HashMap::is_empty)
    );
    assert_eq!(
        request_options.stream.api_key.as_deref(),
        Some("compaction-api-secret")
    );
    assert_eq!(
        request_options
            .stream
            .headers
            .get("X-Probe")
            .map(String::as_str),
        Some("compaction-header-secret")
    );
    assert_eq!(
        request_options
            .stream
            .env
            .get("COMPACTION_AUTH_ENV")
            .map(String::as_str),
        Some("compaction-env-secret")
    );
}

#[tokio::test]
async fn manual_compaction_retries_transient_summary_and_emits_ordered_events() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "summary-retry-faux".into();
    model.api = "summary-retry-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 200;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(vec![
        FauxResponse::error("503 Service unavailable"),
        FauxResponse::text("retried checkpoint"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 1,
        base_delay_ms: 1,
        ..Default::default()
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::Assistant({
                let mut message = AssistantMessage::pending(&session.model().expect("model"));
                message.content = vec![ContentBlock::text("older response ".repeat(20))];
                message.stop_reason = StopReason::Stop;
                message.timestamp = 2;
                message
            }),
            Message::user_text("recent", 3),
        ])
        .await
        .expect("load");
    let mut events = session.subscribe_session_events();
    let result = session.compact(None).await.expect("retried compact");
    assert!(
        result.summary.ends_with("retried checkpoint"),
        "{}",
        result.summary
    );
    let mut lifecycle = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            SessionEvent::CompactionStart { .. } => lifecycle.push("start"),
            SessionEvent::SummarizationRetryScheduled { attempt: 1, .. } => {
                lifecycle.push("scheduled")
            }
            SessionEvent::SummarizationRetryAttemptStart { .. } => lifecycle.push("attempt"),
            SessionEvent::SummarizationRetryFinished => lifecycle.push("finished"),
            SessionEvent::CompactionEnd {
                result: Some(_), ..
            } => lifecycle.push("end"),
            _ => {}
        }
    }
    assert_eq!(
        lifecycle,
        ["start", "scheduled", "attempt", "finished", "end"]
    );
    registration.unregister();
}

fn context_text(context: &Context) -> String {
    context
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => message.content.iter(),
            Message::Assistant(message) => message.content.iter(),
            Message::ToolResult(message) => message.content.iter(),
            other => panic!("provider context contains an unprojected session message: {other:?}"),
        })
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Regression: a turn that starts with its live context already above
/// `should_compact` must summarize BEFORE the turn's first provider request.
/// A threshold check that only runs in `finish_run` lets the oversized context
/// reach the provider and defers compaction to the next successful turn, which
/// is the "auto-run errors once, user has to send another message" lag.
#[tokio::test]
async fn threshold_compaction_preflights_before_the_first_provider_request() {
    let contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_fn: StreamFn = {
        let contexts = contexts.clone();
        Arc::new(move |model, context, _options| {
            let contexts = contexts.clone();
            Box::pin(async move {
                let call = {
                    let mut captured = contexts.lock().expect("captured contexts");
                    captured.push(context.clone());
                    captured.len() - 1
                };
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text(if call == 0 {
                    "checkpoint summary"
                } else {
                    "final answer"
                })];
                message.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                stream
                    .push(AssistantMessageEvent::Done {
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
        id: "compaction-preflight".into(),
        name: "Compaction Preflight".into(),
        context_window: 60,
        max_tokens: 32,
        ..Model::default()
    };
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    };
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(settings.clone()),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    let preloaded = session.history();
    assert!(
        should_compact(estimate_live_context_tokens(&preloaded), 60, &settings),
        "preloaded context must already exceed the threshold: {preloaded:?}"
    );
    let mut events = session.subscribe_session_events();
    let answer = session
        .run_print(&mut Vec::new(), "first normal request")
        .await
        .expect("run prompt");
    assert_eq!(
        answer, "final answer",
        "the normal request must be served by the post-summary response"
    );

    let captured = contexts.lock().expect("captured contexts").clone();
    assert!(
        captured.len() >= 2,
        "expected a compaction summary call before the normal turn request, saw {} provider call(s)",
        captured.len(),
    );
    assert_eq!(
        captured[0].messages.len(),
        1,
        "the first provider call must be the summarization request, not the oversized turn"
    );
    let normal = context_text(&captured[1]);
    assert!(
        normal.contains("checkpoint summary"),
        "the first normal request must carry the compaction summary"
    );
    assert!(
        normal.contains("first normal request"),
        "the second provider call must be the user's turn"
    );

    let mut reasons = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let SessionEvent::CompactionStart { reason } = event {
            reasons.push(reason);
        }
    }
    assert_eq!(
        reasons,
        vec![CompactionReason::Threshold],
        "preflight compaction must publish exactly one threshold start"
    );
    assert!(
        matches!(
            session.history().first(),
            Some(Message::CompactionSummary(_))
        ),
        "history must start with the compaction summary"
    );
}


#[tokio::test]
async fn preflight_hook_decline_continues_on_last_good_history() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "decline-faux".into();
    model.api = "decline-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 60;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    registration.set_responses(vec![FauxResponse::text("decline answer")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    session.set_before_compaction(Some(Arc::new(|_| {
        Box::pin(async move {
            Ok(BeforeCompactionResult {
                cancel: true,
                compaction: None,
            })
        })
    })));
    let mut events = session.subscribe_session_events();
    let answer = session
        .run_print(&mut Vec::new(), "first normal request")
        .await
        .expect("decline must continue on last-good history");
    assert_eq!(answer, "decline answer");
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, SessionEvent::CompactionStart { .. }),
            "a declined preflight must not publish a compaction start: {event:?}"
        );
    }
    let history = session.history();
    assert!(
        !matches!(history.first(), Some(Message::CompactionSummary(_))),
        "last-good history must be preserved: {history:?}"
    );
    assert_eq!(
        history.len(),
        4,
        "history must keep both preloaded turns plus this prompt and answer: {history:?}"
    );
    registration.unregister();
}

#[tokio::test]
async fn preflight_abort_returns_typed_abort_without_provider_or_queue_restart() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "abort-faux".into();
    model.api = "abort-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 60;
    model.max_tokens = 32;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_fn: StreamFn = {
        let calls = calls.clone();
        Arc::new(move |model, _context, _options| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text("aborted turn answer")];
                message.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                stream
                    .push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: message.clone(),
                    })
                    .await;
                stream.end(Some(message)).await;
                stream
            })
        })
    };
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    session
        .steer(Message::user_text("queued mid-abort", 3))
        .await
        .expect("steer queued before the run");
    let hook_session = session.clone();
    session.set_before_compaction(Some(Arc::new(move |_| {
        let hook_session = hook_session.clone();
        Box::pin(async move {
            hook_session.abort().await;
            Ok(BeforeCompactionResult::default())
        })
    })));
    let result = session.run("first normal request", Vec::new()).await;
    let error = result.expect_err("aborted preflight must surface the abort");
    assert!(
        error.downcast_ref::<pi_agent::AbortError>().is_some(),
        "preflight abort must be the typed AbortError: {error:#}"
    );
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) <= 1,
        "the turn request must not start after the abort"
    );
    assert_eq!(
        session.pending_message_count().await,
        1,
        "the queued steering must stay queued, never restart a provider turn"
    );
}

#[tokio::test]
async fn queued_next_turn_messages_count_toward_preflight_threshold() {
    let contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_fn: StreamFn = {
        let contexts = contexts.clone();
        Arc::new(move |model, context, _options| {
            let contexts = contexts.clone();
            Box::pin(async move {
                let call = {
                    let mut captured = contexts.lock().expect("captured contexts");
                    captured.push(context);
                    captured.len() - 1
                };
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text(if call == 0 {
                    "checkpoint summary"
                } else {
                    "final answer"
                })];
                message.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                stream
                    .push(AssistantMessageEvent::Done {
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
        id: "queued-preflight".into(),
        name: "Queued Preflight".into(),
        context_window: 60,
        max_tokens: 32,
        ..Model::default()
    };
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    };
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(settings.clone()),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(3), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    let preloaded = session.history();
    assert!(
        !should_compact(estimate_live_context_tokens(&preloaded), 60, &settings),
        "history alone must sit under the threshold: {preloaded:?}"
    );
    session
        .send_custom_message(
            pi_ai::CustomMessage {
                custom_type: "pi.test.queued".to_owned(),
                content: "queued payload ".repeat(40).into(),
                display: false,
                details: None,
                timestamp: 2,
            },
            pi_coding::MessageDelivery::NextTurn,
            false,
        )
        .await
        .expect("queue next-turn message");
    let mut events = session.subscribe_session_events();
    let answer = session
        .run_print(&mut Vec::new(), "first normal request")
        .await
        .expect("run prompt");
    assert_eq!(answer, "final answer");
    let captured = contexts.lock().expect("captured contexts").clone();
    assert!(
        captured.len() >= 2,
        "the queued message must push preflight over the threshold: {} call(s)",
        captured.len()
    );
    assert_eq!(
        captured[0].messages.len(),
        1,
        "the first provider call must be the summarization request"
    );
    let mut saw_threshold = false;
    while let Ok(event) = events.try_recv() {
        saw_threshold |= matches!(
            event,
            SessionEvent::CompactionStart {
                reason: CompactionReason::Threshold
            }
        );
    }
    assert!(saw_threshold, "preflight must publish a threshold start");
}

#[tokio::test]
async fn tool_result_overflow_compacts_and_retries_in_one_run() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "tool-overflow-faux".into();
    model.api = "tool-overflow-faux-api".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    model.context_window = 100;
    model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(pi_ai::ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::error("input exceeds the context window"),
        FauxResponse::text("overflow checkpoint"),
        FauxResponse::text("turn prefix checkpoint"),
        FauxResponse::text("recovered answer"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let echo = pi_agent::AgentTool::new(
        "echo",
        "echo",
        pi_ai::Schema::default(),
        |_context| async {
            Ok(pi_agent::AgentToolResult {
                content: vec![ContentBlock::text("tool output")],
                ..Default::default()
            })
        },
    );
    let session = hermetic_session(SessionOptions {
        model: Some(model),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: Some(CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 4,
            snap_keep_turns: 10,
        }),
        stream_options: Default::default(),
        tools: Some(vec![echo]),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    });
    session
        .load_history(vec![Message::user_text("older context ".repeat(30), 1)])
        .await
        .expect("load history");
    let mut events = session.subscribe_session_events();
    let result = session
        .run("one user turn", Vec::new())
        .await
        .expect("same-run overflow recovery");
    assert_eq!(result.text, "recovered answer");
    let history = session.history();
    assert!(
        history
            .iter()
            .any(|message| matches!(message, Message::ToolResult(result) if result.tool_name == "echo")),
        "the tool result must stay in the transcript: {history:?}"
    );
    assert!(
        matches!(history.first(), Some(Message::CompactionSummary(_))),
        "overflow compaction must run before the retry"
    );
    let mut reasons = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let SessionEvent::CompactionStart { reason } = event {
            reasons.push(reason);
        }
    }
    assert_eq!(
        reasons,
        vec![CompactionReason::Overflow],
        "mid-loop overflow must compact exactly once for the overflow reason"
    );
    registration.unregister();
}
