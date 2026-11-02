use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Context, Message, Model, StopReason, Usage};
use pi_coding::{
    ACTIVE_GOAL_CUSTOM_TYPE, Application, ApplicationEvent, ApplicationRuntimeCandidate,
    GoalActivationOutcome, GoalContinuationDecision, GoalLifecycle, GoalPauseReason,
    GoalUsageDelta, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, ToolSelection,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

struct HermeticGoalFixture {
    workspace: tempfile::TempDir,
    _agent_dir: tempfile::TempDir,
    session_dir: tempfile::TempDir,
    resources: ResourceManager,
    session: Session,
    registration: FauxProviderRegistration,
}

impl HermeticGoalFixture {
    fn new(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("fixture workspace");
        let agent_dir = tempfile::tempdir().expect("fixture agent dir");
        let session_dir = tempfile::tempdir().expect("fixture session dir");
        std::fs::write(agent_dir.path().join("settings.json"), DANGER_SETTINGS)
            .expect("fixture danger settings");

        let suffix = uuid::Uuid::now_v7();
        let model = Model {
            id: format!("{label}-model-{suffix}"),
            name: format!("{label} Model"),
            api: format!("{label}-api-{suffix}"),
            provider: format!("{label}-provider-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 64,
        });
        registration.set_responses(responses);

        let mut resource_options = ResourceManagerOptions::new(workspace.path());
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = ResourceManager::new(resource_options).expect("fixture resources");

        let session = Session::new_with_additional_tools_filtered_and_discovery(
            SessionOptions {
                model,
                cwd: workspace.path().to_path_buf(),
                system_prompt: resources
                    .snapshot()
                    .system_prompt
                    .clone()
                    .unwrap_or_default(),
                thinking_level: pi_agent::ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("fixture session");
        session.set_session_dir(session_dir.path().to_path_buf());

        Self {
            workspace,
            _agent_dir: agent_dir,
            session_dir,
            resources,
            session,
            registration,
        }
    }

    fn session(&self) -> Session {
        self.session.clone()
    }

    fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    fn session_dir(&self) -> &Path {
        self.session_dir.path()
    }

    fn model_key(&self) -> (String, String) {
        let model = self.session.model().expect("fixture model");
        (model.provider, model.id)
    }

    fn start_recording(&self, session_id: &str) -> PathBuf {
        let recorder = pi_coding::start_session_in(
            self.workspace(),
            self.session.model().as_ref(),
            Some("off"),
            Some(self.session_dir()),
            Some(session_id),
            None,
        )
        .expect("fixture recorder");
        let path = recorder.path();
        self.session
            .record(recorder)
            .expect("attach fixture recorder");
        path
    }

    async fn application(&self) -> Application {
        self.session
            .attach_resources(self.resources.clone())
            .await
            .expect("attach fixture resources");
        let application =
            Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(self.session()))
                .await
                .expect("publish application generation");
        assert_eq!(
            application.resource_generation(),
            Some(1),
            "published fixture generation"
        );
        application
    }
}

impl Drop for HermeticGoalFixture {
    fn drop(&mut self) {
        self.registration.unregister();
        pi_ai::replace_registered_models(&[self.model_key()], Vec::new());
    }
}

fn usage_fixture(usage: Usage) -> HermeticGoalFixture {
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
    HermeticGoalFixture::new("application-goal", Vec::new(), Some(stream_fn))
}

/// Fake provider session whose `block_call`-th model call blocks until the
/// given token is cancelled, so a specific turn's completion (and its usage
/// charge) can be held at a deterministic point. Every reply carries a fixed
/// 3-token usage so completed turns always commit a usage update.
fn controlled_usage_fixture(
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: CancellationToken,
    block_call: usize,
) -> HermeticGoalFixture {
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
        let started = started.clone();
        let release = release.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                if call == block_call {
                    release.cancelled().await;
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.usage = Usage {
                    input: 2,
                    output: 1,
                    total_tokens: 3,
                    ..Usage::default()
                };
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    HermeticGoalFixture::new("goal-controlled", Vec::new(), Some(stream_fn))
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

async fn wait_for_matching_turn_terminal(
    events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut started = None::<String>;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "goal work never published TurnStarted+TurnTerminal"
        );
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(ApplicationEvent::TurnStarted { turn_id, .. })) => {
                started = Some(turn_id);
            }
            Ok(Ok(ApplicationEvent::TurnTerminal { turn_id, .. })) => {
                let expected = started.expect("TurnTerminal without TurnStarted");
                assert_eq!(turn_id, expected);
                return turn_id;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => panic!("goal work event channel closed"),
            Err(_) => panic!("goal work never published TurnStarted+TurnTerminal"),
        }
    }
}

#[tokio::test]
async fn goal_activation_starts_a_hidden_model_turn_and_charges_usage() {
    let usage = Usage {
        input: 2,
        output: 1,
        total_tokens: 3,
        ..Usage::default()
    };
    let fixture = usage_fixture(usage);
    let session = fixture.session();
    let application = fixture.application().await;
    let mut events = application.subscribe();

    assert_eq!(
        application
            .activate_goal("start visible work", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_matching_turn_terminal(&mut events).await;
    application.wait_for_idle().await;
    assert!(
        !application.is_streaming(),
        "goal work must emit TurnTerminal so is_streaming is false after idle"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 3);
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert!(
        session
            .history()
            .iter()
            .any(|message| matches!(message, Message::Assistant(_)))
    );
    assert!(
        !session
            .history()
            .iter()
            .any(|message| matches!(message, Message::User(_)))
    );
    let transcript = serde_json::to_string(&application.messages()).expect("transcript");
    assert_no_active_goal_projection(&transcript);
}

#[tokio::test]
async fn failed_goal_activation_can_be_retried_once() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = capturing_fixture(contexts.clone(), 0);
    fixture.registration.set_responses(vec![
        FauxResponse::error("first activation failed"),
        FauxResponse::text("recovered"),
    ]);
    fixture.session().set_auto_retry_enabled(false);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("retry failed work", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;
    assert_eq!(
        application.resume_goal_work().await.expect("retry"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;

    assert!(contexts.lock().expect("contexts").len() >= 2);
    assert!(
        application
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::Assistant(_)))
            .count()
            >= 2
    );
}

#[tokio::test]
async fn goal_resume_starts_exactly_once_and_pause_cancels_queued_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_second = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release_second.clone(), 2);
    let application = fixture.application().await;
    application
        .goal_create("resume exactly once", Some(100))
        .expect("create metadata");
    application.goal_pause().expect("pause");

    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("duplicate resume"),
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
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "queued paused work must not start a second provider call"
    );
}

#[tokio::test]
async fn goal_resume_survives_prior_turn_usage_charge_race() {
    // Regression for the backend /goal resume race (Web loop_goal E2E): the
    // activate-created work turn is still in flight when the user pauses and
    // resumes. The resume commits `Resumed` (revision R, Active) and its
    // continuation is queued behind the in-flight turn. When that prior turn
    // finally completes it charges usage, advancing the revision to R+1
    // WITHOUT touching the lifecycle. The queued continuation must still run
    // exactly once (usage-only revision bumps must not cancel it), and the
    // usage charge must land on the resumed active lifecycle — never on a
    // stale paused/active snapshot. Before the fix the continuation was
    // cancelled by the revision-mismatch stale check, so the second provider
    // call never happened.
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release_first.clone(), 1);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("race the resume", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 1).await;
    application.goal_pause().expect("pause");

    // The resume commits Active and queues exactly one continuation behind
    // the in-flight prior turn. It must succeed — never fail with
    // "goal is not active" — because the schedule is keyed to the revision
    // the resume itself committed.
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("resume must succeed"),
        GoalActivationOutcome::Queued
    );

    // Prior turn completes and charges usage: revision advances, lifecycle
    // stays active. The queued continuation must run exactly once.
    release_first.cancel();
    application.wait_for_idle().await;

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "resume must schedule exactly one continuation"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert_eq!(goal.pause_reason, None);
    assert_eq!(
        goal.usage.tokens_used, 6,
        "both turns' usage charged against the resumed active goal"
    );
}

/// Regression (reviewer P2): a queued activation/resume continuation must not
/// survive a pause followed by a MUTATION-ONLY resume (GoalPanel / RPC
/// `activate:false`). Any lifecycle-revision-changing command invalidates the
/// older activation — the mutation-only resume returning the goal to Active
/// must not resurrect it. Deterministic: the busy prompt holds the turn gate,
/// the resume queues behind it, and the pause + mutation-only resume land
/// before the gate frees, so the queued continuation's wake check sees the
/// newer lifecycle epoch and cancels itself.
#[tokio::test]
async fn pause_then_mutation_only_resume_does_not_resurrect_queued_activation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_prompt = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release_prompt.clone(), 1);
    let application = fixture.application().await;
    application
        .goal_create("panel goal", Some(100))
        .expect("create metadata");
    application.goal_pause().expect("pause");

    // The busy prompt holds the turn gate so the resume queues behind it.
    application
        .prompt("busy turn".to_owned(), Vec::new(), None)
        .await
        .expect("busy prompt");
    wait_for_calls(&calls, &started, 1).await;
    assert_eq!(
        application.resume_goal_work().await.expect("queued resume"),
        GoalActivationOutcome::Queued
    );
    // Pause invalidates the queued reservation…
    application
        .goal_pause()
        .expect("pause invalidates queued work");
    // …and the mutation-only resume (GoalPanel / RPC activate:false) returns
    // the goal to Active WITHOUT scheduling; it must not resurrect the
    // invalidated reservation.
    application.goal_resume().expect("mutation-only resume");

    release_prompt.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "mutation-only resume must not resurrect the paused queued activation"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
}

/// A newer resume supersedes an older queued continuation: the older
/// run_goal_work cancels itself on wake and only the newest revision's
/// continuation runs exactly one model call. Deterministic via the release
/// token + turn-gate ordering, no sleeps.
#[tokio::test]
async fn newer_resume_supersedes_queued_continuation_and_only_newest_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release_first.clone(), 1);
    let session = fixture.session();
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("supersede me", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 1).await;
    application.goal_pause().expect("pause");

    // First resume queues a continuation behind the in-flight activate turn.
    assert_eq!(
        application.resume_goal_work().await.expect("first resume"),
        GoalActivationOutcome::Queued
    );
    // Pause then resume again: the second continuation supersedes the first.
    application.goal_pause().expect("pause again");
    assert_eq!(
        application.resume_goal_work().await.expect("newer resume"),
        GoalActivationOutcome::Queued
    );

    release_first.cancel();
    application.wait_for_idle().await;

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "activate turn plus exactly one newest continuation must run"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    // The continuation that ran is keyed to the newest resume revision; the
    // superseded first-resume continuation never delivered a message.
    let continuations: Vec<u64> = session
        .history()
        .iter()
        .filter_map(|message| match message {
            Message::Custom(message) if message.custom_type == "pi.goal.continue" => {
                message.details.as_ref()?.get("revision")?.as_u64()
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        continuations.len(),
        2,
        "exactly the activate turn plus the newest continuation deliver messages: {continuations:?}"
    );
    assert_eq!(
        *continuations.iter().min().expect("activate revision"),
        1,
        "activate message keys the create revision: {continuations:?}"
    );
    assert!(
        *continuations.iter().max().expect("newest revision") > 1,
        "the delivered continuation must be keyed to the newest resume revision, got {continuations:?}"
    );
}

/// Inverse stale-start interleaving: the older continuation's wake check
/// passes and its model call starts before a newer pause/resume lands. The
/// in-flight call cannot be cancelled, but the newest continuation must still
/// run exactly once afterwards. Deterministic via the release token +
/// turn-gate ordering, no sleeps.
#[tokio::test]
async fn stale_continuation_already_started_then_superseded_still_runs_newest_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_old = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release_old.clone(), 2);
    let session = fixture.session();
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("stale start", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;
    application.goal_pause().expect("pause");

    // The resume starts immediately (gate free) and blocks at call 2: the
    // older continuation is now stale-starting.
    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;

    // While the stale continuation is in flight, pause + newer resume.
    application
        .goal_pause()
        .expect("pause while stale turn runs");
    assert_eq!(
        application.resume_goal_work().await.expect("newer resume"),
        GoalActivationOutcome::Queued
    );

    release_old.cancel();
    application.wait_for_idle().await;

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the already-started stale continuation plus exactly one newest continuation must run"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    let continuations: Vec<u64> = session
        .history()
        .iter()
        .filter_map(|message| match message {
            Message::Custom(message) if message.custom_type == "pi.goal.continue" => {
                message.details.as_ref()?.get("revision")?.as_u64()
            }
            _ => None,
        })
        .collect();
    assert!(
        *continuations.iter().max().expect("newest revision") >= 3,
        "the newest continuation must be keyed to a resume revision, got {continuations:?}"
    );
}

fn capturing_fixture(
    contexts: Arc<Mutex<Vec<Context>>>,
    response_count: usize,
) -> HermeticGoalFixture {
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts.lock().expect("contexts").push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });
    HermeticGoalFixture::new(
        "goal-context",
        std::iter::repeat_with(|| FauxResponse::text("done"))
            .take(response_count)
            .collect(),
        Some(stream_fn),
    )
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
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = capturing_fixture(contexts.clone(), 1);
    let session_path = fixture.start_recording("goal-model-context");
    let application = fixture.application().await;

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
    assert!(
        provider_text.contains("revision 1, lifecycle active"),
        "{provider_text}"
    );
    assert!(
        provider_text.contains("Objective: ship &lt;safely&gt; &amp; preserve context"),
        "{provider_text}"
    );
    assert!(
        provider_text.contains("Token budget: 0/100."),
        "{provider_text}"
    );
    drop(contexts);

    let transcript = serde_json::to_string(&application.messages()).expect("transcript JSON");
    assert_no_active_goal_projection(&transcript);
    let tree = serde_json::to_string(&application.session_tree().expect("session tree"))
        .expect("tree JSON");
    assert_no_active_goal_projection(&tree);
    let persisted = std::fs::read_to_string(&session_path).expect("session JSONL");
    assert_no_active_goal_projection(&persisted);

    let html_path = fixture.workspace().join("goal.html");
    application
        .export_html(Some(&html_path))
        .expect("HTML export");
    assert_no_active_goal_projection(&std::fs::read_to_string(html_path).expect("HTML"));
    let jsonl_path = fixture.workspace().join("goal-export.jsonl");
    application
        .export_jsonl(Some(&jsonl_path))
        .expect("JSONL export");
    assert_no_active_goal_projection(&std::fs::read_to_string(jsonl_path).expect("JSONL"));
}

#[tokio::test]
async fn goal_pins_are_projected_into_the_goal_turn_context() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = capturing_fixture(contexts.clone(), 1);
    let application = fixture.application().await;
    let session = application.session();
    let runtime = session.goal_runtime();
    runtime
        .create("pinned change", Some(100))
        .expect("create goal");
    runtime
        .pin("keep <safely> & in scope")
        .expect("pin with escapes");
    runtime
        .pin("follow the omp skill-card style")
        .expect("second pin");
    session.run("work", Vec::new()).await.expect("pinned turn");

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 1);
    let provider_text = context_text(&contexts[0]);
    assert!(
        provider_text.contains("Role-model pins:"),
        "pins must be projected into the goal turn context: {provider_text}"
    );
    assert!(
        provider_text.contains("1. keep &lt;safely&gt; &amp; in scope"),
        "pins must be XML-escaped like the objective: {provider_text}"
    );
    assert!(
        provider_text.contains("2. follow the omp skill-card style"),
        "numbered pin listing: {provider_text}"
    );
    assert_eq!(provider_text.matches("Role-model pins:").count(), 1);
    drop(contexts);
}

#[tokio::test]
async fn goal_projection_refreshes_lifecycle_revision_and_usage_on_each_provider_call() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = capturing_fixture(contexts.clone(), 3);
    let application = fixture.application().await;
    let session = application.session();
    let runtime = session.goal_runtime();
    runtime
        .create("finish the focused change", Some(20))
        .expect("create");
    session
        .run("active", Vec::new())
        .await
        .expect("active turn");

    runtime
        .update_usage(GoalUsageDelta::new(3, 7))
        .expect("charge usage");
    runtime.pause().expect("pause");
    session
        .run("paused", Vec::new())
        .await
        .expect("paused turn");

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
    assert!(
        completed.contains("revision 4, lifecycle completed"),
        "{completed}"
    );
    assert!(completed.contains("Token budget: 3/20."), "{completed}");
    drop(contexts);
    assert_no_active_goal_projection(
        &serde_json::to_string(&session.history()).expect("history JSON"),
    );
}

#[tokio::test]
async fn absent_and_dropped_goals_emit_no_provider_projection() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = capturing_fixture(contexts.clone(), 2);
    let application = fixture.application().await;
    let session = application.session();
    session
        .run("absent", Vec::new())
        .await
        .expect("absent turn");
    let runtime = session.goal_runtime();
    runtime.create("discard this goal", None).expect("create");
    runtime.drop().expect("drop");
    session
        .run("dropped", Vec::new())
        .await
        .expect("dropped turn");

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 2);
    assert_no_active_goal_projection(&context_text(&contexts[0]));
    assert_no_active_goal_projection(&context_text(&contexts[1]));
}

#[tokio::test]
async fn application_charges_post_turn_usage_once_and_pauses_at_budget() {
    let fixture = usage_fixture(Usage {
        input: 7,
        output: 3,
        cache_read: 2,
        cache_write: 1,
        total_tokens: 13,
        ..Usage::default()
    });
    fixture.start_recording("application-goal-usage");
    let application = fixture.application().await;
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
    let source_fixture = usage_fixture(Usage::default());
    let source_path = source_fixture.start_recording("goal-source");
    let source = source_fixture.application().await;
    let original = source
        .goal_create("preserve lineage", Some(50))
        .expect("goal");

    let resumed_fixture = usage_fixture(Usage::default());
    resumed_fixture
        .session()
        .record(pi_coding::resume_session(&source_path).expect("resume recorder"))
        .expect("attach resumed recorder");
    let resumed = resumed_fixture.application().await;
    resumed.prepare_resumed_goal(false).expect("resume safety");
    let resumed_goal = resumed.goal_state().current.expect("resumed goal");
    assert_eq!(resumed_goal.id, original.id);
    assert_eq!(resumed_goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(
        resumed_goal.pause_reason,
        Some(GoalPauseReason::ResumeSafety)
    );

    let fork_fixture = usage_fixture(Usage::default());
    let fork_recorder = pi_coding::fork_session_in(
        &source_path,
        fork_fixture.workspace(),
        Some(fork_fixture.session_dir()),
        Some("goal-fork"),
    )
    .expect("fork recorder");
    fork_fixture
        .session()
        .record(fork_recorder)
        .expect("attach fork recorder");
    let fork = fork_fixture.application().await;
    fork.prepare_resumed_goal(true).expect("fork goal");
    let forked = fork.goal_state().current.expect("forked goal");
    assert_ne!(forked.id, original.id);
    assert_eq!(forked.origin_goal_id.as_deref(), Some(original.id.as_str()));
    assert_eq!(forked.objective, original.objective);
    assert_eq!(forked.token_budget, original.token_budget);
    assert_eq!(forked.lifecycle, GoalLifecycle::Paused);
    assert_eq!(forked.pause_reason, Some(GoalPauseReason::ResumeSafety));
}
