use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use pi_agent::{AbortController, ThinkingLevel, ToolCallContext};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{ContentBlock, Message, Model, SimpleStreamOptions, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentDiscoveryOptions, AgentStatus,
    ChildSessionFactory, OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions,
    TaskItem,
};

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition { name: name.to_owned(),
    description: format!("{name} description"),
    system_prompt: format!("{name} prompt"),
    tools: Some(Vec::new()),
    autoload_skills: Vec::new(),
    model: None,
    thinking_level: Some(ThinkingLevel::Off),
    max_turns: None,
    max_tool_calls: None,
    timeout_secs: None,
    disallowed_tools: Vec::new(),
    capability_ceiling: None,
    source: AgentDefinitionSource::Bundled,
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
}

fn faux_factory(
    registrations: Arc<Mutex<HashMap<String, pi_ai::providers::FauxProviderRegistration>>>,
) -> ChildSessionFactory {
    Arc::new(move |request| {
        let registrations = registrations.clone();
        Box::pin(async move {
            let registration = registrations
                .lock()
                .get(&request.child_id)
                .cloned()
                .expect("registration for child");
            let child_model = registration.model(None).expect("registered child model");
            Session::new(SessionOptions {
                model: child_model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "faux".to_owned(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    })
}

/// `faux_factory` variant that installs a test-only `before_tool_call` hook on
/// each child session. The hook fires from *child tool execution* the instant a
/// child's model emits a `hub` call with `op: "wait"`, notifying the per-child
/// readiness signal so the parent test can deterministically wait until both
/// children have begun their explicit waits before issuing any send. This
/// removes the race where peer-roster visibility (which precedes the model
/// emitting its wait) let Main's send arrive before wait registration and be
/// consumed as steering. Routing still flows entirely through the owned
/// `hub` AgentTool; no direct child `runtime` sends are used.
fn faux_factory_with_wait_readiness(
    registrations: Arc<Mutex<HashMap<String, pi_ai::providers::FauxProviderRegistration>>>,
    wait_readiness: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
) -> ChildSessionFactory {
    Arc::new(move |request| {
        let registrations = registrations.clone();
        let wait_readiness = wait_readiness.clone();
        let child_id = request.child_id.clone();
        Box::pin(async move {
            let registration = registrations
                .lock()
                .get(&child_id)
                .cloned()
                .expect("registration for child");
            let child_model = registration.model(None).expect("registered child model");
            let before_tool_call: Option<pi_agent::BeforeToolCallFn> =
                wait_readiness.lock().get(&child_id).cloned().map(|notify| {
                    Arc::new(move |context: pi_agent::BeforeToolCallContext| {
                        let notify = notify.clone();
                        Box::pin(async move {
                            if context.tool_call.name == "hub"
                                && context.arguments.get("op").and_then(|value| value.as_str())
                                    == Some("wait")
                            {
                                notify.notify_one();
                            }
                            Ok(pi_agent::BeforeToolCallResult::default())
                        })
                            as pi_agent::BoxFuture<anyhow::Result<pi_agent::BeforeToolCallResult>>
                    }) as pi_agent::BeforeToolCallFn
                });
            Session::new(SessionOptions {
                model: child_model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "faux".to_owned(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    })
}

fn runtime_with_responses(
    artifact_dir: &std::path::Path,
    responses: Vec<(&str, FauxResponse)>,
    max_concurrency: usize,
    mailbox_capacity: usize,
    max_recursion_depth: usize,
) -> (
    OrchestrationRuntime,
    Vec<pi_ai::providers::FauxProviderRegistration>,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("orchestration-model-{suffix}"),
        name: "Orchestration Test".to_owned(),
        api: format!("orchestration-api-{suffix}"),
        provider: format!("orchestration-provider-{suffix}"),
        ..Model::default()
    };
    let mut registrations = Vec::new();
    let mut by_id = HashMap::new();
    for (index, (child_id, response)) in responses.into_iter().enumerate() {
        let mut child_model = model.clone();
        child_model.api = format!("{}-{index}", model.api);
        child_model.provider = format!("{}-{index}", model.provider);
        let registration = register_faux_provider(FauxProviderOptions {
            api: child_model.api.clone(),
            provider: child_model.provider.clone(),
            models: vec![child_model],
            chunk_size: 1,
        });
        registration.set_responses(vec![response]);
        by_id.insert(child_id.to_owned(), registration.clone());
        registrations.push(registration);
    }
    let factory = faux_factory(Arc::new(Mutex::new(by_id)));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifact_dir,
    );
    config.max_concurrency = max_concurrency;
    config.mailbox_capacity = mailbox_capacity;
    config.max_recursion_depth = max_recursion_depth;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    (runtime, registrations)
}

#[test]
fn discovery_is_first_wins_and_project_trust_gated() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("project");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(cwd.join(".pi/agents")).expect("project agents");
    std::fs::create_dir_all(agent_dir.join("agents")).expect("user agents");
    std::fs::write(
        cwd.join(".pi/agents/task.md"),
        "---\nname: task\ndescription: project\nautoloadSkills: [rust]\n---\nproject prompt",
    )
    .expect("project definition");
    std::fs::write(
        agent_dir.join("agents/task.md"),
        "---\nname: task\ndescription: user\n---\nuser prompt",
    )
    .expect("user definition");
    std::fs::write(
        agent_dir.join("agents/review.md"),
        "---\nname: review\ndescription: review\ntools:\n  - read\n  - grep\n---\nreview prompt",
    )
    .expect("review definition");

    let mut options = AgentDiscoveryOptions::new(&cwd, &agent_dir);
    let untrusted = AgentCatalog::discover(&options).expect("untrusted catalog");
    assert_eq!(untrusted.get("task").expect("task").description, "user");
    assert!(untrusted.get("review").is_some());
    assert!(untrusted.agents().iter().all(|agent| agent.trusted));

    options.project_trusted = true;
    let trusted = AgentCatalog::discover(&options).expect("trusted catalog");
    let task = trusted.get("task").expect("project task");
    assert_eq!(task.description, "project");
    assert_eq!(task.autoload_skills, vec!["rust"]);
    assert_eq!(task.source, AgentDefinitionSource::Project);
}

#[tokio::test]
async fn orchestration_ignores_unknown_tools_without_blocking_any_agents() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut architect = definition("architect");
    architect.tools = Some(vec![
        "read".to_owned(),
        "unsupported_child_tool".to_owned(),
        "yield_output".to_owned(),
    ]);
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![architect, definition("task")]),
            artifacts.path(),
        ),
        Arc::new(|_| Box::pin(async { Err(anyhow::anyhow!("stop after capture")) })),
    )
    .expect("an unknown-tool agent must not block runtime startup");

    // OMP alignment: unknown declared tools never make an agent unavailable —
    // the architect is advertised alongside task.
    assert_eq!(
        runtime
            .enabled_agents()
            .into_iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        vec!["architect", "task"],
    );
    let task_tool = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    assert!(task_tool.description.contains("task —"));
    assert!(
        task_tool.description.contains("architect —"),
        "architect must be advertised in the task tool description: {}",
        task_tool.description
    );

    // The compatibility channel is model-only now; the unknown-tool report
    // lives on the dedicated deduped-warning channel, recorded at spawn.
    assert!(
        runtime.incompatible_agent_diagnostics().is_empty(),
        "unknown tools are not incompatibilities"
    );

    // Spawning the unknown-tool agent succeeds: no batch abort, no error.
    let spawns = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ArchitectChild".to_owned(),
                agent: "architect".to_owned(),
                assignment: "design the system".to_owned(),
                todo_task_id: None,
                ..TaskItem::default()
            }],
        )
        .expect("an unknown-tool agent must spawn");
    assert_eq!(spawns.len(), 1);
    // Let the async child run settle (the factory errors by design) so the
    // spawned task finishes before the runtime drops.
    runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(std::time::Duration::from_secs(10)),
            None,
        )
        .await
        .expect("job settles");

    // The unknown-tool warnings fire once per (agent, tool) — the architect
    // has two unknown names, so exactly two messages, each naming the agent.
    let warnings = runtime.unknown_tool_warnings();
    assert_eq!(warnings.len(), 2, "one warning per unknown tool: {warnings:?}");
    assert!(
        warnings.iter().all(|warning| warning.contains("architect")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("unsupported_child_tool")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("yield_output")),
        "{warnings:?}"
    );
    // Repeated spawns do not re-warn: still exactly two entries.
    assert_eq!(runtime.unknown_tool_warnings().len(), 2);
}

#[tokio::test]
async fn user_task_spawns_child_that_invokes_configured_glob() {
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir_all(root.path().join("src/nested")).expect("source tree");
    std::fs::write(root.path().join("src/lib.rs"), "pub fn lib() {}\n").expect("lib source");
    std::fs::write(root.path().join("src/nested/mod.rs"), "pub mod nested {}\n")
        .expect("nested source");
    std::fs::write(root.path().join("src/ignored.txt"), "ignored\n").expect("non-match");

    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("glob-child-{suffix}"),
        name: "Glob Child".to_owned(),
        api: format!("glob-child-api-{suffix}"),
        provider: format!("glob-child-provider-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 16,
    });
    registration.set_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "glob-call".to_owned(),
                name: "glob".to_owned(),
                arguments: serde_json::json!({ "pattern": "**/*.rs", "path": "src" }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::text("glob completed"),
    ]);

    let parent = Session::new(SessionOptions {
        model: model.clone(),
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("parent");
    let mut agent = definition("task");
    agent.tools = Some(vec!["glob".to_owned()]);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent]),
        root.path().join("artifacts"),
    );
    config.parent_model = model;
    let runtime = OrchestrationRuntime::new(
        config,
        OrchestrationRuntime::child_factory_from_session(&parent),
    )
    .expect("runtime");

    let task = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    let (_, abort) = AbortController::new();
    let spawn_result = (task.execute)(ToolCallContext {
        tool_call_id: "user-task".to_owned(),
        arguments: serde_json::json!({
            "name": "GlobChild",
            "agent": "task",
            "task": "Find every Rust source file under src"
        }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    })
    .await
    .expect("user-triggered task spawn");
    let spawns: Vec<pi_coding::TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("task spawn details");
    let jobs = runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(std::time::Duration::from_secs(5)),
            None,
        )
        .await
        .expect("child completion");
    let result = jobs[0].result.as_ref().expect("settled result");
    assert_eq!(
        result.output,
        format!("glob completed\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    assert!(result.error.is_none(), "{:?}", result.error);

    let history = runtime
        .resolve_history_reference("GlobChild")
        .expect("history path");
    let messages: Vec<Message> = serde_json::from_slice(
        &std::fs::read(history).expect("read child history"),
    )
    .expect("parse child history");
    let glob_result = messages.iter().find_map(|message| match message {
        Message::ToolResult(result) if result.tool_name == "glob" => Some(result),
        _ => None,
    });
    let glob_result = glob_result.expect("glob tool result in child history");
    assert!(!glob_result.is_error);
    let text = glob_result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.contains("lib.rs"), "{text}");
    assert!(text.contains("nested/mod.rs"), "{text}");
    assert!(!text.contains("ignored.txt"), "{text}");

    runtime.shutdown().await;
    registration.unregister();
}

fn hub_tool_call(id: &str, arguments: serde_json::Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: "hub".to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

#[tokio::test]
async fn real_task_children_route_main_alpha_beta_main_through_owned_hub_tools() {
    const MAIN_TO_ALPHA: &str = "main-to-alpha-authoritative";
    const ALPHA_TO_BETA: &str = "alpha-to-beta-authoritative";
    const BETA_TO_MAIN: &str = "beta-to-main-authoritative";
    let artifacts = tempfile::tempdir().expect("artifacts");
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("owned-hub-model-{suffix}"),
        name: "Owned Hub E2E".to_owned(),
        api: format!("owned-hub-api-{suffix}"),
        provider: format!("owned-hub-provider-{suffix}"),
        ..Model::default()
    };
    let register = |child: &str, responses: Vec<FauxResponse>| {
        let api = format!("{}-{child}", model.api);
        let provider = format!("{}-{child}", model.provider);
        let id = format!("{}-{child}", model.id);
        let registration = register_faux_provider(FauxProviderOptions {
            api: api.clone(),
            provider: provider.clone(),
            models: vec![Model { id, api, provider, ..model.clone() }],
            chunk_size: 64,
        });
        registration.set_responses(responses);
        registration
    };
    let alpha_registration = register("alpha", vec![
        hub_tool_call("alpha-wait-main", serde_json::json!({"op":"wait","from":"Main","timeoutMs":2000})),
        hub_tool_call("alpha-send-beta", serde_json::json!({"op":"send","to":"Beta","message":ALPHA_TO_BETA})),
        FauxResponse::text("Alpha relayed through its owned hub tool"),
    ]);
    let beta_registration = register("beta", vec![
        hub_tool_call("beta-wait-alpha", serde_json::json!({"op":"wait","from":"Alpha","timeoutMs":2000})),
        hub_tool_call("beta-send-main", serde_json::json!({"op":"send","to":"Main","message":BETA_TO_MAIN})),
        FauxResponse::text("Beta relayed through its owned hub tool"),
    ]);
    let mut alpha_definition = definition("alpha");
    alpha_definition.model = Some(vec![format!("{}-alpha", model.id)]);
    let mut beta_definition = definition("beta");
    beta_definition.model = Some(vec![format!("{}-beta", model.id)]);
    let alpha_wait_ready = Arc::new(tokio::sync::Notify::new());
    let beta_wait_ready = Arc::new(tokio::sync::Notify::new());
    let mut wait_readiness: HashMap<String, Arc<tokio::sync::Notify>> = HashMap::new();
    wait_readiness.insert("Alpha".to_owned(), alpha_wait_ready.clone());
    wait_readiness.insert("Beta".to_owned(), beta_wait_ready.clone());
    let mut registrations = HashMap::new();
    registrations.insert("Alpha".to_owned(), alpha_registration.clone());
    registrations.insert("Beta".to_owned(), beta_registration.clone());
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![alpha_definition, beta_definition]),
        artifacts.path(),
    );
    config.default_agent = "alpha".to_owned();
    config.max_concurrency = 2;
    config.mailbox_capacity = 8;
    config.max_recursion_depth = 2;
    config.parent_model = model;
    let runtime = OrchestrationRuntime::new(config, faux_factory_with_wait_readiness(Arc::new(Mutex::new(registrations)), Arc::new(Mutex::new(wait_readiness)))).expect("runtime");
    let tools = runtime.agent_tools("Main", 0);
    let task = tools.iter().find(|tool| tool.name == "task").expect("task tool");
    let hub = tools.iter().find(|tool| tool.name == "hub").expect("hub tool");
    let context = |id: &str, arguments| {
        let (_, abort) = AbortController::new();
        ToolCallContext { tool_call_id: id.to_owned(), arguments, on_update: Arc::new(|_| {}), abort, model: None }
    };
    let spawned = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        (task.execute)(context("spawn-alpha-beta", serde_json::json!({
            "context": "Hub relay exercise: Main coordinates; each child waits for a hub message then relays the exact body onward. Do not settle before the relay completes.",
            "tasks":[
                {"name":"Alpha","agent":"alpha","task":"Wait for Main, then relay to Beta."},
                {"name":"Beta","agent":"beta","task":"Wait for Alpha, then relay to Main."}
            ]
        }))),
    ).await.expect("task returns before children settle").expect("spawn children");
    let mut spawns: Vec<pi_coding::TaskSpawn> = serde_json::from_value(spawned.details).expect("spawn details");
    spawns.sort_by_key(|spawn| spawn.index);
    assert_eq!((spawns[0].agent_id.as_str(), spawns[1].agent_id.as_str()), ("Alpha", "Beta"));
    assert_ne!(spawns[0].job_id, spawns[1].job_id);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(alpha_wait_ready.notified(), beta_wait_ready.notified());
    })
    .await
    .expect("both children register hub wait before Main sends");
    let main_send = (hub.execute)(context("main-send-alpha", serde_json::json!({"op":"send","to":"Alpha","message":MAIN_TO_ALPHA}))).await.expect("Main sends Alpha");
    assert_eq!(main_send.details["receipts"][0]["to"], "Alpha");
    assert_ne!(main_send.details["receipts"][0]["outcome"], "failed");
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        (hub.execute)(context("main-wait-beta", serde_json::json!({"op":"wait","from":"Beta","timeoutMs":2500}))),
    ).await.expect("bounded Beta wait").expect("Main waits through hub");
    assert_eq!(reply.details["message"]["from"], "Beta");
    assert_eq!(reply.details["message"]["to"], "Main");
    assert_eq!(reply.details["message"]["body"], BETA_TO_MAIN);
    let job_ids = spawns.iter().map(|spawn| spawn.job_id.clone()).collect::<Vec<_>>();
    let jobs = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let jobs = runtime.jobs(Some(&job_ids));
            if jobs.len() == 2 && jobs.iter().all(|job| job.status.is_settled()) {
                break jobs;
            }
            tokio::task::yield_now().await;
        }
    }).await.expect("both jobs settle");
    assert!(jobs.iter().all(|job| job.status == pi_coding::JobStatus::Completed));
    let history = |id: &str| -> Vec<Message> {
        serde_json::from_slice(&std::fs::read(runtime.resolve_history_reference(id).expect("history path")).expect("read history")).expect("parse history")
    };
    let alpha_history = history("Alpha");
    let beta_history = history("Beta");
    fn hub_results(messages: &[Message]) -> Vec<&pi_ai::ToolResultMessage> {
        messages.iter().filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == "hub" => Some(result),
            _ => None,
        }).collect()
    }
    let alpha_hub = hub_results(&alpha_history);
    let beta_hub = hub_results(&beta_history);
    assert_eq!(alpha_hub.len(), 2);
    assert_eq!(alpha_hub[0].details.as_ref().expect("Alpha wait details")["message"]["from"], "Main");
    assert_eq!(alpha_hub[0].details.as_ref().expect("Alpha wait details")["message"]["body"], MAIN_TO_ALPHA);
    assert_eq!(alpha_hub[1].details.as_ref().expect("Alpha send details")["receipts"][0]["to"], "Beta");
    assert_eq!(beta_hub[0].details.as_ref().expect("Beta wait details")["message"]["from"], "Alpha");
    assert_eq!(beta_hub[0].details.as_ref().expect("Beta wait details")["message"]["body"], ALPHA_TO_BETA);
    assert_eq!(beta_hub[1].details.as_ref().expect("Beta send details")["receipts"][0]["to"], "Main");
    assert!(runtime.inbox("Alpha", true).is_empty());
    assert!(runtime.inbox("Beta", true).is_empty());
    assert!(runtime.inbox("Main", true).is_empty());
    runtime.shutdown().await;
    alpha_registration.unregister();
    beta_registration.unregister();
}

#[tokio::test]
async fn selector_settings_choose_the_deterministic_matching_agent() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut review = definition("reviewer");
    review.description = "Reviews patches for correctness and security".to_owned();
    let config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task"), review]),
        artifacts.path(),
    )
    .with_selector_settings(pi_coding::SelectorSettings {
        auto_select_threshold: 1,
        confidence_margin: 0,
        min_score: 0,
        ..pi_coding::SelectorSettings::default()
    });
    let runtime = OrchestrationRuntime::new(config, Arc::new(|_| {
        Box::pin(async { panic!("selector test must not create a child") })
    }))
    .expect("runtime");
    assert_eq!(runtime.select_agent("review this security patch", None), "reviewer");
    assert_eq!(runtime.select_agent("review this security patch", Some("task")), "task");
    runtime.shutdown().await;
}

#[tokio::test]
async fn parent_factory_disables_ambient_discovery_and_forbidden_child_tools() {
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir_all(root.path().join(".pi/skills/local")).expect("skill dir");
    std::fs::write(
        root.path().join(".pi/skills/local/SKILL.md"),
        "---\nname: local\ndescription: local skill\n---\nlocal",
    )
    .expect("skill");
    let parent = Session::new(SessionOptions {
        model: Model::default(),
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("parent");
    let mut child_definition = definition("child");
    child_definition.tools = Some(vec!["read".to_owned(), "todo".to_owned(), "process".to_owned(), "task".to_owned(), "hub".to_owned(), "goal".to_owned()]);
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(pi_coding::ChildSessionRequest {
        child_id: "Child".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 16,
        depth: 1,
        definition: child_definition,
        assignment: "inspect".to_owned(),
        system_prompt: "child".to_owned(),
        requested_tool_names: Some(vec!["read".to_owned(), "todo".to_owned(), "process".to_owned(), "task".to_owned(), "hub".to_owned(), "goal".to_owned()]),
        orchestration_tools: Vec::new(),
        thinking_level: None,
        model: parent.model().unwrap_or_default(),
        yield_state: Arc::new(pi_coding::YieldState::default()),
    output_schema: None,
    schema_mode: None,
    })
    .await
    .expect("child");
    // Orchestration plumbing is auto-provided: todo/process/task/hub/goal are
    // never ambient-discovered, and the child-only `yield` tool is appended.
    assert_eq!(child.get_active_tool_names(), vec!["read", "yield"]);
    assert!(child.select_for_request("local skill").await.skills.is_empty());
}

#[tokio::test]
async fn child_factory_resolves_auth_for_a_different_provider() {
    let root = tempfile::tempdir().expect("root");
    let parent_model = Model {
        id: "parent-model".to_owned(),
        name: "Parent Model".to_owned(),
        api: "parent-api".to_owned(),
        provider: "parent-provider".to_owned(),
        ..Model::default()
    };
    let child_model = Model {
        id: "child-model".to_owned(),
        name: "Child Model".to_owned(),
        api: "child-api".to_owned(),
        provider: "child-provider".to_owned(),
        ..Model::default()
    };
    let resolved_models = Arc::new(Mutex::new(Vec::<Model>::new()));
    let observed_models = resolved_models.clone();
    let resolver: pi_coding::SessionAuthResolver = Arc::new(move |model| {
        observed_models.lock().push(model);
        Box::pin(async {
            Ok(pi_coding::RequestAuth {
                api_key: "child-secret".to_owned(),
                ..pi_coding::RequestAuth::default()
            })
        })
    });
    let parent = Session::new(SessionOptions {
        model: parent_model,
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "parent-secret-must-not-leak".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: Some(resolver),
    })
    .expect("parent");
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(
        pi_coding::ChildSessionRequest {
            child_id: "Child".to_owned(),
            parent_id: "Main".to_owned(),
            max_tools_per_agent: 16,
            depth: 1,
            definition: definition("child"),
            assignment: "inspect".to_owned(),
            system_prompt: "child".to_owned(),
            requested_tool_names: Some(Vec::new()),
            orchestration_tools: Vec::new(),
            thinking_level: None,
            model: child_model.clone(),
            yield_state: Arc::new(pi_coding::YieldState::default()),
        output_schema: None,
        schema_mode: None,
        },
    )
    .await
    .expect("child");
    assert_eq!(child.current_api_key(), "child-secret");
    assert_eq!(resolved_models.lock().as_slice(), &[child_model]);
}

#[tokio::test]
async fn child_factory_rejects_cross_provider_reuse_without_auth_resolver() {
    let root = tempfile::tempdir().expect("root");
    let parent = Session::new(SessionOptions {
        model: Model {
            id: "parent-model".to_owned(),
            name: "Parent Model".to_owned(),
            api: "parent-api".to_owned(),
            provider: "parent-provider".to_owned(),
            ..Model::default()
        },
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "parent-secret-must-not-leak".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("parent");
    let error = match OrchestrationRuntime::child_factory_from_session(&parent)(
        pi_coding::ChildSessionRequest {
            child_id: "Child".to_owned(),
            parent_id: "Main".to_owned(),
            max_tools_per_agent: 16,
            depth: 1,
            definition: definition("child"),
            assignment: "inspect".to_owned(),
            system_prompt: "child".to_owned(),
            requested_tool_names: Some(Vec::new()),
            orchestration_tools: Vec::new(),
            thinking_level: None,
            model: Model {
                id: "child-model".to_owned(),
                name: "Child Model".to_owned(),
                api: "child-api".to_owned(),
                provider: "child-provider".to_owned(),
                ..Model::default()
            },
            yield_state: Arc::new(pi_coding::YieldState::default()),
        output_schema: None,
        schema_mode: None,
        },
    )
    .await
    {
        Ok(_) => panic!("cross-provider child requires its own credential"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("no auth resolver"));
}

#[test]
fn orchestration_settings_are_explicitly_off_by_default() {
    let runtime = pi_coding::Settings::default().runtime_settings().expect("runtime settings");
    assert!(!runtime.orchestration_enabled);
    assert!(!runtime.process_tool_enabled);
    // The todo tool is on by default (OMP parity); `orchestration.todo: false`
    // opts out. Only orchestration itself and its process tool default off.
    assert!(runtime.todo_tool_enabled);
}

#[tokio::test]
async fn batch_results_are_correlated_and_artifacts_resolve() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let response = match request.child_id.as_str() {
            "First" => "first",
            "Second" => "second",
            id => panic!("unexpected child {id}"),
        }
        .to_owned();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let response = response.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text(response));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    );
    config.max_concurrency = 2;
    config.max_recursion_depth = 2;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let (_, abort) = AbortController::new();
    let results = runtime
        .run_tasks(
            "Main",
            0,
            vec![
                TaskItem {
                    index: 1,
                    id: "Second".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "second assignment".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                },
                TaskItem {
                    index: 0,
                    id: "First".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "first assignment".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                },
            ],
            abort,
        )
        .await
        .expect("batch");
    assert_eq!(
        results
            .iter()
            .map(|result| result.index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        results[0].output,
        format!("first\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    assert_eq!(
        results[1].output,
        format!("second\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    for result in &results {
        assert_eq!(result.status, AgentStatus::Idle);
        assert!(
            runtime
                .resolve_agent_reference(&result.id)
                .expect("agent ref")
                .is_file()
        );
        assert!(
            runtime
                .resolve_history_reference(&result.id)
                .expect("history ref")
                .is_file()
        );
        assert_eq!(
            runtime.resolve_read_uri(&result.artifact_ref).expect("artifact URI"),
            runtime.resolve_agent_reference(&result.id).expect("agent ref")
        );
        assert_eq!(
            runtime.resolve_read_uri(&result.history_ref).expect("history URI"),
            runtime.resolve_history_reference(&result.id).expect("history ref")
        );
        assert_eq!(result.artifact_uri, format!("artifact://{}", result.id));
        assert_eq!(
            runtime.resolve_read_uri(&result.artifact_uri).expect("artifact alias"),
            runtime.resolve_agent_reference(&result.id).expect("agent ref")
        );
    }
    let read = pi_coding::read_tool_with_resolver(
        artifacts.path().to_str().expect("artifact cwd"),
        None,
        Some(runtime.read_uri_resolver()),
    );
    let (_, read_abort) = AbortController::new();
    let read_result = (read.execute)(pi_agent::ToolCallContext {
        tool_call_id: "read-artifact".to_owned(),
        arguments: serde_json::json!({ "path": results[0].artifact_ref }),
        on_update: Arc::new(|_| {}),
        abort: read_abort,
        model: None,
    })
    .await
    .expect("read artifact URI");
    assert!(matches!(
        read_result.content.first(),
        Some(pi_ai::ContentBlock::Text { text, .. }) if text.contains("first")
    ));
    assert_eq!(
        runtime.send("First", "Second", "peer result", None)[0].outcome,
        pi_coding::DeliveryOutcome::Woken
    );
    let sibling_inbox = runtime.inbox("Second", false);
    assert_eq!(sibling_inbox.len(), 1);
    assert_eq!(sibling_inbox[0].from, "First");
    assert_eq!(sibling_inbox[0].body, "peer result");
    runtime.shutdown().await;
}

#[tokio::test]
async fn batch_never_exceeds_configured_concurrency() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let model = Model {
        id: "concurrency-model".to_owned(),
        name: "Concurrency Model".to_owned(),
        api: "concurrency-api".to_owned(),
        provider: "concurrency-provider".to_owned(),
        ..Model::default()
    };
    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = tokio_util::sync::CancellationToken::new();
    let observed_peak = peak.clone();
    let observed_started = started.clone();
    let release_all = release.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let current = current.clone();
        let peak = peak.clone();
        let started = started.clone();
        let release = release.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let current = current.clone();
                let peak = peak.clone();
                let started = started.clone();
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(active, Ordering::SeqCst);
                        started.notify_waiters();
                        release.cancelled().await;
                        current.fetch_sub(1, Ordering::SeqCst);
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    );
    config.max_concurrency = 2;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let (_, abort) = AbortController::new();
    let runner = runtime.clone();
    let run = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                (0..3)
                    .map(|index| TaskItem {
                        index,
                        id: format!("Concurrent{index}"),
                        agent: "task".to_owned(),
                        assignment: format!("work {index}"),
                        todo_task_id: None,
                        ..Default::default()
                    })
                    .collect(),
                abort,
            )
            .await
            .expect("batch")
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let notified = observed_started.notified();
            if observed_peak.load(Ordering::SeqCst) == 2 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("two children started");
    assert_eq!(observed_peak.load(Ordering::SeqCst), 2);
    release_all.cancel();
    assert_eq!(run.await.expect("run join").len(), 3);
    assert_eq!(observed_peak.load(Ordering::SeqCst), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn mailbox_cap_wait_and_peer_roster_are_enforced() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![("Worker", FauxResponse::text("done"))],
        1,
        2,
        2,
    );
    let (_, abort) = AbortController::new();
    runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Worker".to_owned(),
                agent: "task".to_owned(),
                assignment: "finish".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect("child");

    assert_eq!(runtime.send("Main", "Worker", "one", None)[0].outcome, pi_coding::DeliveryOutcome::Woken);
    assert_eq!(runtime.send("Main", "Worker", "two", None)[0].outcome, pi_coding::DeliveryOutcome::Woken);
    assert_eq!(runtime.send("Main", "Worker", "three", None)[0].outcome, pi_coding::DeliveryOutcome::Failed);
    let inbox = runtime.inbox("Worker", true);
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].body, "one");
    assert_eq!(inbox[1].body, "two");

    let wait_runtime = runtime.clone();
    let waiter = tokio::spawn(async move {
        wait_runtime
            .wait_message(
                "Main",
                Some("Worker"),
                Some(std::time::Duration::from_secs(1)),
                None,
            )
            .await
            .expect("wait")
            .expect("message")
    });
    tokio::task::yield_now().await;
    assert_eq!(runtime.send("Worker", "Main", "hello main", None)[0].outcome, pi_coding::DeliveryOutcome::Woken);
    assert_eq!(waiter.await.expect("wait join").body, "hello main");
    let peers = runtime.list("Worker");
    assert!(peers.iter().any(|peer| peer.id == "Main"));

    runtime.shutdown().await;
    for registration in registrations {
        registration.unregister();
    }
}

#[tokio::test]
async fn recursion_cutoff_removes_task_and_shutdown_cleans_children() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![("Unused", FauxResponse::text("unused"))],
        1,
        100,
        1,
    );
    let root_tools = runtime.agent_tools("Main", 0);
    assert_eq!(
        root_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["task", "hub"]
    );
    let child_tools = runtime.agent_tools("Child", 1);
    assert_eq!(
        child_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["hub"]
    );
    runtime.shutdown().await;
    assert_eq!(runtime.active_child_count(), 0);
    for registration in registrations {
        registration.unregister();
    }
}

#[tokio::test]
async fn cancellation_aborts_a_running_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("cancel-model-{suffix}"),
        name: "Cancel Model".to_owned(),
        api: format!("cancel-api-{suffix}"),
        provider: format!("cancel-provider-{suffix}"),
        ..Model::default()
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::new(AtomicUsize::new(0));
    let stream_started = started.clone();
    let stream_observed = observed.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let observed = stream_observed.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                if let Some(abort) = options.stream.abort_signal {
                    abort.cancelled().await;
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.stop_reason = pi_ai::StopReason::Aborted;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition("task")]),
            artifacts.path(),
        ),
        factory,
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let run_runtime = runtime.clone();
    let run = tokio::spawn(async move {
        run_runtime
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "CancelMe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort,
            )
            .await
            .expect("cancel batch")
    });
    started.notified().await;
    assert_eq!(runtime.cancel(&["CancelMe".to_owned()]), vec!["CancelMe"]);
    let results = run.await.expect("run join");
    assert_eq!(results[0].status, AgentStatus::Aborted);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    runtime.shutdown().await;
    assert_eq!(runtime.active_child_count(), 0);
}

#[tokio::test]
async fn dropping_runtime_owner_cancels_children() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let model = Model {
        id: "drop-model".to_owned(),
        name: "Drop Model".to_owned(),
        api: "drop-api".to_owned(),
        provider: "drop-provider".to_owned(),
        ..Model::default()
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let aborted = Arc::new(tokio::sync::Notify::new());
    let stream_started = started.clone();
    let stream_aborted = aborted.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let aborted = stream_aborted.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                options
                    .stream
                    .abort_signal
                    .expect("abort signal")
                    .cancelled()
                    .await;
                aborted.notify_waiters();
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.stop_reason = pi_ai::StopReason::Aborted;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition("task")]),
            artifacts.path(),
        ),
        factory,
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let runner = runtime.clone();
    let task = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "DropMe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort,
            )
            .await
    });
    started.notified().await;
    drop(runtime);
    tokio::time::timeout(std::time::Duration::from_secs(1), aborted.notified())
        .await
        .expect("child observed owner drop");
    let results = task.await.expect("task join").expect("task results");
    assert_eq!(results[0].status, AgentStatus::Aborted);
}

// ---------------------------------------------------------------------------
// IRC lifecycle (registry-only): truthful delivery outcomes, idle->park,
// retained parked mail and artifact/history refs, mailbox bounds, shutdown
// cleanup, no cross-group delivery, and Main never parked. The child Session
// is dropped after run_one completes, so delivery to Parked cannot claim that
// execution resumed; an explicit future resume may consume the retained mail.
// ---------------------------------------------------------------------------

fn hanging_factory() -> (
    ChildSessionFactory,
    Arc<tokio::sync::Notify>,
    tokio_util::sync::CancellationToken,
) {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = tokio_util::sync::CancellationToken::new();
    let stream_started = started.clone();
    let stream_release = release.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let release = stream_release.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                let abort = options.stream.abort_signal;
                tokio::select! {
                    () = release.cancelled() => {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    }
                    () = async {
                        match abort {
                            Some(signal) => signal.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = pi_ai::StopReason::Aborted;
                        producer.end(Some(message)).await;
                    }
                }
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model: Model::default(),
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    (factory, started, release)
}

fn config_with_ttl(
    artifacts: &std::path::Path,
    idle_ttl: Option<std::time::Duration>,
) -> OrchestrationConfig {
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts,
    );
    config.idle_ttl = idle_ttl;
    config
}

fn single_child_runtime(
    artifacts: &std::path::Path,
    child_id: &str,
    response: FauxResponse,
    idle_ttl: Option<std::time::Duration>,
) -> (
    OrchestrationRuntime,
    pi_ai::providers::FauxProviderRegistration,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("lifecycle-model-{suffix}"),
        name: "Lifecycle Model".to_owned(),
        api: format!("lifecycle-api-{suffix}"),
        provider: format!("lifecycle-provider-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });
    registration.set_responses(vec![response]);
    let mut by_id = HashMap::new();
    by_id.insert(child_id.to_owned(), registration.clone());
    let factory = faux_factory(Arc::new(parking_lot::Mutex::new(by_id)));
    let runtime = OrchestrationRuntime::new(config_with_ttl(artifacts, idle_ttl), factory)
        .expect("runtime");
    (runtime, registration)
}

async fn run_one_child(runtime: &OrchestrationRuntime, child_id: &str) {
    let (_, abort) = AbortController::new();
    runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: child_id.to_owned(),
                agent: "task".to_owned(),
                assignment: "finish".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect("child batch");
}

fn context_user_texts(context: &pi_ai::Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::Custom(custom) => Some(
                custom
                    .content
                    .to_blocks()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn running_child_observes_mid_run_steering_exactly_once() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let started = Arc::new(tokio::sync::Notify::new());
    let release_first = tokio_util::sync::CancellationToken::new();
    let contexts = Arc::new(Mutex::new(Vec::<pi_ai::Context>::new()));
    let stream_started = started.clone();
    let stream_release = release_first.clone();
    let stream_contexts = contexts.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_calls = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let started = stream_started.clone();
        let release = stream_release.clone();
        let contexts = stream_contexts.clone();
        let calls = stream_calls.clone();
        Box::pin(async move {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            contexts.lock().push(context);
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                if call == 0 {
                    started.notify_waiters();
                    let abort = options.stream.abort_signal;
                    tokio::select! {
                        () = release.cancelled() => {}
                        () = async {
                            match abort {
                                Some(signal) => signal.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {}
                    }
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text(if call == 0 {
                    "initial response"
                } else {
                    "urgent instruction followed"
                }));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model: Model::default(),
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(config_with_ttl(artifacts.path(), None), factory)
        .expect("runtime");
    let runner = runtime.clone();
    let run = tokio::spawn(async move {
        let (_, abort) = AbortController::new();
        runner
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Steered".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "start with the original plan".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort,
            )
            .await
    });
    started.notified().await;
    let receipt = runtime.send(
        "Main",
        "Steered",
        "URGENT: replace the original plan with the new instruction",
        None,
    );
    assert_eq!(receipt[0].outcome, pi_coding::DeliveryOutcome::Woken);
    release_first.cancel();
    let results = run.await.expect("run join").expect("run result");
    assert_eq!(
        results[0].output,
        format!("urgent instruction followed\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let captured = contexts.lock();
    let second = captured.get(1).expect("steered provider request");
    let steering = context_user_texts(second)
        .into_iter()
        .filter(|text| text.contains("URGENT: replace the original plan"))
        .collect::<Vec<_>>();
    assert_eq!(steering.len(), 1, "steering must reach provider exactly once");
    assert!(runtime.inbox("Steered", true).is_empty());
    runtime.shutdown().await;
}

#[tokio::test]
async fn cancellation_race_does_not_report_or_deliver_fake_wake() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (factory, started, _release) = hanging_factory();
    let runtime = OrchestrationRuntime::new(config_with_ttl(artifacts.path(), None), factory)
        .expect("runtime");
    let runner = runtime.clone();
    let run = tokio::spawn(async move {
        let (_, abort) = AbortController::new();
        runner
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Race".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "remain active".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort,
            )
            .await
    });
    started.notified().await;
    assert_eq!(runtime.cancel(&["Race".to_owned()]), vec!["Race"]);
    let receipt = runtime.send("Main", "Race", "too late", None)[0].clone();
    let results = run.await.expect("race join").expect("race result");
    assert_eq!(results[0].status, AgentStatus::Aborted);
    assert_ne!(receipt.outcome, pi_coding::DeliveryOutcome::Woken);
    if receipt.outcome == pi_coding::DeliveryOutcome::Queued {
        assert_eq!(runtime.inbox("Race", false).len(), 1);
    } else {
        assert_eq!(receipt.outcome, pi_coding::DeliveryOutcome::Failed);
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn delivery_outcomes_queued_woken_parked_failed() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (factory, started, release) = hanging_factory();
    let runtime =
        OrchestrationRuntime::new(config_with_ttl(artifacts.path(), None), factory).expect("runtime");
    let (_, abort) = AbortController::new();
    let runner = runtime.clone();
    let run = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Runner".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "hang".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort,
            )
            .await
    });
    started.notified().await;

    // Woken: target is Running and the active child delivery bridge accepts it.
    assert_eq!(
        runtime.send("Main", "Runner", "while running", None)[0].outcome,
        pi_coding::DeliveryOutcome::Woken,
    );
    // Failed: unknown target.
    assert_eq!(
        runtime.send("Main", "NoSuchAgent", "x", None)[0].outcome,
        pi_coding::DeliveryOutcome::Failed,
    );

    release.cancel();
    let results = run.await.expect("run join").expect("results");
    assert_eq!(results[0].status, AgentStatus::Idle);

    // Woken: target is now Idle.
    assert_eq!(
        runtime.send("Main", "Runner", "after completion", None)[0].outcome,
        pi_coding::DeliveryOutcome::Woken,
    );

    // A parked registry entry has no live execution to revive. Delivery is
    // retained for a future explicit resume and the status remains truthful.
    runtime.park("Runner").expect("park");
    let runner_snapshot = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Runner")
        .expect("runner listed");
    assert_eq!(runner_snapshot.status, AgentStatus::Parked);
    assert_eq!(
        runtime.send("Main", "Runner", "deliver while parked", None)[0].outcome,
        pi_coding::DeliveryOutcome::Queued,
    );
    let still_parked = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Runner")
        .expect("runner listed after parked delivery");
    assert_eq!(still_parked.status, AgentStatus::Parked);

    // Failed: aborted target.
    let (factory2, started2, _release2) = hanging_factory();
    let runtime2 =
        OrchestrationRuntime::new(config_with_ttl(artifacts.path(), None), factory2).expect("runtime2");
    let (_, abort2) = AbortController::new();
    let runner2 = runtime2.clone();
    let run2 = tokio::spawn(async move {
        runner2
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "CancelMe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "hang".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
                abort2,
            )
            .await
    });
    started2.notified().await;
    assert_eq!(runtime2.cancel(&["CancelMe".to_owned()]), vec!["CancelMe"]);
    let results2 = run2.await.expect("run2 join").expect("results2");
    assert_eq!(results2[0].status, AgentStatus::Aborted);
    assert_eq!(
        runtime2.send("Main", "CancelMe", "post abort", None)[0].outcome,
        pi_coding::DeliveryOutcome::Failed,
    );
    runtime2.shutdown().await;
    runtime.shutdown().await;
}

#[tokio::test]
async fn idle_agent_auto_parks_after_ttl() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registration) = single_child_runtime(
        artifacts.path(),
        "Worker",
        FauxResponse::text("done"),
        Some(std::time::Duration::from_millis(60)),
    );
    run_one_child(&runtime, "Worker").await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let worker = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Worker")
        .expect("worker listed");
    assert_eq!(worker.status, AgentStatus::Parked);
    assert!(worker.artifact_ref.is_some());
    assert!(worker.history_ref.is_some());
    // Main must NOT auto-park even though it has been Idle the whole time.
    let main = runtime
        .list("Worker")
        .into_iter()
        .find(|peer| peer.id == "Main")
        .expect("main listed");
    assert_eq!(main.status, AgentStatus::Idle);
    registration.unregister();
    runtime.shutdown().await;
}

#[tokio::test]
async fn parked_agent_delivery_retains_registry_refs_without_claiming_revival() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registration) = single_child_runtime(
        artifacts.path(),
        "Survivor",
        FauxResponse::text("revivable output"),
        Some(std::time::Duration::from_millis(60)),
    );
    let results = runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Survivor".to_owned(),
                agent: "task".to_owned(),
                assignment: "produce".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            {
                let (_, abort) = AbortController::new();
                abort
            },
        )
        .await
        .expect("child");
    let history_ref = results[0].history_ref.clone();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let parked = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Survivor")
        .expect("survivor listed");
    assert_eq!(parked.status, AgentStatus::Parked);
    let receipt = runtime.send("Main", "Survivor", "retain until resume", None)[0].clone();
    assert_eq!(receipt.outcome, pi_coding::DeliveryOutcome::Queued);
    let still_parked = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Survivor")
        .expect("survivor listed after parked delivery");
    assert_eq!(still_parked.status, AgentStatus::Parked);
    assert_eq!(still_parked.artifact_ref.as_deref(), Some("agent://Survivor"));
    assert_eq!(still_parked.history_ref.as_deref(), Some(history_ref.as_str()));
    let inbox = runtime.inbox("Survivor", false);
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].body, "retain until resume");
    assert!(
        runtime
            .resolve_read_uri(&results[0].artifact_uri)
            .expect("artifact resolves")
            .is_file()
    );
    registration.unregister();
    runtime.shutdown().await;
}

#[tokio::test]
async fn mailbox_bound_remains_enforced_for_parked_agent() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("cap-model-{suffix}"),
        name: "Cap Model".to_owned(),
        api: format!("cap-api-{suffix}"),
        provider: format!("cap-provider-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });
    registration.set_responses(vec![FauxResponse::text("done")]);
    let mut by_id = HashMap::new();
    by_id.insert("Worker".to_owned(), registration.clone());
    let factory = faux_factory(Arc::new(parking_lot::Mutex::new(by_id)));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    );
    config.mailbox_capacity = 2;
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    run_one_child(&runtime, "Worker").await;
    runtime.park("Worker").expect("park");
    assert_eq!(
        runtime.send("Main", "Worker", "one", None)[0].outcome,
        pi_coding::DeliveryOutcome::Queued,
    );
    assert_eq!(
        runtime.send("Main", "Worker", "two", None)[0].outcome,
        pi_coding::DeliveryOutcome::Queued,
    );
    assert_eq!(
        runtime.send("Main", "Worker", "three", None)[0].outcome,
        pi_coding::DeliveryOutcome::Failed,
    );
    assert_eq!(runtime.inbox("Worker", true).len(), 2);
    runtime.shutdown().await;
    registration.unregister();
}

#[tokio::test]
async fn shutdown_cleans_group_and_no_park_fires_afterwards() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registration) = single_child_runtime(
        artifacts.path(),
        "Worker",
        FauxResponse::text("done"),
        Some(std::time::Duration::from_millis(40)),
    );
    run_one_child(&runtime, "Worker").await;
    runtime.shutdown().await;
    assert!(runtime.list("Main").is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(runtime.list("Main").is_empty());
    assert_eq!(
        runtime.send("Main", "Worker", "post shutdown", None)[0].outcome,
        pi_coding::DeliveryOutcome::Failed,
    );
    assert_eq!(runtime.active_child_count(), 0);
    registration.unregister();
}

#[tokio::test]
async fn group_bound_resolver_keeps_same_agent_artifacts_unique_and_stale_safe() {
    let artifacts = tempfile::tempdir().expect("shared artifacts");
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let response = request.assignment.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let response = response.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text(response));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime_a =
        OrchestrationRuntime::new(config_with_ttl(artifacts.path(), None), factory.clone())
            .expect("runtime_a");
    let runtime_b = OrchestrationRuntime::new(
        config_with_ttl(artifacts.path(), None),
        factory,
    )
    .expect("runtime_b");

    let (_, abort_a) = AbortController::new();
    let result_a = runtime_a
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Shared".to_owned(),
                agent: "task".to_owned(),
                assignment: "runtime a".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort_a,
        )
        .await
        .expect("runtime a task")
        .remove(0);
    let path_a = runtime_a
        .resolve_read_uri(&result_a.artifact_uri)
        .expect("runtime a artifact");
    let stale_resolver = runtime_a.read_uri_resolver();

    let (_, abort_b) = AbortController::new();
    let result_b = runtime_b
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Shared".to_owned(),
                agent: "task".to_owned(),
                assignment: "runtime b".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort_b,
        )
        .await
        .expect("runtime b task")
        .remove(0);
    let path_b = runtime_b
        .resolve_read_uri(&result_b.artifact_uri)
        .expect("runtime b artifact");

    assert_ne!(path_a, path_b, "job-unique physical paths must not collide");
    assert_eq!(
        std::fs::read_to_string(&path_a).expect("runtime a body"),
        format!("runtime a\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    assert_eq!(
        std::fs::read_to_string(&path_b).expect("runtime b body"),
        format!("runtime b\n\n{}", pi_coding::MISSING_YIELD_WARNING)
    );
    assert_eq!(
        stale_resolver("artifact://Shared").expect("group-bound resolver"),
        path_a,
    );
    assert_eq!(
        runtime_b
            .resolve_read_uri("artifact://Shared")
            .expect("runtime b stable alias"),
        path_b,
    );

    runtime_a.shutdown().await;
    assert!(stale_resolver("artifact://Shared").is_err());
    assert_eq!(
        runtime_b
            .resolve_read_uri("artifact://Shared")
            .expect("runtime b survives runtime a cleanup"),
        path_b,
    );
    runtime_b.shutdown().await;
}

#[tokio::test]
async fn main_agent_cannot_be_parked() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (factory, _started, _release) = hanging_factory();
    let runtime = OrchestrationRuntime::new(
        config_with_ttl(artifacts.path(), Some(std::time::Duration::from_millis(40))),
        factory,
    )
    .expect("runtime");
    let err = runtime.park("Main").expect_err("main park rejected");
    assert!(err.to_string().contains("main agent cannot be parked"));
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let main = runtime
        .list("Worker")
        .into_iter()
        .find(|peer| peer.id == "Main")
        .expect("main listed");
    assert_eq!(main.status, AgentStatus::Idle);
    runtime.shutdown().await;
}


#[tokio::test]
async fn disabled_agent_prevents_spawn_with_actionable_error() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    );
    config.agent_settings.insert(
        "task".to_owned(),
        pi_coding::AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
                tools: None,
            },
    );
    let runtime = OrchestrationRuntime::new(
        config,
        Arc::new(|_| Box::pin(async { panic!("disabled agent must not spawn a child session") })),
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let error = runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Child".to_owned(),
                agent: "task".to_owned(),
                assignment: "should fail".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect_err("disabled agent must fail spawn");
    let message = error.to_string();
    assert!(message.contains("disabled"), "{message}");
    assert!(message.contains("task"), "{message}");
    assert!(message.contains("/agents") || message.contains("settings.agents"), "{message}");
    runtime.shutdown().await;
}


#[tokio::test]
async fn agent_model_override_changes_child_session_model() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let parent_model = Model {
        id: "parent-model".to_owned(),
        name: "Parent".to_owned(),
        api: "parent-api".to_owned(),
        provider: "parent-provider".to_owned(),
        ..Model::default()
    };
    let seen = Arc::new(Mutex::new(None::<Model>));
    let seen_factory = seen.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let seen_factory = seen_factory.clone();
        Box::pin(async move {
            *seen_factory.lock() = Some(request.model.clone());
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "test".to_owned(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    });

    let mut definition = definition("task");
    definition.model = Some(vec!["definition-provider/definition-model".to_owned()]);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition]),
        artifacts.path(),
    );
    config.parent_model = parent_model.clone();
    // Settings override names the parent model so empty available catalogs still resolve.
    config.agent_settings.insert(
        "task".to_owned(),
        pi_coding::AgentRuntimeSettings {
            enabled: Some(true),
            model: Some(format!("{}/{}", parent_model.provider, parent_model.id)),
            tools: None,
        },
    );

    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let _ = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Child".to_owned(),
                agent: "task".to_owned(),
                assignment: "use override model".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn");
    for _ in 0..50 {
        if seen.lock().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let captured = seen.lock().clone().expect("factory observed request.model");
    assert_eq!(captured.provider, parent_model.provider);
    assert_eq!(captured.id, parent_model.id);
    runtime.shutdown().await;
}

#[tokio::test]
async fn live_parent_model_provider_changes_fallback_between_spawns() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let first = Model {
        id: "live-parent-first".to_owned(),
        name: "Live Parent First".to_owned(),
        api: "live-parent-api-first".to_owned(),
        provider: "live-parent-provider-first".to_owned(),
        ..Model::default()
    };
    let second = Model {
        id: "live-parent-second".to_owned(),
        name: "Live Parent Second".to_owned(),
        api: "live-parent-api-second".to_owned(),
        provider: "live-parent-provider-second".to_owned(),
        ..Model::default()
    };
    let current = Arc::new(Mutex::new(first.clone()));
    let provider_current = current.clone();
    let seen = Arc::new(Mutex::new(Vec::<Model>::new()));
    let factory_seen = seen.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let seen = factory_seen.clone();
        Box::pin(async move {
            seen.lock().push(request.model.clone());
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "test".to_owned(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    });
    let config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    )
    .with_parent_model(first.clone())
    .with_parent_model_provider(Arc::new(move || provider_current.lock().clone()));
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");

    runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "FirstLiveParent".to_owned(),
                agent: "task".to_owned(),
                assignment: "first parent".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("first spawn");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if seen.lock().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first model captured");
    *current.lock() = second.clone();
    runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "SecondLiveParent".to_owned(),
                agent: "task".to_owned(),
                assignment: "second parent".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("second spawn");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if seen.lock().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second model captured");

    let captured = seen.lock().clone();
    assert_eq!(captured[0].provider, first.provider);
    assert_eq!(captured[0].id, first.id);
    assert_eq!(captured[1].provider, second.provider);
    assert_eq!(captured[1].id, second.id);
    runtime.shutdown().await;
}

#[tokio::test]
async fn public_events_report_queued_running_terminal_and_parked_truthfully() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![("EventChild", FauxResponse::text("event result"))],
        1,
        8,
        2,
    );
    let mut events = runtime.subscribe();
    let assignment_secret = ["s", "k-", "live-super", "-secret"].concat();
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "EventChild".to_owned(),
                agent: "task".to_owned(),
                assignment: format!("summarize\nsecret {assignment_secret}"),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);

    let mut queued = false;
    let mut running = false;
    let mut completed = false;
    while !completed {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        if let pi_coding::OrchestrationEvent::JobUpdated { group_id, job } = event
            && job.id == spawn.job_id
        {
            assert_eq!(group_id, runtime.group_id());
            match job.status {
                pi_coding::JobStatus::Queued => {
                    queued = true;
                    assert_eq!(job.description.as_deref(), Some("summarize secret [REDACTED]"));
                }
                pi_coding::JobStatus::Running => running = true,
                pi_coding::JobStatus::Completed => {
                    completed = true;
                    assert_eq!(
                        job.result
                            .as_ref()
                            .map(|result| result.output.clone()),
                        Some(format!("event result\n\n{}", pi_coding::MISSING_YIELD_WARNING))
                    );
                }
                pi_coding::JobStatus::Failed | pi_coding::JobStatus::Cancelled => {}
            }
        }
    }
    assert!(queued && running && completed);
    runtime.park("EventChild").expect("park agent");
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("park event timeout")
            .expect("event channel");
        if let pi_coding::OrchestrationEvent::AgentUpdated { group_id, agent } = event
            && agent.id == "EventChild"
            && agent.status == AgentStatus::Parked
        {
            assert_eq!(group_id, runtime.group_id());
            break;
        }
    }

    runtime.shutdown().await;
    drop(registrations);
}
