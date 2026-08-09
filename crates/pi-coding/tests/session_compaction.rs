use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, Model, StopReason, Usage,
    new_assistant_message_event_stream,
};
use pi_coding::{
    BeforeCompactionResult, CompactionDetails, CompactionReason, CompactionResult,
    CompactionSettings, RetrySettings, Session, SessionEvent, SessionOptions,
};

static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn registry_guard() -> MutexGuard<'static, ()> {
    REGISTRY_LOCK.lock().expect("provider registry test lock")
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
        FauxResponse::text("final answer"),
        FauxResponse::text("checkpoint summary"),
    ]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions {
        model,
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
    })
    .expect("build compacting session");
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
    assert!(matches!(session.history().first(), Some(Message::CompactionSummary(_))));
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
    let session = Session::new(SessionOptions {
        model,
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
    })
    .expect("build non-compacting session");
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
    let session = Session::new(SessionOptions {
        model,
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
    })
    .expect("build session");

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
                if call == 1 {
                    summary_started.notify_one();
                    release_summary.notified().await;
                }
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text(if call == 0 {
                    "final answer"
                } else {
                    "checkpoint summary"
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
    let session = Session::new(SessionOptions {
        model,
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
    })
    .expect("build compacting session");
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
        api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32,
    });
    registration.set_responses(vec![FauxResponse::text("manual checkpoint")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions {
        model, cwd: cwd.path().to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(), compaction: Some(CompactionSettings { enabled: true, reserve_tokens: 20, keep_recent_tokens: 4, snap_keep_turns: 10 }),
        stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None,
        stream_fn: None, auth_resolver: None,
    }).expect("build session");
    let history = vec![
        Message::user_text("older request ".repeat(20), 1),
        Message::Assistant({ let mut message = AssistantMessage::pending(&session.model().expect("model")); message.content = vec![ContentBlock::text("older answer")]; message.stop_reason = StopReason::Stop; message }),
        Message::user_text("recent request", 3),
    ];
    let recorder = pi_coding::start_session_in(
        cwd.path(), None, None, Some(cwd.path()), Some("manual-compaction-metadata"), None,
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
    assert!(matches!(events.recv().await.expect("start"), SessionEvent::CompactionStart { reason: CompactionReason::Manual }));
    assert!(matches!(events.recv().await.expect("end"), SessionEvent::CompactionEnd { reason: CompactionReason::Manual, aborted: false, result: Some(_), .. }));
    let tree = pi_coding::load_session_tree(&session_path).expect("load recorded compaction");
    let entry = tree.entries.iter().find(|entry| entry.entry_type == "compaction").expect("compaction entry");
    assert_eq!(entry.details, serde_json::to_value(result.details.as_ref().expect("generated details")).ok());
    assert_eq!(entry.usage, None);
    assert_eq!(entry.from_hook, None);
    registration.unregister();
}

#[tokio::test]
async fn hook_compaction_persists_details_usage_and_source() {
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions {
        model: Model::default(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: Some(CompactionSettings { enabled: true, reserve_tokens: 20, keep_recent_tokens: 4, snap_keep_turns: 10 }),
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");
    let recorder = pi_coding::start_session_in(
        cwd.path(), None, None, Some(cwd.path()), Some("hook-compaction-metadata"), None,
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
    recorder.record_message(&assistant).expect("record assistant");
    let recent = Message::user_text("recent request", 3);
    recorder.record_message(&recent).expect("record recent");
    let session_path = recorder.path();
    session.record(recorder).expect("attach recorder");
    session.load_history(vec![first, assistant, recent]).await.expect("load history");

    let details = CompactionDetails {
        read_files: vec!["src/lib.rs".to_owned()],
        modified_files: vec!["src/main.rs".to_owned()],
    };
    let usage = Usage { input: 17, output: 5, total_tokens: 22, ..Usage::default() };
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
            Ok(BeforeCompactionResult { cancel: false, compaction: Some(hook_result) })
        })
    })));
    session.compact(None).await.expect("hook compaction");

    let tree = pi_coding::load_session_tree(&session_path).expect("load hook compaction");
    let entry = tree.entries.iter().find(|entry| entry.entry_type == "compaction").expect("compaction entry");
    assert_eq!(entry.details, serde_json::to_value(details).ok());
    assert_eq!(entry.usage, Some(usage));
    assert_eq!(entry.from_hook, Some(true));
}

#[tokio::test]
async fn overflow_compacts_once_then_retries_without_duplicate_user_turn() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "overflow-faux".into(); model.api = "overflow-faux-api".into(); model.provider = "faux".into();
    model.base_url = "http://localhost:0".into(); model.context_window = 100; model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions { api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32 });
    registration.set_responses(vec![FauxResponse::error("input exceeds the context window"), FauxResponse::text("overflow checkpoint"), FauxResponse::text("recovered answer")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions { model, cwd: cwd.path().to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(), compaction: Some(CompactionSettings { enabled: true, reserve_tokens: 20, keep_recent_tokens: 4, snap_keep_turns: 10 }),
        stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("build session");
    session.load_history(vec![Message::user_text("older context ".repeat(30), 1)]).await.expect("load history");
    let mut events = session.subscribe_session_events();
    let result = session.run("one user turn", Vec::new()).await.expect("overflow recovery");
    assert_eq!(result.text, "recovered answer");
    assert_eq!(session.history().iter().filter(|message| matches!(message, Message::User(user) if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "one user turn")))).count(), 1);
    let mut saw_start = false; let mut saw_end = false;
    while let Ok(event) = events.try_recv() {
        saw_start |= matches!(event, SessionEvent::CompactionStart { reason: CompactionReason::Overflow });
        saw_end |= matches!(event, SessionEvent::CompactionEnd { reason: CompactionReason::Overflow, aborted: false, will_retry: true, result: Some(_), .. });
    }
    assert!(saw_start && saw_end);
    registration.unregister();
}

#[tokio::test]
async fn second_overflow_stops_with_actionable_error() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "overflow-stop-faux".into(); model.api = "overflow-stop-faux-api".into(); model.provider = "faux".into();
    model.base_url = "http://localhost:0".into(); model.context_window = 100; model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions { api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32 });
    registration.set_responses(vec![FauxResponse::error("input exceeds the context window"), FauxResponse::text("overflow checkpoint"), FauxResponse::error("input exceeds the context window")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions { model, cwd: cwd.path().to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(), compaction: Some(CompactionSettings { enabled: true, reserve_tokens: 20, keep_recent_tokens: 4, snap_keep_turns: 10 }),
        stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("build session");
    session.load_history(vec![Message::user_text("older context ".repeat(30), 1)]).await.expect("load history");
    let error = session.run("one user turn", Vec::new()).await.expect_err("second overflow stops");
    assert!(error.to_string().contains("Reduce the prompt or start a new session"));
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
    let session = Session::new(SessionOptions {
        model: model.clone(),
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
    })
    .expect("build session");
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::Assistant({ let mut message = AssistantMessage::pending(&model); message.content = vec![ContentBlock::text("older response ".repeat(20))]; message.stop_reason = StopReason::Stop; message.timestamp = 2; message }),
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
    assert!(request_model.headers.as_ref().is_none_or(std::collections::HashMap::is_empty));
    assert_eq!(request_options.stream.api_key.as_deref(), Some("compaction-api-secret"));
    assert_eq!(
        request_options.stream.headers.get("X-Probe").map(String::as_str),
        Some("compaction-header-secret")
    );
    assert_eq!(
        request_options.stream.env.get("COMPACTION_AUTH_ENV").map(String::as_str),
        Some("compaction-env-secret")
    );
}

#[tokio::test]
async fn manual_compaction_retries_transient_summary_and_emits_ordered_events() {
    let _guard = registry_guard();
    let mut model = Model::default();
    model.id = "summary-retry-faux".into(); model.api = "summary-retry-faux-api".into(); model.provider = "faux".into();
    model.base_url = "http://localhost:0".into(); model.context_window = 200; model.max_tokens = 32;
    let registration = register_faux_provider(FauxProviderOptions { api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32 });
    registration.set_responses(vec![FauxResponse::error("503 Service unavailable"), FauxResponse::text("retried checkpoint")]);
    let cwd = tempfile::tempdir().expect("temporary working directory");
    let session = Session::new(SessionOptions { model, cwd: cwd.path().to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(), compaction: Some(CompactionSettings { enabled: true, reserve_tokens: 20, keep_recent_tokens: 4, snap_keep_turns: 10 }),
        stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("build session");
    session.set_retry_settings(RetrySettings { enabled: true, max_retries: 1, base_delay_ms: 1 , ..Default::default() });
    session.load_history(vec![Message::user_text("older request ".repeat(20), 1), Message::Assistant({ let mut message = AssistantMessage::pending(&session.model().expect("model")); message.content = vec![ContentBlock::text("older response ".repeat(20))]; message.stop_reason = StopReason::Stop; message.timestamp = 2; message }), Message::user_text("recent", 3)]).await.expect("load");
    let mut events = session.subscribe_session_events();
    let result = session.compact(None).await.expect("retried compact");
    assert!(result.summary.ends_with("retried checkpoint"), "{}", result.summary);
    let mut lifecycle = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            SessionEvent::CompactionStart { .. } => lifecycle.push("start"),
            SessionEvent::SummarizationRetryScheduled { attempt: 1, .. } => lifecycle.push("scheduled"),
            SessionEvent::SummarizationRetryAttemptStart { .. } => lifecycle.push("attempt"),
            SessionEvent::SummarizationRetryFinished => lifecycle.push("finished"),
            SessionEvent::CompactionEnd { result: Some(_), .. } => lifecycle.push("end"),
            _ => {}
        }
    }
    assert_eq!(lifecycle, ["start", "scheduled", "attempt", "finished", "end"]);
    registration.unregister();
}
