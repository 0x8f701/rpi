//! Deterministic pi-cli adapter coverage for settings persistence, loop
//! restart/durability, model/thinking runtime changes, goal continuation
//! boundaries, and same-CWD session tree/branch/fork/resume.
//!
//! These tests strengthen the CLI adapter boundary only. Core contracts that
//! already live in `pi-coding` unit/integration suites are exercised here
//! through the interactive command adapters and Application APIs the CLI uses.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_agent::ThinkingLevel;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Message, Model, StopReason, Usage};
use pi_cli::goal_commands::{
    InteractiveGoalCommand, execute_interactive_goal_command, parse_interactive_goal_command,
};
use pi_cli::interactive_commands::{
    InteractiveSettingsCommand, execute_interactive_settings_command,
    parse_interactive_settings_command,
};
use pi_cli::loop_commands::{
    InteractiveLoopCommand, execute_interactive_loop_command, parse_interactive_loop_command,
};
use pi_coding::{
    Application, ApplicationEvent, GoalContinuationDecision, GoalLifecycle, GoalPauseReason,
    GoalUsageDelta, LoopCreateRequest, NavigateTreeOptions, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::now_v7().simple())
}

fn faux_model(label: &str, reasoning: bool) -> (Model, FauxProviderRegistration) {
    let suffix = unique(label);
    let mut model = Model::default();
    model.id = format!("{label}-model");
    model.name = format!("{label} Model");
    model.api = format!("{suffix}-api");
    model.provider = format!("{suffix}-provider");
    model.base_url = "http://localhost:0".into();
    model.reasoning = reasoning;
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    (model, registration)
}

fn session_options(model: Model, cwd: &Path, thinking: ThinkingLevel) -> SessionOptions {
    SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: thinking,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    }
}

async fn application_with_resources(
    model: Model,
    cwd: &Path,
    agent_dir: &Path,
    thinking: ThinkingLevel,
    responses: Vec<FauxResponse>,
) -> (Application, FauxProviderRegistration) {
    let (model, registration) = {
        let mut model = model;
        let suffix = unique("app");
        model.api = format!("{}-{suffix}", model.api);
        model.provider = format!("{}-{suffix}", model.provider);
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 8,
        });
        if !responses.is_empty() {
            registration.set_responses(responses);
        }
        (model, registration)
    };

    fs::create_dir_all(agent_dir).expect("agent dir");
    if !agent_dir.join("settings.json").exists() {
        fs::write(agent_dir.join("settings.json"), "{}").expect("seed global settings");
    }
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir.to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let session = Session::new(session_options(model, cwd, thinking)).expect("session");
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application = Application::new(session).await;
    (application, registration)
}

async fn recorded_application(
    model: Model,
    cwd: &Path,
    session_dir: &Path,
    thinking: ThinkingLevel,
    responses: Vec<FauxResponse>,
    session_id: &str,
) -> (Application, FauxProviderRegistration, PathBuf) {
    let (model, registration) = {
        let mut model = model;
        let suffix = unique("recorded");
        model.api = format!("{}-{suffix}", model.api);
        model.provider = format!("{}-{suffix}", model.provider);
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        if !responses.is_empty() {
            registration.set_responses(responses);
        }
        (model, registration)
    };
    let thinking_name = match thinking {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    };
    let session = Session::new(session_options(model.clone(), cwd, thinking)).expect("session");
    let recorder = pi_coding::start_session_in(
        cwd,
        Some(&model),
        Some(thinking_name),
        Some(session_dir),
        Some(session_id),
        None,
    )
    .expect("start recorder");
    let path = recorder.path();
    session.record(recorder).expect("attach recorder");
    let application = Application::new(session).await;
    (application, registration, path)
}

fn usage_stream_session(cwd: &Path, usage: Usage) -> Session {
    let model = Model {
        id: unique("goal-usage-model"),
        name: "Goal Usage Model".into(),
        api: unique("goal-usage-api"),
        provider: unique("goal-usage-provider"),
        base_url: "http://localhost:0".into(),
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
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("usage session")
}

fn parse_settings(argument: &str) -> InteractiveSettingsCommand {
    parse_interactive_settings_command("settings", Some(argument))
        .expect("parse settings")
        .expect("settings command")
}

fn parse_loop(name: &str, argument: Option<&str>) -> InteractiveLoopCommand {
    parse_interactive_loop_command(name, argument)
        .expect("parse loop")
        .expect("loop command")
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
            .collect(),
        Message::Assistant(message) => message.text(),
        _ => String::new(),
    }
}

fn user_entry_id(application: &Application) -> String {
    application
        .session_entries(None)
        .expect("entries")
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(Message::User(_))))
        .expect("user entry")
        .id
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read json")).expect("parse json")
}

/// Settings adapter persists project vs global scopes and reports restart-required
/// values without applying them live.
#[tokio::test]
async fn settings_adapter_persists_project_and_global_scopes_with_restart_truth() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let agent_dir = home.path().join(".pi").join("agent");
    let (model, _) = faux_model("settings", false);
    let (application, registration) = application_with_resources(
        model,
        cwd.path(),
        &agent_dir,
        ThinkingLevel::Off,
        Vec::new(),
    )
    .await;

    let live = execute_interactive_settings_command(
        &application,
        parse_settings("apply --global compaction.enabled false"),
    )
    .await
    .expect("apply live global");
    let live: Value = serde_json::from_str(&live).expect("live outcome json");
    assert_eq!(live["appliedLive"], true);
    assert_eq!(live["restartRequired"], false);
    assert!(!application.runtime_settings().compaction.enabled);

    let project_live = execute_interactive_settings_command(
        &application,
        parse_settings("apply --project quietStartup true"),
    )
    .await
    .expect("apply live project");
    let project_live: Value = serde_json::from_str(&project_live).expect("project outcome json");
    assert_eq!(project_live["appliedLive"], true);
    assert_eq!(project_live["restartRequired"], false);

    let restart = execute_interactive_settings_command(
        &application,
        InteractiveSettingsCommand::Apply {
            scope: pi_coding::SettingsScope::Global,
            key: "defaultProvider".into(),
            value: json!("future-provider"),
        },
    )
    .await
    .expect("apply restart global");
    let restart: Value = serde_json::from_str(&restart).expect("restart outcome json");
    assert_eq!(restart["appliedLive"], false);
    assert_eq!(restart["reloaded"], false);
    assert_eq!(restart["restartRequired"], true);

    let global = read_json(&agent_dir.join("settings.json"));
    assert_eq!(global["compaction"]["enabled"], false);
    assert_eq!(global["defaultProvider"], "future-provider");

    let project = read_json(&cwd.path().join(".pi").join("settings.json"));
    assert_eq!(project["quietStartup"], true);
    assert!(
        project.get("defaultProvider").is_none(),
        "global restart key must not leak into project scope: {project}"
    );

    // Reload from disk through a fresh manager to prove values survive process
    // boundaries while the still-running application keeps its prior generation.
    let reloaded = ResourceManager::new({
        let mut options = ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent_dir.clone();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        options
    })
    .expect("reload resources");
    let snapshot = reloaded.snapshot();
    let settings = &snapshot.settings;
    assert_eq!(settings.default_provider.as_deref(), Some("future-provider"));
    assert_eq!(
        settings
            .compaction
            .as_ref()
            .and_then(|value| value.enabled),
        Some(false)
    );
    assert_eq!(settings.quiet_startup, Some(true));

    application.cleanup().await;
    registration.unregister();
}

/// Durable loops restore after session restart; ephemeral loops do not, and an
/// immediate fire path settles once (run_count == 1) without double-counting.
#[tokio::test]
async fn loop_adapter_restores_durable_tasks_and_avoids_double_fire() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let (model, _) = faux_model("loop", false);
    let (application, registration, first_path) = recorded_application(
        model.clone(),
        cwd.path(),
        session_dir.path(),
        ThinkingLevel::Off,
        vec![
            FauxResponse::text("loop one"),
            FauxResponse::text("loop two"),
            FauxResponse::text("loop three"),
        ],
        "loop-source",
    )
    .await;

    let durable = application
        .loop_create(LoopCreateRequest {
            interval: "1h".into(),
            prompt: "durable checkpoint".into(),
            fire_immediately: false,
            durable: true,
        })
        .await
        .expect("create durable");
    assert!(durable.durable);
    assert_eq!(durable.run_count, 0);

    // Adapter create is fire-immediately + non-durable.
    let ephemeral_out = execute_interactive_loop_command(
        &application,
        parse_loop("loop", Some("1h ephemeral only")),
    )
    .await
    .expect("create ephemeral via adapter");
    let ephemeral_id = ephemeral_out
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("ephemeral id")
        .to_owned();
    application.wait_for_idle().await;

    // Immediate fire path settles exactly once.
    let immediate = application
        .loop_create(LoopCreateRequest::immediate("30m", "fire once"))
        .await
        .expect("immediate create");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        application.wait_for_idle().await;
        let task = application
            .loop_list()
            .await
            .expect("list")
            .into_iter()
            .find(|task| task.id == immediate.id);
        if let Some(task) = task {
            if task.run_count >= 1 {
                assert_eq!(
                    task.run_count, 1,
                    "immediate loop must not double-fire: {task:?}"
                );
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("immediate loop never recorded a single run");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Cancel remaining non-durable tasks before switch evidence.
    let _ = application.loop_cancel(&immediate.id).await;
    let _ = application.loop_cancel(&ephemeral_id).await;

    let ephemeral = application
        .loop_create(LoopCreateRequest {
            interval: "2h".into(),
            prompt: "ephemeral again".into(),
            fire_immediately: false,
            durable: false,
        })
        .await
        .expect("recreate ephemeral");

    let second = pi_coding::start_session_in(
        cwd.path(),
        Some(&model),
        Some("off"),
        Some(session_dir.path()),
        Some("loop-second"),
        None,
    )
    .expect("second recorder");
    let second_path = second.path();
    second.persist_now().expect("persist second session");
    drop(second);

    application
        .switch_session(&second_path)
        .await
        .expect("switch away");
    assert!(
        application
            .loop_list()
            .await
            .expect("second loops")
            .is_empty(),
        "session switch must suspend all loops"
    );

    application
        .switch_session(&first_path)
        .await
        .expect("switch back");
    let restored = application.loop_list().await.expect("restored loops");
    assert_eq!(restored.len(), 1, "only durable loops restore: {restored:?}");
    assert_eq!(restored[0].id, durable.id);
    assert_eq!(restored[0].prompt, "durable checkpoint");
    assert!(restored[0].durable);
    assert!(
        restored.iter().all(|task| task.id != ephemeral.id),
        "ephemeral loop must not restore after restart"
    );

    let loops_file = first_path.with_file_name(format!(
        "{}.loops.json",
        first_path
            .file_name()
            .expect("session file name")
            .to_string_lossy()
    ));
    assert!(loops_file.exists(), "durable loop state file must exist");
    let persisted = read_json(&loops_file);
    let tasks = persisted["tasks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], durable.id);
    assert_eq!(tasks[0]["prompt"], "durable checkpoint");
    assert_eq!(tasks[0]["durable"], true);

    application.cleanup().await;
    registration.unregister();
}

/// Model and thinking adapters clamp to the active model, persist the effective
/// level, and make the next turn observe the new model identity.
#[tokio::test]
async fn model_and_thinking_runtime_clamp_and_next_turn_observe_change() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let contexts = Arc::new(Mutex::new(Vec::<pi_ai::Context>::new()));

    let mut reasoning = Model::default();
    reasoning.id = "reasoner".into();
    reasoning.name = "Reasoner".into();
    reasoning.api = unique("reason-api");
    reasoning.provider = unique("reason-provider");
    reasoning.base_url = "http://localhost:0".into();
    reasoning.reasoning = true;

    let mut plain = Model::default();
    plain.id = "plain".into();
    plain.name = "Plain".into();
    plain.api = unique("plain-api");
    plain.provider = unique("plain-provider");
    plain.base_url = "http://localhost:0".into();
    plain.reasoning = false;

    let registration = register_faux_provider(FauxProviderOptions {
        api: reasoning.api.clone(),
        provider: reasoning.provider.clone(),
        models: vec![reasoning.clone(), plain.clone()],
        chunk_size: 4,
    });
    // Second registration so plain provider/api resolves through the faux stream.
    let plain_registration = register_faux_provider(FauxProviderOptions {
        api: plain.api.clone(),
        provider: plain.provider.clone(),
        models: vec![plain.clone()],
        chunk_size: 4,
    });
    registration.set_responses(vec![FauxResponse::text("first turn")]);
    plain_registration.set_responses(vec![FauxResponse::text("second turn")]);

    let captured = contexts.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("contexts").push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });

    let session = Session::new(SessionOptions {
        model: reasoning.clone(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("session");
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        Some(&reasoning),
        Some("off"),
        Some(session_dir.path()),
        Some("model-think"),
        None,
    )
    .expect("recorder");
    let session_path = recorder.path();
    session.record(recorder).expect("attach");
    let application = Application::new(session).await;

    let high = application.set_thinking_level(ThinkingLevel::High);
    assert!(!high.clamped, "{}", high.message);
    assert_eq!(high.requested, ThinkingLevel::High);
    assert_eq!(high.effective, ThinkingLevel::High);
    assert_eq!(application.session().thinking_level(), ThinkingLevel::High);

    application
        .prompt("first".into(), Vec::new(), None)
        .await
        .expect("first prompt");
    application.wait_for_idle().await;

    // Switching to a non-reasoning model clamps the live thinking level and
    // records both model_change and thinking_level_change on the branch.
    let clamped = application.set_model(plain.clone(), "faux".into());
    assert!(clamped.clamped, "{}", clamped.message);
    assert_eq!(clamped.requested, ThinkingLevel::High);
    assert_eq!(clamped.effective, ThinkingLevel::Off);
    assert_eq!(application.session().thinking_level(), ThinkingLevel::Off);
    assert_eq!(
        application
            .state()
            .await
            .model
            .as_ref()
            .map(|model| model.id.as_str()),
        Some("plain")
    );

    application
        .prompt("second".into(), Vec::new(), None)
        .await
        .expect("second prompt");
    application.wait_for_idle().await;

    let contexts = contexts.lock().expect("contexts");
    assert_eq!(contexts.len(), 2, "two provider turns");
    drop(contexts);

    let history = application.messages();
    let assistant_models = history
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message.model.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_models,
        ["reasoner", "plain"],
        "next turn must use the post-switch model identity"
    );

    let tree = application.session_tree().expect("tree");
    let loaded = pi_coding::load_session_tree(&session_path).expect("load session");
    let branch = loaded.branch(tree.active_leaf_id.as_deref());
    assert!(
        branch.iter().any(|entry| {
            entry.entry_type == "model_change"
                && entry.provider.as_deref() == Some(plain.provider.as_str())
                && entry.model_id.as_deref() == Some("plain")
        }),
        "model_change must record the plain model"
    );
    assert!(
        branch.iter().any(|entry| {
            entry.entry_type == "thinking_level_change"
                && entry.thinking_level.as_deref() == Some("off")
        }),
        "clamped thinking level must be recorded"
    );

    // Requesting high again on the plain model stays clamped.
    let still_clamped = application.set_thinking_level(ThinkingLevel::High);
    assert!(still_clamped.clamped, "{}", still_clamped.message);
    assert_eq!(still_clamped.effective, ThinkingLevel::Off);

    application.cleanup().await;
    registration.unregister();
    plain_registration.unregister();
}

/// Goal adapter projects continuation decisions and pauses when the turn budget
/// is exhausted; resume safety keeps the goal paused across same-CWD resume.
#[tokio::test]
async fn goal_adapter_projects_active_goal_and_pauses_at_budget_boundary() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let session = usage_stream_session(
        cwd.path(),
        Usage {
            input: 5,
            output: 5,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 10,
            ..Usage::default()
        },
    );
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some("goal-budget"),
        None,
    )
    .expect("recorder");
    let session_path = recorder.path();
    session.record(recorder).expect("attach");
    let application = Application::new(session).await;

    let created = execute_interactive_goal_command(
        &application,
        parse_interactive_goal_command(Some("create --tokens 10 ship safely"))
            .expect("parse create"),
    )
    .await
    .expect("create goal");
    assert!(
        created.starts_with("Goal work started · active · 0/10 tokens · ship safely"),
        "{created}"
    );

    application.wait_for_idle().await;

    let show = execute_interactive_goal_command(&application, InteractiveGoalCommand::Show)
        .await
        .expect("show goal");
    assert!(
        show.starts_with("paused · 10/10 tokens · ship safely"),
        "budget exhaustion must pause via adapter: {show}"
    );
    let state = application.goal_state();
    let goal = state.current.expect("goal");
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
    assert_eq!(
        application.goal_continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: goal.id.clone(),
            reason: GoalPauseReason::BudgetExhausted,
            revision: state.revision,
        }
    );

    // Same-CWD resume rebuilds the goal runtime and keeps a budget-paused goal
    // paused (no auto-continue).
    let resumed_session = usage_stream_session(cwd.path(), Usage::default());
    resumed_session
        .record(pi_coding::resume_session(&session_path).expect("resume recorder"))
        .expect("attach resume");
    let resumed = Application::new(resumed_session).await;
    resumed.prepare_resumed_goal(false).expect("resume safety");
    let resumed_goal = resumed.goal_state().current.expect("resumed goal");
    assert_eq!(resumed_goal.id, goal.id);
    assert_eq!(resumed_goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(
        resumed_goal.pause_reason,
        Some(GoalPauseReason::BudgetExhausted)
    );
    assert_eq!(
        resumed.goal_continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: resumed_goal.id,
            reason: GoalPauseReason::BudgetExhausted,
            revision: resumed.goal_state().revision,
        }
    );

    // Active-goal resume safety: create a fresh active goal, switch, and prove
    // resume pauses it with ResumeSafety rather than auto-continuing.
    let active_session = usage_stream_session(cwd.path(), Usage::default());
    let active_recorder = pi_coding::start_session_in(
        cwd.path(),
        active_session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some("goal-active-resume"),
        None,
    )
    .expect("active recorder");
    let active_path = active_recorder.path();
    active_session.record(active_recorder).expect("attach active");
    let active_app = Application::new(active_session).await;
    let original = active_app
        .goal_create("stay paused on resume", Some(50))
        .expect("create active");
    assert_eq!(original.lifecycle, GoalLifecycle::Active);

    let safety_session = usage_stream_session(cwd.path(), Usage::default());
    safety_session
        .record(pi_coding::resume_session(&active_path).expect("resume active"))
        .expect("attach safety");
    let safety = Application::new(safety_session).await;
    safety
        .switch_session(&active_path)
        .await
        .expect("switch resume");
    let paused = safety.goal_state().current.expect("paused on resume");
    assert_eq!(paused.id, original.id);
    assert_eq!(paused.lifecycle, GoalLifecycle::Paused);
    assert_eq!(paused.pause_reason, Some(GoalPauseReason::ResumeSafety));

    application.cleanup().await;
    resumed.cleanup().await;
    active_app.cleanup().await;
    safety.cleanup().await;
}

/// Same-CWD session tree navigation, fork, clone, and resume preserve lineage
/// and reset the live transcript to the selected branch.
#[tokio::test]
async fn same_cwd_session_tree_branch_fork_clone_and_resume_lineage() {
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let (model, _) = faux_model("session-tree", false);
    let (application, registration, source_path) = recorded_application(
        model.clone(),
        cwd.path(),
        session_dir.path(),
        ThinkingLevel::Off,
        vec![
            FauxResponse::text("answer one"),
            FauxResponse::text("answer two"),
            FauxResponse::text("answer three"),
        ],
        "tree-source",
    )
    .await;

    application
        .prompt("first question".into(), Vec::new(), None)
        .await
        .expect("first prompt");
    application.wait_for_idle().await;
    application
        .prompt("second question".into(), Vec::new(), None)
        .await
        .expect("second prompt");
    application.wait_for_idle().await;

    let before = application.state().await;
    let source_file = before.session_file.clone().expect("source file");
    let source_id = before.session_id.clone().expect("source id");
    let messages_before = application.messages();
    assert!(
        messages_before.len() >= 4,
        "expected at least two full turns, got {}: {messages_before:?}",
        messages_before.len()
    );
    assert!(
        messages_before
            .iter()
            .any(|message| message_text(message) == "first question"),
        "missing first question in {messages_before:?}"
    );
    assert!(
        messages_before
            .iter()
            .any(|message| message_text(message) == "second question"),
        "missing second question in {messages_before:?}"
    );

    let first_user = application
        .session_entries(None)
        .expect("entries")
        .entries
        .into_iter()
        .find(|entry| {
            matches!(
                &entry.message,
                Some(Message::User(user))
                    if message_text(&Message::User(user.clone())) == "first question"
            )
        })
        .expect("first user entry");

    // Branch/navigate back to the first user turn: live transcript shrinks and
    // editor text exposes the abandoned prompt for re-editing.
    let navigated = application
        .navigate_tree(&first_user.id, NavigateTreeOptions::default())
        .await
        .expect("navigate");
    assert!(navigated.changed);
    assert_eq!(navigated.editor_text.as_deref(), Some("first question"));
    let after_nav = application.messages();
    assert!(
        after_nav
            .iter()
            .all(|message| message_text(message) != "second question"),
        "navigating to the first turn must drop the later branch from the live transcript: {after_nav:?}"
    );

    // Re-run a turn on the navigated branch so fork has a concrete user entry.
    application
        .prompt("first question".into(), Vec::new(), None)
        .await
        .expect("re-ask first");
    application.wait_for_idle().await;
    let fork_user = user_entry_id(&application);

    let forked_text = application.fork_session(&fork_user).await.expect("fork");
    assert_eq!(forked_text, "first question");
    let forked_state = application.state().await;
    let fork_file = forked_state.session_file.clone().expect("fork file");
    let fork_id = forked_state.session_id.clone().expect("fork id");
    assert_ne!(fork_file, source_file);
    assert_ne!(fork_id, source_id);
    let fork_tree = pi_coding::load_session_tree(Path::new(&fork_file)).expect("fork tree");
    assert_eq!(
        fork_tree.header.parent_session.as_deref(),
        Some(source_file.as_str())
    );
    // Fork branches before the selected user turn: the prompt is returned for
    // re-edit and must not keep abandoned later-branch content.
    let fork_messages = application.messages();
    assert!(
        fork_messages
            .iter()
            .all(|message| message_text(message) != "second question"),
        "forked branch must not include the abandoned second turn: {fork_messages:?}"
    );
    assert!(
        fork_messages
            .iter()
            .all(|message| message_text(message) != "clone marker"),
        "fork must start without later clone-only content"
    );

    // Clone the current active branch into a new session file with lineage.
    application
        .prompt("clone marker".into(), Vec::new(), None)
        .await
        .expect("clone marker turn");
    application.wait_for_idle().await;
    let pre_clone = application.state().await;
    let pre_clone_file = pre_clone.session_file.clone().expect("pre-clone file");
    let pre_clone_messages = application.messages();
    application.clone_session().await.expect("clone");
    let cloned = application.state().await;
    let clone_file = cloned.session_file.clone().expect("clone file");
    assert_ne!(clone_file, pre_clone_file);
    let clone_tree = pi_coding::load_session_tree(Path::new(&clone_file)).expect("clone tree");
    assert_eq!(
        clone_tree.header.parent_session.as_deref(),
        Some(pre_clone_file.as_str())
    );
    assert_eq!(
        application
            .messages()
            .iter()
            .map(message_text)
            .collect::<Vec<_>>(),
        pre_clone_messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>(),
        "clone keeps the active branch transcript"
    );

    // Resume the original source session in-process (same CWD) and prove the
    // live transcript rebuilds from that file, not the clone tip.
    application
        .switch_session(Path::new(&source_file))
        .await
        .expect("resume source");
    let resumed = application.state().await;
    assert_eq!(resumed.session_file.as_deref(), Some(source_file.as_str()));
    assert_eq!(resumed.session_id.as_deref(), Some(source_id.as_str()));
    let resumed_messages = application.messages();
    assert!(
        resumed_messages
            .iter()
            .any(|message| message_text(message) == "second question")
            || resumed_messages
                .iter()
                .any(|message| message_text(message) == "first question"),
        "same-CWD resume must rebuild transcript from the source session: {resumed_messages:?}"
    );
    assert!(
        resumed_messages
            .iter()
            .all(|message| message_text(message) != "clone marker"),
        "resumed source must not carry clone-only turns"
    );

    // new_session resets the live transcript while remaining in the same CWD.
    application.new_session().await.expect("new session");
    let fresh = application.state().await;
    assert_eq!(fresh.message_count, 0);
    assert!(application.messages().is_empty());
    assert_ne!(fresh.session_file.as_deref(), Some(source_file.as_str()));
    assert_eq!(
        application.session().cwd(),
        cwd.path(),
        "new session stays on the same CWD"
    );

    let _ = source_path;
    application.cleanup().await;
    registration.unregister();
}

/// Settings inspect/search adapters surface scoped rows without writing.
#[tokio::test]
async fn settings_inspect_and_search_are_read_only() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let agent_dir = home.path().join(".pi").join("agent");
    fs::create_dir_all(&agent_dir).expect("agent dir");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"theme":"dark","compaction":{"enabled":true}}"#,
    )
    .expect("seed settings");
    let (model, _) = faux_model("settings-ro", false);
    let (application, registration) = application_with_resources(
        model,
        cwd.path(),
        &agent_dir,
        ThinkingLevel::Off,
        Vec::new(),
    )
    .await;

    let before = fs::read(agent_dir.join("settings.json")).expect("before");
    let inspect = execute_interactive_settings_command(
        &application,
        parse_settings("list --global"),
    )
    .await
    .expect("inspect");
    assert!(inspect.contains("compaction.enabled"), "{inspect}");
    assert!(
        inspect.to_ascii_lowercase().contains("\"scope\": \"global\"")
            || inspect.to_ascii_lowercase().contains("\"scope\":\"global\""),
        "{inspect}"
    );

    let search = execute_interactive_settings_command(
        &application,
        parse_settings("search --global compaction"),
    )
    .await
    .expect("search");
    assert!(search.contains("compaction"), "{search}");
    assert_eq!(
        fs::read(agent_dir.join("settings.json")).expect("after"),
        before,
        "inspect/search must not mutate settings.json"
    );

    application.cleanup().await;
    registration.unregister();
}

/// Goal create/pause/resume/complete/drop adapter chain starts and resumes
/// work while keeping lifecycle mutations coherent.
#[tokio::test]
async fn goal_adapter_lifecycle_commands_are_coherent() {
    let cwd = TempDir::new().expect("cwd");
    let (model, registration) = faux_model("goal-life", false);
    let session =
        Session::new(session_options(model, cwd.path(), ThinkingLevel::Off)).expect("session");
    session
        .record(
            pi_coding::start_session_in(
                cwd.path(),
                session.model().as_ref(),
                Some("off"),
                Some(cwd.path()),
                Some("goal-life"),
                None,
            )
            .expect("recorder"),
        )
        .expect("attach");
    let application = Application::new(session).await;
    registration.set_responses(vec![
        FauxResponse::text("created"),
        FauxResponse::text("resumed"),
    ]);

    let created = execute_interactive_goal_command(
        &application,
        parse_interactive_goal_command(Some("create --tokens 20 adapter goal")).expect("parse"),
    )
    .await
    .expect("create");
    assert_eq!(created, "Goal work started · active · 0/20 tokens · adapter goal");
    application.wait_for_idle().await;

    let paused = execute_interactive_goal_command(&application, InteractiveGoalCommand::Pause)
        .await
        .expect("pause");
    assert_eq!(paused, "paused · 0/20 tokens · adapter goal");
    assert_eq!(
        application.goal_state().current.expect("goal").pause_reason,
        Some(GoalPauseReason::Manual)
    );

    // Manual usage charge while paused must not auto-resume.
    application
        .goal_update_usage(GoalUsageDelta::new(4, 1))
        .expect("usage while paused");
    let still_paused =
        execute_interactive_goal_command(&application, InteractiveGoalCommand::Show)
            .await
            .expect("show paused");
    assert_eq!(still_paused, "paused · 4/20 tokens · adapter goal");

    let resumed = execute_interactive_goal_command(&application, InteractiveGoalCommand::Resume)
        .await
        .expect("resume");
    assert_eq!(resumed, "Goal work started · active · 4/20 tokens · adapter goal");
    assert!(matches!(
        application.goal_continuation_decision(),
        GoalContinuationDecision::Continue {
            remaining_tokens: Some(16),
            ..
        }
    ));

    application.wait_for_idle().await;
    let completed =
        execute_interactive_goal_command(&application, InteractiveGoalCommand::Complete)
            .await
            .expect("complete");
    assert_eq!(completed, "completed · 4/20 tokens · adapter goal");
    assert!(matches!(
        application.goal_continuation_decision(),
        GoalContinuationDecision::Terminal { .. }
    ));
    // Completed goals remain current and reject drop/create replacements — the
    // terminal continuation decision is the adapter boundary under test.
    assert!(
        execute_interactive_goal_command(&application, InteractiveGoalCommand::Drop)
            .await
            .is_err(),
        "completed goals cannot be dropped"
    );
    assert!(
        execute_interactive_goal_command(
            &application,
            parse_interactive_goal_command(Some("create replacement")).expect("parse"),
        )
        .await
        .is_err(),
        "completed current goal blocks a second create"
    );

    application.cleanup().await;
    registration.unregister();
}
