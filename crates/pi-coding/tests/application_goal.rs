use std::sync::{Arc, LazyLock, Mutex, atomic::{AtomicUsize, Ordering}};

use pi_ai::providers::{FauxProviderOptions, FauxResponse, FauxProviderRegistration, register_faux_provider};
use pi_ai::{ContentBlock, Context, Message, Model, StopReason, Usage};
use pi_coding::{
    ACTIVE_GOAL_CUSTOM_TYPE, Application, GoalActivationOutcome, GoalContinuationDecision,
    GoalLifecycle, GoalPauseReason, GoalUsageDelta, Session, SessionOptions,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

static FAUX_REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn usage_session(usage: Usage) -> Session {
    let model = Model {
        id: "application-goal-model".to_owned(),
        name: "Application Goal Model".to_owned(),
        api: "application-goal-api".to_owned(),
        provider: "application-goal-provider".to_owned(),
        ..Model::default()
    };
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let usage = usage.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.usage = usage;
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("session")
}

fn controlled_usage_session(
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release_second: CancellationToken,
) -> Session {
    let suffix = uuid::Uuid::now_v7();
    let model = Model {
        id: format!("goal-controlled-model-{suffix}"),
        name: "Goal Controlled Model".to_owned(),
        api: format!("goal-controlled-api-{suffix}"),
        provider: format!("goal-controlled-provider-{suffix}"),
        ..Model::default()
    };
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
        let started = started.clone();
        let release_second = release_second.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                if call == 2 {
                    release_second.cancelled().await;
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("controlled usage session")
}

async fn wait_for_calls(calls: &AtomicUsize, started: &Notify, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let notified = started.notified();
            if calls.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("provider call started");
}

#[tokio::test]
async fn goal_activation_starts_a_hidden_model_turn_and_charges_usage() {
    let usage = Usage {
        input: 2,
        output: 1,
        total_tokens: 3,
        ..Usage::default()
    };
    let session = usage_session(usage);
    let application = Application::new(session.clone()).await;

    assert_eq!(
        application.activate_goal("start visible work", Some(100)).await.expect("activate"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 3);
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert!(session.history().iter().any(|message| matches!(message, Message::Assistant(_))));
    assert!(!session.history().iter().any(|message| matches!(message, Message::User(_))));
    let transcript = serde_json::to_string(&application.messages()).expect("transcript");
    assert_no_active_goal_projection(&transcript);
}

#[tokio::test]
async fn failed_goal_activation_can_be_retried_once() {
    let directory = tempfile::tempdir().expect("cwd");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let (session, registration) = capturing_session(directory.path(), contexts.clone(), 0);
    registration.set_responses(vec![
        FauxResponse::error("first activation failed"),
        FauxResponse::text("recovered"),
    ]);
    session.set_auto_retry_enabled(false);
    let application = Application::new(session).await;

    assert_eq!(
        application.activate_goal("retry failed work", Some(100)).await.expect("activate"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;
    assert_eq!(
        application.resume_goal_work().await.expect("retry"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;

    assert!(contexts.lock().expect("contexts").len() >= 2);
    assert!(application.messages().iter().filter(|message| matches!(message, Message::Assistant(_))).count() >= 2);
    registration.unregister();
}

#[tokio::test]
async fn goal_resume_starts_exactly_once_and_pause_cancels_queued_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_second = CancellationToken::new();
    let application = Application::new(controlled_usage_session(
        calls.clone(),
        started.clone(),
        release_second.clone(),
    ))
    .await;
    application.goal_create("resume exactly once", Some(100)).expect("create metadata");
    application.goal_pause().expect("pause");

    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );
    assert_eq!(
        application.resume_goal_work().await.expect("duplicate resume"),
        GoalActivationOutcome::AlreadyActive
    );
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    application.goal_pause().expect("pause again");
    application
        .prompt("busy turn".to_owned(), Vec::new(), None)
        .await
        .expect("busy prompt");
    wait_for_calls(&calls, &started, 2).await;
    assert_eq!(
        application.resume_goal_work().await.expect("queued resume"),
        GoalActivationOutcome::Queued
    );
    application.goal_pause().expect("cancel queued goal work");
    release_second.cancel();
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "queued paused work must not start a second provider call");
}

fn capturing_session(
    cwd: &std::path::Path,
    contexts: Arc<Mutex<Vec<Context>>>,
    response_count: usize,
) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7();
    let model = Model {
        id: format!("goal-context-model-{suffix}"),
        name: "Goal Context Model".to_owned(),
        api: format!("goal-context-api-{suffix}"),
        provider: format!("goal-context-provider-{suffix}"),
        base_url: "http://localhost:0".to_owned(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 64,
    });
    registration.set_responses(
        std::iter::repeat_with(|| FauxResponse::text("done"))
            .take(response_count)
            .collect(),
    );
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts.lock().expect("contexts").push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("capturing session");
    (session, registration)
}

fn context_text(context: &Context) -> String {
    context
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => message.content.iter(),
            Message::Assistant(message) => message.content.iter(),
            Message::ToolResult(message) => message.content.iter(),
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                panic!("provider context contains an unprojected session message")
            }
        })
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_active_goal_projection(text: &str) {
    assert!(!text.contains(ACTIVE_GOAL_CUSTOM_TYPE), "{text}");
    assert!(!text.contains("Active session goal ("), "{text}");
    assert!(!text.contains("<system-reminder>"), "{text}");
}

#[tokio::test]
async fn active_goal_is_projected_once_to_provider_but_never_persisted_or_displayed() {
    let _registry_guard = FAUX_REGISTRY_LOCK.lock().expect("faux registry lock");
    let directory = tempfile::tempdir().expect("session dir");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let (session, registration) = capturing_session(directory.path(), contexts.clone(), 1);
    let recorder = pi_coding::start_session_in(
        directory.path(),
        session.model().as_ref(),
        Some("off"),
        Some(directory.path()),
        Some("goal-model-context"),
        None,
    )
    .expect("recorder");
    let session_path = recorder.path();
    session.record(recorder).expect("attach recorder");
    let application = Application::new(session).await;

    application
        .goal_create("ship <safely> & preserve context", Some(100))
        .expect("create goal");
    application
        .prompt("work".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 1);
    let provider_text = context_text(&contexts[0]);
    assert_eq!(provider_text.matches("<system-reminder>").count(), 1);
    assert_eq!(provider_text.matches("Active session goal (").count(), 1);
    assert!(provider_text.contains("revision 1, lifecycle active"), "{provider_text}");
    assert!(provider_text.contains("Objective: ship &lt;safely&gt; &amp; preserve context"), "{provider_text}");
    assert!(provider_text.contains("Token budget: 0/100."), "{provider_text}");
    drop(contexts);

    let transcript = serde_json::to_string(&application.messages()).expect("transcript JSON");
    assert_no_active_goal_projection(&transcript);
    let tree = serde_json::to_string(&application.session_tree().expect("session tree"))
        .expect("tree JSON");
    assert_no_active_goal_projection(&tree);
    let persisted = std::fs::read_to_string(&session_path).expect("session JSONL");
    assert_no_active_goal_projection(&persisted);

    let html_path = directory.path().join("goal.html");
    application.export_html(Some(&html_path)).expect("HTML export");
    assert_no_active_goal_projection(&std::fs::read_to_string(html_path).expect("HTML"));
    let jsonl_path = directory.path().join("goal-export.jsonl");
    application.export_jsonl(Some(&jsonl_path)).expect("JSONL export");
    assert_no_active_goal_projection(&std::fs::read_to_string(jsonl_path).expect("JSONL"));
    registration.unregister();
}

#[tokio::test]
async fn goal_projection_refreshes_lifecycle_revision_and_usage_on_each_provider_call() {
    let _registry_guard = FAUX_REGISTRY_LOCK.lock().expect("faux registry lock");
    let directory = tempfile::tempdir().expect("cwd");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let (session, registration) = capturing_session(directory.path(), contexts.clone(), 3);
    let runtime = session.goal_runtime();
    runtime.create("finish the focused change", Some(20)).expect("create");
    session.run("active", Vec::new()).await.expect("active turn");

    runtime
        .update_usage(GoalUsageDelta::new(3, 7))
        .expect("charge usage");
    runtime.pause().expect("pause");
    session.run("paused", Vec::new()).await.expect("paused turn");

    runtime.complete().expect("complete");
    session
        .run("completed", Vec::new())
        .await
        .expect("completed turn");

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 3);
    let active = context_text(&contexts[0]);
    let paused = context_text(&contexts[1]);
    let completed = context_text(&contexts[2]);
    for text in [&active, &paused, &completed] {
        assert_eq!(text.matches("Active session goal (").count(), 1, "{text}");
    }
    assert!(active.contains("revision 1, lifecycle active"), "{active}");
    assert!(active.contains("Token budget: 0/20."), "{active}");
    assert!(paused.contains("revision 3, lifecycle paused"), "{paused}");
    assert!(paused.contains("Token budget: 3/20."), "{paused}");
    assert!(completed.contains("revision 4, lifecycle completed"), "{completed}");
    assert!(completed.contains("Token budget: 3/20."), "{completed}");
    drop(contexts);
    assert_no_active_goal_projection(
        &serde_json::to_string(&session.history()).expect("history JSON"),
    );
    registration.unregister();
}

#[tokio::test]
async fn absent_and_dropped_goals_emit_no_provider_projection() {
    let _registry_guard = FAUX_REGISTRY_LOCK.lock().expect("faux registry lock");
    let directory = tempfile::tempdir().expect("cwd");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let (session, registration) = capturing_session(directory.path(), contexts.clone(), 2);
    session.run("absent", Vec::new()).await.expect("absent turn");
    let runtime = session.goal_runtime();
    runtime.create("discard this goal", None).expect("create");
    runtime.drop().expect("drop");
    session.run("dropped", Vec::new()).await.expect("dropped turn");

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 2);
    assert_no_active_goal_projection(&context_text(&contexts[0]));
    assert_no_active_goal_projection(&context_text(&contexts[1]));
    registration.unregister();
}

#[tokio::test]
async fn application_charges_post_turn_usage_once_and_pauses_at_budget() {
    let directory = tempfile::tempdir().expect("session dir");
    let session = usage_session(Usage {
        input: 7,
        output: 3,
        cache_read: 2,
        cache_write: 1,
        total_tokens: 13,
        ..Usage::default()
    });
    session
        .record(
            pi_coding::start_session_in(
                std::env::current_dir().expect("cwd"),
                session.model().as_ref(),
                Some("off"),
                Some(directory.path()),
                Some("application-goal-usage"),
                None,
            )
            .expect("recorder"),
        )
        .expect("attach recorder");
    let application = Application::new(session).await;
    application
        .goal_create("finish within budget", Some(13))
        .expect("create goal");
    application
        .prompt("work".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    let state = application.goal_state();
    let goal = state.current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 13);
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
    assert_eq!(
        application.goal_continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: goal.id,
            reason: GoalPauseReason::BudgetExhausted,
            revision: state.revision,
        }
    );
}

#[tokio::test]
async fn application_resume_safety_and_fork_lineage_rebuild_goal_runtime() {
    let directory = tempfile::tempdir().expect("session dir");
    let first = usage_session(Usage::default());
    let first_recorder = pi_coding::start_session_in(
        std::env::current_dir().expect("cwd"),
        first.model().as_ref(),
        Some("off"),
        Some(directory.path()),
        Some("goal-source"),
        None,
    )
    .expect("source recorder");
    let source_path = first_recorder.path();
    first.record(first_recorder).expect("attach source recorder");
    let source = Application::new(first).await;
    let original = source.goal_create("preserve lineage", Some(50)).expect("goal");

    let resumed_session = usage_session(Usage::default());
    resumed_session
        .record(pi_coding::resume_session(&source_path).expect("resume recorder"))
        .expect("attach resumed recorder");
    let resumed = Application::new(resumed_session).await;
    resumed.prepare_resumed_goal(false).expect("resume safety");
    let resumed_goal = resumed.goal_state().current.expect("resumed goal");
    assert_eq!(resumed_goal.id, original.id);
    assert_eq!(resumed_goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(resumed_goal.pause_reason, Some(GoalPauseReason::ResumeSafety));

    let fork_recorder = pi_coding::fork_session_in(
        &source_path,
        std::env::current_dir().expect("cwd"),
        Some(directory.path()),
        Some("goal-fork"),
    )
    .expect("fork recorder");
    let fork = usage_session(Usage::default());
    fork.record(fork_recorder).expect("attach fork recorder");
    let fork = Application::new(fork).await;
    fork.prepare_resumed_goal(true).expect("fork goal");
    let forked = fork.goal_state().current.expect("forked goal");
    assert_ne!(forked.id, original.id);
    assert_eq!(forked.origin_goal_id.as_deref(), Some(original.id.as_str()));
    assert_eq!(forked.objective, original.objective);
    assert_eq!(forked.token_budget, original.token_budget);
    assert_eq!(forked.lifecycle, GoalLifecycle::Paused);
    assert_eq!(forked.pause_reason, Some(GoalPauseReason::ResumeSafety));
}
