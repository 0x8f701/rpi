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
    CompactionReason, CompactionSettings, GoalActivationOutcome, GoalContinuationDecision,
    GoalLifecycle, GoalPauseReason, GoalUsageDelta, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionEvent, SessionOptions, ToolSelection,
    TurnTerminalStatus,
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
        Self::new_with_context_window(
            label,
            responses,
            stream_fn,
            Model::default().context_window,
        )
    }

    fn new_with_context_window(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
        context_window: i64,
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
            context_window,
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
                model: Some(model),
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

/// Controlled model stream: every `blocks` entry holds the Nth call until
/// that entry's token is cancelled, the `fail_call`-th call resolves with
/// `StopReason::Error`, and an optional capture records each request's
/// context. Every reply carries a fixed 3-token usage so completed turns
/// always commit a usage update.
fn controlled_stream_fn(
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    blocks: Vec<(usize, CancellationToken)>,
    fail_call: usize,
    contexts: Option<Arc<Mutex<Vec<Context>>>>,
) -> pi_agent::StreamFn {
    let blocks = Arc::new(blocks);
    Arc::new(move |model, context, options| {
        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
        let started = started.clone();
        let blocks = blocks.clone();
        let contexts = contexts.clone();
        let abort = options.stream.abort_signal.clone();
        Box::pin(async move {
            if let Some(contexts) = contexts.as_ref() {
                contexts.lock().expect("contexts").push(context.clone());
            }
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                let mut aborted = false;
                for (blocked_call, release) in blocks.iter() {
                    if call != *blocked_call {
                        continue;
                    }
                    // Abort wins when both are already ready: a later
                    // release.cancel() is only cleanup and must not turn an
                    // aborted call into Stop.
                    aborted = match abort.clone() {
                        Some(abort) => {
                            tokio::select! {
                                biased;
                                _ = abort.cancelled() => true,
                                _ = release.cancelled() => false,
                            }
                        }
                        None => {
                            release.cancelled().await;
                            false
                        }
                    };
                    break;
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                if aborted {
                    // No content was produced, so the aborted call carries
                    // no usage. A released call still charges the fixed 3.
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("Request was aborted".to_owned());
                } else {
                    message.content.push(ContentBlock::text("done"));
                    message.usage = Usage {
                        input: 2,
                        output: 1,
                        total_tokens: 3,
                        ..Usage::default()
                    };
                    if call == fail_call {
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some("goal turn provider failure".to_owned());
                    } else {
                        message.stop_reason = StopReason::Stop;
                    }
                }
                producer.end(Some(message)).await;
            });
            stream
        })
    })
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
    controlled_multi_block_fixture(calls, started, vec![(block_call, release)])
}

/// Same controlled session, but several calls block on independent tokens,
/// so two different rounds (e.g. a goal continuation and a user prompt) can
/// be held at deterministic points in one test.
fn controlled_multi_block_fixture(
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    blocks: Vec<(usize, CancellationToken)>,
) -> HermeticGoalFixture {
    HermeticGoalFixture::new(
        "goal-controlled",
        Vec::new(),
        Some(controlled_stream_fn(calls, started, blocks, 0, None)),
    )
}

/// Same controlled session, but the `fail_call`-th model call resolves with
/// `StopReason::Error` while still carrying the usual 3-token charge, so a
/// specific continuation round can be made fatal.
fn controlled_error_fixture(
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: CancellationToken,
    fail_call: usize,
) -> HermeticGoalFixture {
    HermeticGoalFixture::new(
        "goal-controlled",
        Vec::new(),
        Some(controlled_stream_fn(calls, started, Vec::new(), fail_call, None)),
    )
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

/// Waits until a `TurnTerminal` with `TurnTerminalStatus::Failed` arrives on
/// the subscription, bounded by a deadline; used where the failed round's
/// `turn_id` is not pre-known.
async fn wait_for_failed_turn_terminal(
    events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "goal work never published a failed TurnTerminal"
        );
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(ApplicationEvent::TurnTerminal {
                turn_id,
                status: TurnTerminalStatus::Failed,
                ..
            })) => return turn_id,
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => panic!("goal work event channel closed"),
            Err(_) => panic!("goal work never published a failed TurnTerminal"),
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
            // 3-token provider: a 3-token budget admits exactly one round,
            // then the usage charge pauses the goal and the owner exits.
            .activate_goal("start visible work", Some(3))
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
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
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
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_retry = CancellationToken::new();
    // Call 1 fails fatally (StopReason::Error); call 2 — the manual retry —
    // blocks so the test can end the retried owner with an explicit pause
    // instead of leaning on faux-response exhaustion.
    let fixture = HermeticGoalFixture::new(
        "goal-controlled",
        Vec::new(),
        Some(controlled_stream_fn(
            calls.clone(),
            started.clone(),
            vec![(2, release_retry.clone())],
            1,
            None,
        )),
    );
    fixture.session().set_auto_retry_enabled(false);
    let application = fixture.application().await;
    let mut events = application.subscribe();

    assert_eq!(
        application
            .activate_goal("retry failed work", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_failed_turn_terminal(&mut events).await;
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        application.resume_goal_work().await.expect("retry"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application
        .goal_pause()
        .expect("pause ends the retried owner");
    release_retry.cancel();
    application.wait_for_idle().await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        application
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::Assistant(_)))
            .count()
            >= 2,
        "the failed round and the retried round each leave an assistant message"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::Manual));
}

#[tokio::test]
async fn goal_resume_starts_exactly_once_and_pause_cancels_queued_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let release_prompt = CancellationToken::new();
    // Call 1 is the resumed continuation; call 2 is the busy prompt. Both
    // block so each phase can pin the turn gate at a deterministic point.
    let fixture = controlled_multi_block_fixture(
        calls.clone(),
        started.clone(),
        vec![(1, release_first.clone()), (2, release_prompt.clone())],
    );
    let application = fixture.application().await;
    application
        .goal_create("resume exactly once", Some(100))
        .expect("create metadata");
    application.goal_pause().expect("pause");

    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );
    // Determinism: round 1 must be blocked in flight before the pause below
    // can invalidate it (the spawned worker otherwise races the pause).
    wait_for_calls(&calls, &started, 1).await;
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("duplicate resume"),
        GoalActivationOutcome::AlreadyActive
    );
    // A pause while the continuation round is in flight ends the owner after
    // that round: exactly one goal provider call ever ran.
    application.goal_pause().expect("pause again");
    release_first.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the resumed owner must run exactly one goal provider call"
    );

    // The busy prompt holds the turn gate; the resume queues behind it.
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
    release_prompt.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "queued paused work must not start a second goal provider call"
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
            // 3 tokens per call: a 5-token budget admits exactly the two
            // rounds under test (the in-flight activate turn and the queued
            // resume continuation), then the charge pauses the goal.
            .activate_goal("race the resume", Some(5))
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

    // Prior turn completes and charges usage: revision advances WITHOUT a
    // lifecycle change and the queued continuation still runs exactly once
    // (the final pause comes from the budget charge, not the stale check).
    release_first.cancel();
    application.wait_for_idle().await;

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "resume must schedule exactly one continuation"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
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
            // 3 tokens per call: a 6-token budget admits exactly the
            // activate turn plus the one newest continuation, then the
            // charge pauses the goal and the owner exits.
            .activate_goal("supersede me", Some(6))
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
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
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
    let release_first = CancellationToken::new();
    let release_second = CancellationToken::new();
    let fixture = controlled_multi_block_fixture(
        calls.clone(),
        started.clone(),
        vec![(1, release_first.clone()), (2, release_second.clone())],
    );
    let session = fixture.session();
    let application = fixture.application().await;

    assert_eq!(
        application
            // 3 tokens per call: a 9-token budget admits exactly the three
            // rounds under test, then the charge pauses the goal.
            .activate_goal("stale start", Some(9))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 1).await;
    // Pause ends the activate owner after its blocked first round.
    application
        .goal_pause()
        .expect("pause while activate round runs");
    release_first.cancel();
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

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

    release_second.cancel();
    application.wait_for_idle().await;

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the already-started stale continuation plus exactly one newest continuation must run"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
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

/// Regression: an activated goal must auto-continue round after round — the
/// activation slot owns the continuation — until a terminating condition
/// ends it. The provider charges a fixed 3 tokens per call, so a 7-token
/// budget admits exactly three rounds (9 tokens used) before the usage
/// charge pauses the goal with `BudgetExhausted` and the owner exits.
/// Deterministic via the call counter plus Notify, no sleeps. Before the
/// fix the continuation decision had no consumer: `calls` stays 1.
#[tokio::test]
async fn active_goal_auto_continues_until_budget_exhaustion() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release, 0);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("work until the budget is gone", Some(7))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 3).await;
    application.wait_for_idle().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the activation owner must re-run the goal each round while the decision is Continue"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 9);
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
}

/// A manual pause landing while a continuation round is in flight must end
/// the auto-continue: the in-flight round still commits, then the owner
/// observes the lifecycle change and exits without starting another
/// provider call. Deterministic via the release token, no sleeps.
#[tokio::test]
async fn goal_auto_continuation_stops_on_manual_pause() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release.clone(), 2);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("pause mid continuation", None)
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;

    application
        .goal_pause()
        .expect("pause during continuation");
    release.cancel();
    application.wait_for_idle().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the continuation owner must stop after the in-flight round"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::Manual));
}

/// An error round is fatal to the auto-continue: the owner must commit a
/// failed terminal and exit rather than retry or spin. The goal itself
/// stays Active — a later manual resume can pick the work up again (one
/// more successful round then exhausts the remaining 7-token budget).
/// Deterministic via the call counter plus Notify, no sleeps.
#[tokio::test]
async fn goal_auto_continuation_error_turn_is_fatal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_error_fixture(calls.clone(), started.clone(), release, 2);
    let application = fixture.application().await;
    let mut events = application.subscribe();

    assert_eq!(
        application
            .activate_goal("survive a fatal round", Some(7))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application.wait_for_idle().await;

    // Per-round uniqueness, drained in full after idle and keyed by turn
    // id: each of the two rounds delivers exactly one TurnStarted and one
    // TurnTerminal (Succeeded then Failed), and the failed round's usage
    // reaches the charge via the typed failure payload (two charges total).
    let mut turns: Vec<(String, usize, usize, Option<TurnTerminalStatus>)> = Vec::new();
    let mut charged_count = 0usize;
    loop {
        match events.try_recv() {
            Ok(ApplicationEvent::TurnStarted { turn_id, .. }) => {
                match turns.iter_mut().find(|(id, _, _, _)| *id == turn_id) {
                    Some(entry) => entry.1 += 1,
                    None => turns.push((turn_id, 1, 0, None)),
                }
            }
            Ok(ApplicationEvent::TurnTerminal {
                turn_id, status, ..
            }) => {
                let entry = turns
                    .iter_mut()
                    .find(|(id, _, _, _)| *id == turn_id)
                    .expect("TurnTerminal without a matching TurnStarted");
                entry.2 += 1;
                entry.3 = Some(status);
            }
            Ok(ApplicationEvent::GoalUsageCharged { .. }) => charged_count += 1,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("application event channel closed before the drain")
            }
        }
    }
    assert_eq!(turns.len(), 2, "one turn per round: {turns:?}");
    for (_, started_count, terminal_count, status) in &turns {
        assert_eq!(*started_count, 1, "exactly one TurnStarted per turn: {turns:?}");
        assert_eq!(*terminal_count, 1, "exactly one terminal per turn: {turns:?}");
        assert!(matches!(status, Some(TurnTerminalStatus::Succeeded | TurnTerminalStatus::Failed)));
    }
    assert_eq!(
        turns.iter().filter(|(_, _, _, status)| matches!(status, Some(TurnTerminalStatus::Failed))).count(),
        1,
        "exactly one failed terminal: {turns:?}"
    );
    assert_eq!(
        turns.iter().filter(|(_, _, _, status)| matches!(status, Some(TurnTerminalStatus::Succeeded))).count(),
        1,
        "exactly one succeeded terminal: {turns:?}"
    );
    assert_eq!(charged_count, 2, "each round settles usage exactly once");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a failed round must not start another continuation"
    );
    assert!(
        !application.is_streaming(),
        "no goal work may remain pending after a fatal turn"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert_eq!(goal.usage.tokens_used, 6);

    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("resume after fatal round"),
        GoalActivationOutcome::Started
    );
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 9);
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
}

/// The activation slot is the single owner of the auto-continue: while a
/// continuation round is in flight, a resume against the still-active goal
/// must report `AlreadyActive` instead of scheduling a duplicate worker,
/// and each round must deliver exactly one continuation reminder keyed to
/// an ever-newer revision. Deterministic via the release token, no sleeps.
#[tokio::test]
async fn goal_auto_continuation_single_owner_no_duplicate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture =
        controlled_usage_fixture(calls.clone(), started.clone(), release.clone(), 2);
    let session = fixture.session();
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("single continuation owner", Some(7))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;

    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("resume during owned continuation"),
        GoalActivationOutcome::AlreadyActive
    );

    release.cancel();
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);

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
    assert_eq!(continuations.len(), 3, "{continuations:?}");
    assert!(
        continuations.windows(2).all(|pair| pair[0] < pair[1]),
        "each continuation round must carry a strictly newer revision: {continuations:?}"
    );
}

/// Stop must end both a queued owner (waiting between rounds) and an
/// in-flight one: the activation slot is dropped and the waiters notified
/// BEFORE `session.abort`, so neither phase makes another goal provider
/// call, and the slot is free again afterwards.
#[tokio::test]
async fn goal_abort_ends_queued_and_in_flight_owner_without_extra_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_prompt = CancellationToken::new();
    let release_goal = CancellationToken::new();
    // Call 1 is the busy prompt, call 2 the resumed goal round; each phase
    // pins the turn gate with its own token so cancelling one never unblocks
    // the other.
    let fixture = controlled_multi_block_fixture(
        calls.clone(),
        started.clone(),
        vec![(1, release_prompt.clone()), (2, release_goal.clone())],
    );
    let application = fixture.application().await;
    application
        .goal_create("stop the owner", Some(100))
        .expect("create metadata");
    application.goal_pause().expect("pause");

    // Queued phase: the prompt holds the gate, the resume queues behind it.
    application
        .prompt("busy turn".to_owned(), Vec::new(), None)
        .await
        .expect("busy prompt");
    wait_for_calls(&calls, &started, 1).await;
    assert_eq!(
        application.resume_goal_work().await.expect("queued resume"),
        GoalActivationOutcome::Queued
    );
    application.abort().await.expect("abort queued owner");
    release_prompt.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the aborted queued owner must never start a goal provider call"
    );

    // In-flight phase: the resume starts (slot was freed by the abort), and
    // a second abort cancels its running round.
    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application.abort().await.expect("abort in-flight owner");
    release_goal.cancel();
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert_eq!(goal.pause_reason, None);
}

/// Stop in the window after a round rekeys and before it claims the session
/// must not start that round. The gate pauses the first round there, so the
/// expected provider-call count is 0. A gate on a later round would be 1.
#[tokio::test]
async fn stop_before_first_claim_starts_no_provider_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release, 0);
    let application = fixture.application().await;
    let gate = application.arm_goal_before_claim_gate_for_test();

    assert_eq!(
        application
            .activate_goal("stop before the claim", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    gate.wait_entered().await;
    application.abort().await.expect("stop before claim");
    gate.release();
    application.wait_for_idle().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a stop before the first claim must not start a provider call"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
    assert_eq!(goal.usage.tokens_used, 0);
}

/// Stop takes the activation slot without touching the goal epoch, and a
/// resume of an Active goal commits nothing — so the successor's key differs
/// from the old owner's only by its spawn-unique token. The old owner must
/// exit with a typed Aborted terminal, without reviving into, rekeying, or
/// wiping the successor's slot.
#[tokio::test]
async fn stop_then_immediate_resume_never_reuses_old_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let release_second = CancellationToken::new();
    let fixture = controlled_multi_block_fixture(
        calls.clone(),
        started.clone(),
        vec![(1, release_first.clone()), (2, release_second.clone())],
    );
    let application = fixture.application().await;
    let mut events = application.subscribe();

    // 3 tokens per call: a 5-token budget admits exactly the two successor
    // rounds, then the charge pauses the goal — the owner ends by budget,
    // never by a manual pause.
    assert_eq!(
        application
            .activate_goal("one live owner", Some(5))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 1).await;
    // Capture the old owner's turn id as soon as it starts.
    let old_turn = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(ApplicationEvent::TurnStarted { turn_id, .. }) => return turn_id,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("application event channel closed before old owner start"),
            }
        }
    })
    .await
    .expect("the old owner never started");
    application.abort().await.expect("stop the old owner");

    // Immediate resume: the goal is still Active, so no lifecycle commit
    // happens and the successor key carries a fresh owner token.
    let outcome = application.resume_goal_work().await.expect("resume");
    assert!(
        matches!(
            outcome,
            GoalActivationOutcome::Started | GoalActivationOutcome::Queued
        ),
        "the successor must be scheduled, got {outcome:?}"
    );
    release_first.cancel();
    // Both conditions are required before the loop ends: the old owner's
    // own turn settles Aborted exactly once, and the successor's first round
    // starts. A missing old terminal fails the bounded wait instead of
    // passing silently.
    let old_terminals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut old_terminals = 0usize;
        let mut successor_started = false;
        loop {
            if old_terminals == 1 && successor_started {
                return old_terminals;
            }
            match events.recv().await {
                Ok(ApplicationEvent::TurnTerminal { turn_id, status, .. }) => {
                    if turn_id == old_turn {
                        old_terminals += 1;
                        assert_eq!(status, TurnTerminalStatus::Aborted);
                    }
                }
                Ok(ApplicationEvent::TurnStarted { turn_id, .. }) => {
                    if turn_id != old_turn {
                        successor_started = true;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("application event channel closed before successor start"),
            }
        }
    })
    .await
    .expect("the old aborted terminal and the successor start never both arrived");
    assert_eq!(old_terminals, 1, "exactly one aborted terminal for the old owner");
    wait_for_calls(&calls, &started, 2).await;

    // While the successor round is blocked, its slot is live: a resume must
    // report AlreadyActive — the old owner neither wiped nor revived into it.
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("resume while successor owns the slot"),
        GoalActivationOutcome::AlreadyActive
    );

    release_second.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the aborted round plus exactly two successor rounds must run"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 6);
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
}

/// A failed goal turn settles its usage through the typed failure payload,
/// never by recomputing from session history: even with compaction having
/// truncated the history mid-round, the round's real charge lands exactly
/// once and the owner exits fatally.
#[tokio::test]
async fn compacted_failed_goal_turn_still_charges_round_usage() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let fixture = HermeticGoalFixture::new_with_context_window(
        "goal-compaction-failure",
        Vec::new(),
        Some(controlled_stream_fn(
            calls.clone(),
            started.clone(),
            Vec::new(),
            // Call 1 is the compaction summary; call 2 is the goal turn.
            2,
            None,
        )),
        60,
    );
    let application = fixture.application().await;
    let session = fixture.session();
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    };
    session.enable_compaction(settings.clone());
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    let mut events = application.subscribe();

    assert_eq!(
        application
            .activate_goal("charge the failed round", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    // One bounded loop collects the compaction lifecycle and the failed
    // terminal together: the preflight's Start and successful End arrive
    // before the failed terminal, so a separate drain would miss them.
    let mut saw_start = false;
    let mut saw_successful_end = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(ApplicationEvent::Session(SessionEvent::CompactionStart { reason })) => {
                    assert_eq!(reason, CompactionReason::Threshold);
                    saw_start = true;
                }
                Ok(ApplicationEvent::Session(SessionEvent::CompactionEnd {
                    result,
                    aborted,
                    ..
                })) => {
                    assert!(!aborted, "the compaction must not be aborted");
                    assert!(result.is_some(), "the compaction must produce a summary");
                    saw_successful_end = true;
                }
                Ok(ApplicationEvent::TurnTerminal {
                    status: TurnTerminalStatus::Failed,
                    ..
                }) => return,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("application event channel closed before the failed terminal"),
            }
        }
    })
    .await
    .expect("the failed round never settled");
    application.wait_for_idle().await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(saw_start, "the compaction preflight never started");
    assert!(
        saw_successful_end,
        "the compaction preflight never produced a summary"
    );
    assert!(matches!(
        session.history().first(),
        Some(Message::CompactionSummary(_))
    ));
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(
        goal.usage.tokens_used, 3,
        "the failed round's streamed usage must reach the charge despite compaction"
    );
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
}

/// An explicit completion ends the owner: the in-flight round still commits,
/// then the owner exits without starting another provider call.
#[tokio::test]
async fn goal_complete_ends_the_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release.clone(), 2);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("finish me", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application.goal_complete().expect("complete");
    release.cancel();
    application.wait_for_idle().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the completed goal must not start another continuation round"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Completed);
}

/// Same exit contract for drop: the in-flight round commits, the owner exits.
#[tokio::test]
async fn goal_drop_ends_the_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release.clone(), 2);
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("discard me", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application.goal_drop().expect("drop");
    release.cancel();
    application.wait_for_idle().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the dropped goal must not start another continuation round"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Dropped);
}

/// A usage charge failure is fatal to the owner: the turn settles with a
/// failed terminal carrying the charge error, exactly one RunFailed is
/// published, and no further round starts.
#[tokio::test]
async fn goal_usage_charge_failure_is_fatal_to_the_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release, 0);
    let application = fixture.application().await;
    let mut events = application.subscribe();

    application
        .goal_create("overflow the charge", None)
        .expect("create metadata");
    // Seed the usage counter so the round's fixed 3-token charge overflows.
    application
        .goal_update_usage(GoalUsageDelta::new(u64::MAX, 0))
        .expect("seed usage near overflow");
    assert_eq!(
        application.resume_goal_work().await.expect("resume"),
        GoalActivationOutcome::Started
    );

    let charge_failure = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut run_failed = None;
        loop {
            match events.recv().await {
                Ok(ApplicationEvent::RunFailed { message }) => {
                    run_failed = Some(message);
                }
                Ok(ApplicationEvent::TurnTerminal {
                    status: TurnTerminalStatus::Failed,
                    ..
                }) => return run_failed,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("application event channel closed before charge failure"),
            }
        }
    })
    .await
    .expect("charge failure never settled the turn");
    let charge_failure = charge_failure.expect("RunFailed before the failed terminal");
    assert!(
        charge_failure.contains("failed to charge goal usage"),
        "the RunFailed message must carry the charge error: {charge_failure}"
    );
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a failed usage charge must end the owner without another round"
    );
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, u64::MAX);
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
}

/// A usage-only revision bump during an owned continuation must not mint a
/// second worker: the resume reports `AlreadyActive` and the slot is left
/// unchanged, so the original owner keeps running.
#[tokio::test]
async fn usage_only_revision_bump_keeps_single_owner_already_active() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let fixture = controlled_usage_fixture(calls.clone(), started.clone(), release.clone(), 2);
    let session = fixture.session();
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("single owner under usage churn", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;

    // Usage-only bump: revision advances, the lifecycle epoch does not.
    application
        .goal_update_usage(GoalUsageDelta::new(3, 0))
        .expect("usage-only bump");
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("resume during owned continuation"),
        GoalActivationOutcome::AlreadyActive
    );

    application.goal_pause().expect("pause ends the owner");
    release.cancel();
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
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
    assert_eq!(continuations.len(), 2, "{continuations:?}");
    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 9, "seed plus both rounds");
}

/// Interaction fairness: a user follow-up landing while a continuation round
/// is in flight is preserved (Steer/FollowUp path) and delivered in the next
/// continuation round's provider context — the owner neither drops the user
/// turn nor spins a second worker.
#[tokio::test]
async fn follow_up_interjection_lands_in_next_continuation_round() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_second = CancellationToken::new();
    let release_third = CancellationToken::new();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let fixture = HermeticGoalFixture::new(
        "goal-fair",
        Vec::new(),
        Some(controlled_stream_fn(
            calls.clone(),
            started.clone(),
            vec![(2, release_second.clone()), (3, release_third.clone())],
            0,
            Some(contexts.clone()),
        )),
    );
    let application = fixture.application().await;

    assert_eq!(
        application
            .activate_goal("fair interleave", Some(100))
            .await
            .expect("activate"),
        GoalActivationOutcome::Started
    );
    wait_for_calls(&calls, &started, 2).await;
    application
        .follow_up("user interjection".to_owned(), Vec::new())
        .await
        .expect("follow-up during owned continuation");

    release_second.cancel();
    wait_for_calls(&calls, &started, 3).await;
    let round_three = context_text(
        &contexts
            .lock()
            .expect("contexts")
            .get(2)
            .expect("third provider call")
            .clone(),
    );
    assert!(
        round_three.contains("user interjection"),
        "the follow-up must reach the next continuation round: {round_three}"
    );
    assert!(
        round_three.contains("Active session goal ("),
        "the next round must still carry the goal reminder: {round_three}"
    );

    application.goal_pause().expect("pause ends the owner");
    release_third.cancel();
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the paused owner must not start a fourth round"
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

/// Regression: goal auto-runs share the turn path with user prompts, so a
/// goal turn that starts above the compaction threshold must summarize before
/// its first provider request. The settings are re-enabled AFTER Application
/// publication on purpose: publication applies the hermetic config
/// (`compaction: None`), and the auto-run must honor the post-publish setting
/// without any second user message to nudge it.
#[tokio::test]
async fn goal_auto_run_preflights_compaction_after_application_publish() {
    let contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_fn: pi_agent::StreamFn = {
        let contexts = contexts.clone();
        Arc::new(move |model, context, options| {
            let contexts = contexts.clone();
            Box::pin(async move {
                contexts.lock().expect("contexts").push(context.clone());
                pi_ai::stream_simple(model, context, options).await
            })
        })
    };
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    };
    let fixture = HermeticGoalFixture::new_with_context_window(
        "goal-compaction",
        vec![
            FauxResponse::text("checkpoint summary"),
            FauxResponse::text("goal work done"),
        ],
        Some(stream_fn),
        60,
    );
    let session = fixture.session();
    let application = fixture.application().await;
    session.enable_compaction(settings.clone());
    assert!(session.auto_compaction_enabled());
    session
        .load_history(vec![
            Message::user_text("older request ".repeat(20), 1),
            Message::user_text("recent request", 2),
        ])
        .await
        .expect("load history");
    let preloaded = session.history();
    assert!(
        pi_coding::should_compact(
            pi_coding::estimate_live_context_tokens(&preloaded),
            60,
            &settings
        ),
        "preloaded goal context must already exceed the threshold: {preloaded:?}"
    );

    let mut events = application.subscribe();

    assert_eq!(
        application
            .activate_goal("ship without a second prompt", Some(100))
            .await
            .expect("activate goal"),
        GoalActivationOutcome::Started
    );
    let application_for_pause = application.clone();
    let start_reason = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(ApplicationEvent::Session(SessionEvent::CompactionStart { reason })) => {
                    // Explicit pause: ends the auto-continue owner after this
                    // in-flight round instead of relying on faux-response
                    // exhaustion to stop it.
                    application_for_pause
                        .goal_pause()
                        .expect("pause after compaction start");
                    return reason;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("application event channel closed before compaction start")
                }
            }
        }
    })
    .await
    .expect("goal auto-run never published a compaction start");
    assert_eq!(
        start_reason,
        CompactionReason::Threshold,
        "the auto-run must compact for the threshold reason"
    );
    application.wait_for_idle().await;

    let captured = contexts.lock().expect("contexts").clone();
    assert!(
        captured.len() >= 2,
        "expected a compaction summary call before the goal turn request, saw {} provider call(s)",
        captured.len()
    );
    assert_eq!(
        captured[0].messages.len(),
        1,
        "the goal auto-run must summarize before its first provider request: {:?}",
        captured[0]
    );
    let normal = context_text(&captured[1]);
    assert!(
        normal.contains("checkpoint summary"),
        "the goal request must carry the compaction summary: {normal}"
    );

    let goal = application.goal_state().current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::Manual));
    assert!(matches!(
        session.history().first(),
        Some(Message::CompactionSummary(_))
    ));
}
