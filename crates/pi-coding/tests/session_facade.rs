use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_agent::{AgentEvent, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context,
    new_assistant_message_event_stream,
};
use pi_ai::{ContentBlock, Message, Model, SimpleStreamOptions, StopReason, ToolCall, Usage};
use pi_coding::{MessageDelivery, RetrySettings, Session, SessionEvent, SessionOptions};
use serde_json::json;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// Per-process isolated native session root so `start_new_recording()` never
/// writes into the real `~/.pi/agent/sessions` tree (Web sidebar source).
fn test_sessions_root() -> PathBuf {
    static ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
        tempfile::tempdir().expect("test sessions root")
    });
    ROOT.path().to_path_buf()
}

fn make_session(
    responses: Vec<FauxResponse>,
) -> (Session, pi_ai::providers::FauxProviderRegistration) {
    let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut model = Model::default();
    model.id = format!("facade-{suffix}");
    model.name = "Facade".into();
    model.api = format!("facade-api-{suffix}");
    model.provider = format!("facade-provider-{suffix}");
    model.base_url = "http://localhost:0".into();
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 64,
    });
    registration.set_responses(responses);
    let cwd = tempfile::tempdir().expect("tempdir");
    let cwd_path = cwd.path().to_path_buf();
    std::mem::forget(cwd);
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd_path,
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("session");
    session.set_session_dir(test_sessions_root());
    (session, registration)
}

fn configure_fast_retry(session: &Session, enabled: bool, max_retries: usize) {
    session.set_retry_settings(RetrySettings {
        enabled,
        max_retries,
        base_delay_ms: 10,
        model_fallback: false,
        fallback_chains: Default::default(),
    });
}

#[tokio::test]
async fn persistent_subscription_observes_multiple_runs_and_prompt_bytes_are_preserved() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) =
        make_session(vec![FauxResponse::text("one"), FauxResponse::text("two")]);
    let starts = Arc::new(Mutex::new(0usize));
    let seen = starts.clone();
    let _subscription = session
        .subscribe(move |event| {
            let seen = seen.clone();
            async move {
                if matches!(event, AgentEvent::AgentStart) {
                    *seen.lock().expect("lock") += 1;
                }
                Ok(())
            }
        })
        .await;
    session.run("  first\n", vec![]).await.expect("first");
    session.run("second", vec![]).await.expect("second");
    assert_eq!(*starts.lock().expect("lock"), 2);
    let first = session.history().into_iter().find_map(|message| match message {
        Message::User(user) => Some(user),
        _ => None,
    }).expect("user");
    assert!(matches!(&first.content[0], ContentBlock::Text { text, .. } if text == "  first\n"));
    registration.unregister();
}

#[tokio::test]
async fn run_result_aggregates_tool_calls_usage_and_final_stop_reason() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let call = ToolCall {
        id: "missing".into(),
        name: "missing".into(),
        arguments: json!({"path":"x"}),
        thought_signature: None,
    };
    let (session, registration) = make_session(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(call.clone())],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::text("done"),
    ]);
    let result = session.run("go", vec![]).await.expect("run");
    assert_eq!(result.text, "done");
    assert_eq!(result.tool_calls, [call]);
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.usage, Usage::default());
    registration.unregister();
}

#[tokio::test]
async fn continue_run_resumes_user_last_transcript() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(vec![FauxResponse::text("continued")]);
    session
        .load_history(vec![Message::user_text("resume", 1)])
        .await
        .expect("load");
    let result = session.continue_run().await.expect("continue");
    assert_eq!(result.text, "continued");
    registration.unregister();
}

#[tokio::test]
async fn run_print_emits_exact_tool_lifecycle_and_single_final_text() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let command = "x".repeat(80);
    let call = ToolCall {
        id: "missing".into(),
        name: "missing".into(),
        arguments: json!({"command":command}),
        thought_signature: None,
    };
    let (session, registration) = make_session(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(call)],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::text("final"),
    ]);
    let mut output = Vec::new();
    let text = session.run_print(&mut output, "go").await.expect("print");
    let output = String::from_utf8(output).expect("utf8");
    assert_eq!(text, "final");
    assert!(output.contains(
        "\n\x1b[2m· missing(xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx...)\x1b[0m\n"
    ));
    assert!(output.contains("\x1b[2m  └ error\x1b[0m\n"));
    assert_eq!(output.matches("final").count(), 1);
    registration.unregister();
}

#[tokio::test]
async fn message_end_subscriber_observes_live_session_history() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(vec![FauxResponse::text("answer")]);
    let observed_lengths = Arc::new(Mutex::new(Vec::new()));
    let observed = observed_lengths.clone();
    let observed_session = session.clone();
    let _subscription = session
        .subscribe(move |event| {
            let observed = observed.clone();
            let observed_session = observed_session.clone();
            async move {
                if matches!(event, AgentEvent::MessageEnd { .. }) {
                    observed
                        .lock()
                        .expect("lock")
                        .push(observed_session.history().len());
                }
                Ok(())
            }
        })
        .await;
    session.run("question", vec![]).await.expect("run");
    assert_eq!(*observed_lengths.lock().expect("lock"), [1, 2]);
    registration.unregister();
}

#[test]
fn dropped_run_survives_owning_runtime_shutdown_and_session_reuses() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let calls = Arc::new(AtomicU64::new(0));
    let seen = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context: Context, options| {
        let seen = seen.clone();
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            let call = seen.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: message.clone(),
                    })
                    .await;
                if call == 0 {
                    if let Some(abort) = options.stream.abort_signal {
                        abort.cancelled().await;
                    }
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("dropped".into());
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: message.clone(),
                        })
                        .await;
                } else {
                    message.content = vec![ContentBlock::text("reused")];
                    message.stop_reason = StopReason::Stop;
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                }
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let model = Model {
        id: "drop-runtime".into(),
        name: "Drop runtime".into(),
        api: "unused".into(),
        provider: "unused".into(),
        ..Model::default()
    };
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "key".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(vec![]),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("session");
    let thread_session = session.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let mut run = Box::pin(thread_session.run("drop", vec![]));
            tokio::select! {
                _ = &mut run => panic!("run completed before drop"),
                () = tokio::task::yield_now() => {}
            }
        });
    })
    .join()
    .expect("thread");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        session.wait_for_idle().await;
        let result = session.run("again", vec![]).await.expect("reuse");
        assert_eq!(result.text, "reused");
    });
}

#[tokio::test]
async fn direct_bash_streams_persists_and_respects_context_exclusion() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(Vec::new());
    let mut events = session.subscribe_session_events();
    session.start_new_recording().expect("start recorder");
    let result = session.execute_bash("printf visible", false).await.expect("visible bash");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.output, "visible");
    // Production streams arbitrary raw pipe-read chunks (each `read()` is one
    // `BashExecutionUpdate.delta`); the streamed byte content is the
    // concatenation in arrival order, never a guaranteed single chunk. Drain
    // updates through the terminal end event, then assert the bytes.
    let mut streamed = String::new();
    let end = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("bash event timeout")
            .expect("bash event");
        match event {
            SessionEvent::BashExecutionUpdate { delta, .. } => streamed.push_str(&delta),
            terminal @ SessionEvent::BashExecutionEnd { .. } => break terminal,
            _ => {}
        }
    };
    assert_eq!(streamed, "visible");
    assert!(matches!(end, SessionEvent::BashExecutionEnd { ref message } if message.command == "printf visible" && message.exit_code == Some(0) && message.exclude_from_context.is_none()));
    let excluded = session.execute_bash("printf hidden", true).await.expect("excluded bash");
    assert_eq!(excluded.output, "hidden");
    let history = session.history();
    assert!(matches!(&history[history.len() - 2], Message::BashExecution(message) if message.output == "visible" && message.exclude_from_context.is_none()));
    assert!(matches!(&history[history.len() - 1], Message::BashExecution(message) if message.output == "hidden" && message.exclude_from_context == Some(true)));
    let entries = session.session_entries(None).expect("session entries").entries;
    assert!(entries.iter().any(|entry| matches!(&entry.message, Some(Message::BashExecution(message)) if message.command == "printf visible" && message.output == "visible")));
    assert!(entries.iter().any(|entry| matches!(&entry.message, Some(Message::BashExecution(message)) if message.command == "printf hidden" && message.exclude_from_context == Some(true))));
    registration.unregister();
}

#[tokio::test]
async fn direct_bash_nonzero_and_abort_are_semantic_results() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(Vec::new());
    let nonzero = session.execute_bash("exit 7", false).await.expect("nonzero bash");
    assert_eq!(nonzero.exit_code, Some(7));
    assert!(!nonzero.cancelled);
    let running = session.clone();
    let task = tokio::spawn(async move { running.execute_bash("sleep 30", false).await });
    for _ in 0..100 {
        if session.is_bash_running() { break; }
        tokio::task::yield_now().await;
    }
    assert!(session.is_bash_running());
    session.abort_bash();
    let aborted = tokio::time::timeout(std::time::Duration::from_secs(3), task).await.expect("abort timeout").expect("bash task").expect("aborted result");
    assert!(aborted.cancelled);
    assert_eq!(aborted.exit_code, None);
    assert!(matches!(session.history().last(), Some(Message::BashExecution(message)) if message.cancelled));
    registration.unregister();
}

#[tokio::test]
async fn dropped_direct_bash_aborts_and_releases_the_session_slot() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(Vec::new());
    let mut bash = Box::pin(session.execute_bash("sleep 30", false));
    tokio::select! {
        result = &mut bash => panic!("bash completed before drop: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert!(session.is_bash_running());
    drop(bash);
    assert!(!session.is_bash_running());
    let reused = session.execute_bash("printf reused", false).await.expect("reused bash");
    assert_eq!(reused.output, "reused");
    registration.unregister();
}

#[tokio::test]
async fn session_stats_aggregate_history_usage_cost_and_tool_calls() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(Vec::new());
    let tool_call = ToolCall { id: "call".into(), name: "read".into(), arguments: json!({"path":"x"}), thought_signature: None };
    let mut assistant = AssistantMessage::pending(&session.model().expect("model"));
    assistant.content = vec![ContentBlock::ToolCall(tool_call)];
    assistant.stop_reason = StopReason::ToolUse;
    assistant.usage = Usage {
        input: 10,
        output: 5,
        cache_read: 2,
        cache_write: 1,
        total_tokens: 18,
        cost: pi_ai::CostBreakdown { total: 0.25, ..Default::default() },
        ..Default::default()
    };
    session.load_history(vec![Message::user_text("question", 1), Message::Assistant(assistant)]).await.expect("load history");
    let stats = session.session_stats();
    assert_eq!(stats.user_messages, 1);
    assert_eq!(stats.assistant_messages, 1);
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.total_messages, 2);
    assert_eq!(stats.tokens.input, 10);
    assert_eq!(stats.tokens.output, 5);
    assert_eq!(stats.tokens.cache_read, 2);
    assert_eq!(stats.tokens.cache_write, 1);
    assert_eq!(stats.tokens.total, 18);
    assert_eq!(stats.cost, 0.25);
    assert!(stats.context_usage.is_some());
    registration.unregister();
}

#[tokio::test]
async fn hidden_custom_message_reaches_provider_as_user_but_stays_typed_in_history() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let contexts = Arc::new(Mutex::new(Vec::<Context>::new()));
    let captured = contexts.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, _options| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("contexts").push(context);
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let mut model = Model::default();
    model.id = "custom-provider-projection".into();
    model.name = "Custom Provider Projection".into();
    model.api = "custom-provider-projection".into();
    model.provider = "test".into();
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    }).expect("session");
    session.run_messages(vec![Message::Custom(pi_ai::CustomMessage {
        custom_type: "loop_scheduled_turn".into(),
        content: "<system-reminder>internal</system-reminder>\n\necho hello".into(),
        display: false,
        details: Some(json!({"prompt":"echo hello"})),
        timestamp: 1,
    })]).await.expect("run custom message");
    assert!(matches!(session.history().first(), Some(Message::Custom(custom)) if !custom.display));
    let contexts = contexts.lock().expect("contexts");
    assert!(matches!(contexts[0].messages.first(), Some(Message::User(user)) if matches!(&user.content[0], ContentBlock::Text { text, .. } if text.contains("system-reminder"))));
}

#[tokio::test]
async fn next_turn_custom_message_is_delivered_once_after_user_prompt() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(vec![
        FauxResponse::text("first"),
        FauxResponse::text("second"),
    ]);
    session
        .send_custom_message(
            pi_ai::CustomMessage {
                custom_type: "extension.context".into(),
                content: "queued context".into(),
                display: false,
                details: Some(json!({"source":"test"})),
                timestamp: 7,
            },
            MessageDelivery::NextTurn,
            false,
        )
        .await
        .expect("queue next-turn message");
    assert!(session.history().is_empty());

    session.run("first prompt", Vec::new()).await.expect("first run");
    session.run("second prompt", Vec::new()).await.expect("second run");
    let history = session.history();
    let user_index = history.iter().position(|message| matches!(message, Message::User(user)
        if matches!(&user.content[0], ContentBlock::Text { text, .. } if text == "first prompt")))
        .expect("first user prompt");
    let custom_index = history.iter().position(|message| matches!(message, Message::Custom(custom)
        if custom.custom_type == "extension.context" && !custom.display))
        .expect("next-turn custom message");
    assert!(user_index < custom_index);
    assert_eq!(history.iter().filter(|message| matches!(message, Message::Custom(custom)
        if custom.custom_type == "extension.context")).count(), 1);
    registration.unregister();
}

#[tokio::test]
async fn auto_retry_schedules_then_succeeds_without_duplicate_user_message() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(vec![
        FauxResponse::error("503 Service unavailable"),
        FauxResponse::text("recovered"),
    ]);
    configure_fast_retry(&session, true, 3);
    session.start_new_recording().expect("start retry recorder");
    let mut events = session.subscribe_session_events();
    let result = session.run("retry me", vec![]).await.expect("retry succeeds");
    assert_eq!(result.text, "recovered");
    assert_eq!(session.history().iter().filter(|message| matches!(message, Message::User(_))).count(), 1);
    assert_eq!(session.history().iter().filter(|message| matches!(message, Message::Assistant(_))).count(), 1);
    let recorded = session.session_entries(None).expect("retry session entries").entries;
    assert_eq!(recorded.iter().filter(|entry| matches!(&entry.message, Some(Message::User(_)))).count(), 1);
    assert_eq!(recorded.iter().filter(|entry| matches!(&entry.message, Some(Message::Assistant(_)))).count(), 2);
    let mut start = None;
    let mut end = None;
    while start.is_none() || end.is_none() {
        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await.expect("retry event timeout").expect("retry event") {
            SessionEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => {
                start = Some((attempt, max_attempts, delay_ms, error_message));
            }
            SessionEvent::AutoRetryEnd { success, attempt, final_error } => {
                end = Some((success, attempt, final_error));
            }
            _ => {}
        }
    }
    assert_eq!(start, Some((1, 3, 10, "503 Service unavailable".to_owned())));
    assert_eq!(end, Some((true, 1, None)));
    registration.unregister();
}

#[tokio::test]
async fn auto_retry_disabled_does_not_schedule_and_exhaustion_is_bounded() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (disabled, disabled_registration) = make_session(vec![FauxResponse::error("503 Service unavailable")]);
    configure_fast_retry(&disabled, true, 3);
    disabled.set_auto_retry_enabled(false);
    assert!(!disabled.auto_retry_enabled());
    let mut disabled_events = disabled.subscribe_session_events();
    disabled.run("no retry", vec![]).await.expect_err("disabled retry returns provider failure");
    while let Ok(Ok(event)) = tokio::time::timeout(std::time::Duration::from_millis(30), disabled_events.recv()).await {
        assert!(!matches!(event, SessionEvent::AutoRetryStart { .. }));
    }
    disabled_registration.unregister();

    let (exhausted, exhausted_registration) = make_session(vec![
        FauxResponse::error("503 first"),
        FauxResponse::error("503 second"),
        FauxResponse::error("503 final"),
    ]);
    configure_fast_retry(&exhausted, true, 2);
    let mut events = exhausted.subscribe_session_events();
    exhausted.run("exhaust", vec![]).await.expect_err("exhausted retries return provider failure");
    let mut starts = Vec::new();
    let mut terminal = None;
    while terminal.is_none() {
        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await.expect("retry event timeout").expect("retry event") {
            SessionEvent::AutoRetryStart { attempt, delay_ms, .. } => starts.push((attempt, delay_ms)),
            SessionEvent::AutoRetryEnd { success, attempt, final_error } if !success => terminal = Some((attempt, final_error)),
            _ => {}
        }
    }
    assert_eq!(starts, [(1, 10), (2, 20)]);
    assert_eq!(terminal, Some((2, Some("503 first | 503 second | 503 final".to_owned()))));
    exhausted_registration.unregister();
}

#[tokio::test]
async fn abort_retry_interrupts_scheduled_sleep() {
    let _guard = REGISTRY_LOCK.lock().expect("registry lock");
    let (session, registration) = make_session(vec![FauxResponse::error("503 Service unavailable")]);
    session.set_retry_settings(RetrySettings { enabled: true, max_retries: 3, base_delay_ms: 30_000 , ..Default::default() });
    let mut events = session.subscribe_session_events();
    let running = session.clone();
    let task = tokio::spawn(async move { running.run("abort retry", vec![]).await });
    loop {
        if matches!(events.recv().await.expect("retry event"), SessionEvent::AutoRetryStart { .. }) {
            break;
        }
    }
    session.abort_retry();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.expect("abort timeout").expect("retry task").expect_err("retry cancelled");
    assert!(error.to_string().contains("Retry cancelled"));
    let end = loop {
        if let SessionEvent::AutoRetryEnd { success, attempt, final_error } = events.recv().await.expect("retry end") {
            break (success, attempt, final_error);
        }
    };
    assert_eq!(end, (false, 1, Some("Retry cancelled".to_owned())));
    registration.unregister();
}

#[tokio::test]
async fn resolved_auth_stays_request_scoped_and_out_of_model_serialization() {
    let captured = Arc::new(Mutex::new(None));
    let captured_request = captured.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context: Context, options| {
        let captured = captured_request.clone();
        Box::pin(async move {
            *captured.lock().expect("capture lock") = Some((model.clone(), options));
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content = vec![ContentBlock::text("authenticated")];
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
    let resolver: pi_coding::SessionAuthResolver = Arc::new(|_model: Model| {
        Box::pin(async {
            Ok(pi_coding::RequestAuth {
                api_key: "request-api-secret".to_owned(),
                headers: std::collections::HashMap::from([
                    ("Authorization".to_owned(), "Bearer request-secret".to_owned()),
                    ("X-Probe".to_owned(), "probe-secret".to_owned()),
                ]),
                env: std::collections::HashMap::from([(
                    "REQUEST_AUTH_ENV".to_owned(),
                    "env-secret".to_owned(),
                )]),
                ..pi_coding::RequestAuth::default()
            })
        }) as pi_agent::BoxFuture<anyhow::Result<pi_coding::RequestAuth>>
    });
    let model = Model {
        id: "request-scoped-auth".into(),
        name: "Request scoped auth".into(),
        api: "unused".into(),
        provider: "custom".into(),
        ..Model::default()
    };
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model: Model::default(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(vec![]),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: Some(resolver),
    })
    .expect("session");

    session
        .set_model_with_resolved_auth(model.clone())
        .await
        .expect("set model with request auth");
    let public_model = session.model().expect("public model");
    let serialized = serde_json::to_string(&public_model).expect("serialize public model");
    let debug = format!("{public_model:?}");
    for secret in [
        "request-api-secret",
        "Bearer request-secret",
        "probe-secret",
        "env-secret",
    ] {
        assert!(!serialized.contains(secret));
        assert!(!debug.contains(secret));
    }
    assert_eq!(public_model, model);
    let application = pi_coding::Application::new(session.clone()).await;
    let application_state = application.state().await;
    let state_serialized =
        serde_json::to_string(&application_state).expect("serialize application state");
    let state_debug = format!("{application_state:?}");
    for secret in [
        "request-api-secret",
        "Bearer request-secret",
        "probe-secret",
        "env-secret",
    ] {
        assert!(!state_serialized.contains(secret));
        assert!(!state_debug.contains(secret));
    }

    session.run("authenticate", vec![]).await.expect("run");
    let (request_model, request_options) = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("captured provider request");
    assert!(request_model.headers.as_ref().is_none_or(std::collections::HashMap::is_empty));
    assert_eq!(request_options.stream.api_key.as_deref(), Some("request-api-secret"));
    assert_eq!(
        request_options.stream.headers.get("Authorization").map(String::as_str),
        Some("Bearer request-secret")
    );
    assert_eq!(
        request_options.stream.headers.get("X-Probe").map(String::as_str),
        Some("probe-secret")
    );
    assert_eq!(
        request_options.stream.env.get("REQUEST_AUTH_ENV").map(String::as_str),
        Some("env-secret")
    );
}
