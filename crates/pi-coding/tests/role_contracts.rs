//! Runtime enforcement of role contract fields on `AgentDefinition`
//! (`max_turns`, `max_tool_calls`, `timeout_secs`, `disallowed_tools`,
//! `capability_ceiling`) plus the `/role` preferred-agent selection path.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pi_agent::ToolCapability;
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, CapabilityCeiling,
    ChildSessionFactory, ChildSessionRequest, JobSnapshot, JobStatus, OrchestrationConfig,
    OrchestrationRuntime, Session, SessionOptions, TaskItem, TaskSpawn,
};
use serde_json::{Value, json};

fn contract_definition(
    max_turns: Option<usize>,
    max_tool_calls: Option<usize>,
    timeout_secs: Option<u64>,
) -> AgentDefinition {
    AgentDefinition { name: "task".to_owned(),
    description: "contract test role".to_owned(),
    system_prompt: "complete the assignment".to_owned(),
    tools: None,
    autoload_skills: Vec::new(),
    model: None,
    thinking_level: None,
    max_turns,
    max_tool_calls,
    timeout_secs,
    disallowed_tools: Vec::new(),
    capability_ceiling: None,
    source: AgentDefinitionSource::Bundled,
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
}

struct ScriptedTurn {
    text: String,
    tool: Option<(String, String, Value)>,
}

fn scripted_turn(text: &str, tool: Option<(String, String, Value)>) -> ScriptedTurn {
    ScriptedTurn {
        text: text.to_owned(),
        tool,
    }
}

/// Runtime whose child factory streams a fixed script of assistant turns.
/// Every non-final turn must carry a `hub` tool call so the agent loop runs
/// the next turn; the final turn carries plain text and `Stop`.
fn scripted_runtime(
    artifact_dir: &std::path::Path,
    definition: AgentDefinition,
    turns: Vec<ScriptedTurn>,
) -> OrchestrationRuntime {
    scripted_runtime_with_budget(
        artifact_dir,
        definition,
        turns,
        pi_coding::JobSoftBudget::default(),
    )
}

/// [`scripted_runtime`] with a configured soft budget, so contract-limit and
/// soft-budget composition is observable on the settled job.
fn scripted_runtime_with_budget(
    artifact_dir: &std::path::Path,
    definition: AgentDefinition,
    turns: Vec<ScriptedTurn>,
    soft_budget: pi_coding::JobSoftBudget,
) -> OrchestrationRuntime {
    let turns = Arc::new(Mutex::new(VecDeque::from(turns)));
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let turns = turns.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let turns = turns.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let turn = turns.lock().pop_front().unwrap_or_else(|| ScriptedTurn {
                            text: "script exhausted".to_owned(),
                            tool: None,
                        });
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        if let Some((id, name, arguments)) = &turn.tool {
                            message.content.push(ContentBlock::ToolCall(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                                thought_signature: None,
                            }));
                            message.stop_reason = StopReason::ToolUse;
                        } else {
                            message.stop_reason = StopReason::Stop;
                        }
                        if !turn.text.is_empty() {
                            message.content.push(ContentBlock::text(turn.text.clone()));
                        }
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: pi_agent::ThinkingLevel::Off,
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
        AgentCatalog::from_agents(vec![definition]),
        artifact_dir,
    );
    config.idle_ttl = None;
    config.soft_budget = soft_budget;
    config.parent_model = Model {
        id: "role-contract-test".to_owned(),
        name: "Role Contract Test".to_owned(),
        api: "role-contract-test".to_owned(),
        provider: "role-contract-test".to_owned(),
        ..Model::default()
    };
    OrchestrationRuntime::new(config, factory).expect("runtime")
}

fn hub_list_tool_call(id: &str) -> (String, String, Value) {
    (id.to_owned(), "hub".to_owned(), json!({ "op": "list" }))
}

async fn spawn_and_settle(runtime: &OrchestrationRuntime, name: &str) -> JobSnapshot {
    let spawns = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: name.to_owned(),
                agent: "task".to_owned(),
                assignment: "contract work".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn");
    let jobs = runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(10)),
            None,
        )
        .await
        .expect("child settlement");
    assert_eq!(jobs.len(), 1);
    jobs.into_iter().next().expect("settled job")
}

#[tokio::test]
async fn max_turns_stops_child_with_clear_reason() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = scripted_runtime(
        artifacts.path(),
        contract_definition(Some(2), None, None),
        vec![
            scripted_turn("first", Some(hub_list_tool_call("hub-1"))),
            scripted_turn("second", Some(hub_list_tool_call("hub-2"))),
            scripted_turn("third", None),
        ],
    );
    let job = spawn_and_settle(&runtime, "MaxTurnsChild").await;

    assert_eq!(
        job.status,
        JobStatus::Failed,
        "a role contract stop must surface as a failed job with its reason"
    );
    assert!(
        !job.soft_budget_exhausted,
        "the soft-limit marker is reserved for the soft budget, never a contract stop"
    );
    let result = job.result.expect("settled result");
    assert_eq!(result.output, "second", "child stopped after its second turn");
    let error = result.error.expect("contract must surface a clear reason");
    assert!(error.contains("maxTurns"), "{error}");
    assert!(error.contains("2"), "{error}");
    assert!(error.contains("task"), "{error}");

    runtime.shutdown().await;
}

#[tokio::test]
async fn max_tool_calls_stops_child_with_clear_reason() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = scripted_runtime(
        artifacts.path(),
        contract_definition(None, Some(1), None),
        vec![
            scripted_turn("first", Some(hub_list_tool_call("hub-1"))),
            scripted_turn("second", Some(hub_list_tool_call("hub-2"))),
            scripted_turn("third", None),
        ],
    );
    let job = spawn_and_settle(&runtime, "MaxCallsChild").await;

    assert_eq!(
        job.status,
        JobStatus::Failed,
        "a role contract stop must surface as a failed job with its reason"
    );
    assert!(
        !job.soft_budget_exhausted,
        "the soft-limit marker is reserved for the soft budget, never a contract stop"
    );
    let result = job.result.expect("settled result");
    assert_eq!(result.output, "first", "child stopped after its single allowed tool call");
    let error = result.error.expect("contract must surface a clear reason");
    assert!(error.contains("maxToolCalls"), "{error}");
    assert!(error.contains("1"), "{error}");

    runtime.shutdown().await;
}

#[tokio::test]
async fn contract_limit_stays_failed_and_unmarked_even_with_soft_budget() {
    // A role contract limit and a soft budget compose; whichever trips first
    // decides the job's fate. maxTurns=1 trips before maxRequests=5, so the
    // job FAILS with the contract reason and must NOT carry the soft-limit
    // marker — the marker is reserved for soft-budget yields.
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = scripted_runtime_with_budget(
        artifacts.path(),
        contract_definition(Some(1), None, None),
        vec![
            scripted_turn("first", Some(hub_list_tool_call("hub-1"))),
            scripted_turn("second", Some(hub_list_tool_call("hub-2"))),
            scripted_turn("third", None),
        ],
        pi_coding::JobSoftBudget {
            max_requests: Some(5),
            ..Default::default()
        },
    );
    let job = spawn_and_settle(&runtime, "ContractVsBudget").await;
    assert_eq!(
        job.status,
        JobStatus::Failed,
        "the contract limit wins and fails the job"
    );
    assert!(
        !job.soft_budget_exhausted,
        "contract stops never set the soft-limit marker"
    );
    let result = job.result.expect("settled result");
    assert!(
        !result.soft_budget_exhausted,
        "task result must match the snapshot marker"
    );
    let error = result.error.expect("contract must surface its own reason");
    assert!(error.contains("maxTurns"), "{error}");
    assert!(
        !result.output.contains("MISSING_YIELD"),
        "a contract stop is a host cut, not a missing-yield case: {:?}",
        result.output
    );

    runtime.shutdown().await;
}

#[tokio::test]
async fn soft_budget_stays_completed_and_marked_even_with_stricter_contract() {
    // The reverse composition: maxRequests=1 (soft) trips before maxTurns=5
    // (contract). The job completes with the soft-limit marker and NO error.
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = scripted_runtime_with_budget(
        artifacts.path(),
        contract_definition(Some(5), None, None),
        vec![
            scripted_turn("first", Some(hub_list_tool_call("hub-1"))),
            scripted_turn("second", Some(hub_list_tool_call("hub-2"))),
            scripted_turn("third", None),
        ],
        pi_coding::JobSoftBudget {
            max_requests: Some(1),
            ..Default::default()
        },
    );
    let job = spawn_and_settle(&runtime, "BudgetVsContract").await;
    assert_eq!(
        job.status,
        JobStatus::Completed,
        "a soft budget must never fail the job"
    );
    assert!(
        job.soft_budget_exhausted,
        "a soft-budget yield carries the soft-limit marker"
    );
    let result = job.result.expect("settled result");
    assert!(
        result.soft_budget_exhausted,
        "task result must match the snapshot marker"
    );
    assert!(result.error.is_none(), "{:?}", result.error);
    assert!(
        !result.output.contains("MISSING_YIELD"),
        "a soft-budget stop is a host cut, not a missing-yield case: {:?}",
        result.output
    );

    runtime.shutdown().await;
}

#[tokio::test]
async fn timeout_aborts_hanging_child_with_clear_reason() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let release = tokio_util::sync::CancellationToken::new();
    let stream_release = release.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let release = stream_release.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let abort = options.stream.abort_signal;
                tokio::select! {
                    () = release.cancelled() => {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(ContentBlock::text("done"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    }
                    () = async {
                        match abort {
                            Some(signal) => signal.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Aborted;
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
                thinking_level: pi_agent::ThinkingLevel::Off,
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
        AgentCatalog::from_agents(vec![contract_definition(None, None, Some(1))]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");

    let job = spawn_and_settle(&runtime, "TimeoutChild").await;
    assert_eq!(
        job.status,
        JobStatus::Failed,
        "a timeout contract must surface as a failed job with its reason"
    );
    let result = job.result.expect("settled result");
    let error = result.error.expect("timeout must surface a clear reason");
    assert!(error.contains("timeout contract"), "{error}");
    assert!(error.contains("1s"), "{error}");
    assert!(error.contains("task"), "{error}");

    release.cancel();
    runtime.shutdown().await;
}

/// Build the production child factory (the one real sessions use) and drive it
/// with a crafted request so the actual child tool set is observable.
///
/// The cwd `TempDir` is returned so the factory's snapshot cwd stays alive for
/// the duration of the test (child `Session::new` canonicalizes the cwd).
fn production_child_factory(
) -> (OrchestrationRuntime, ChildSessionFactory, Model, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("cwd");
    let model = Model::default();
    let parent = Session::new(SessionOptions {
        model: model.clone(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: "test-key".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("parent session");
    let factory = OrchestrationRuntime::child_factory_from_session(&parent);
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![contract_definition(None, None, None)]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, Arc::new(|_| {
        Box::pin(async { Err(anyhow::anyhow!("unused factory")) })
    }))
    .expect("runtime");
    (runtime, factory, model, cwd)
}

fn child_request(model: Model, definition: AgentDefinition, names: Option<Vec<String>>) -> ChildSessionRequest {
    ChildSessionRequest {
        child_id: "Child".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 64,
        depth: 1,
        definition,
        assignment: "filter work".to_owned(),
        system_prompt: "p".to_owned(),
        requested_tool_names: names,
        orchestration_tools: Vec::new(),
        thinking_level: None,
        model,
        output_schema: None,
        schema_mode: None,
        yield_state: std::sync::Arc::new(pi_coding::YieldState::default()),
    }
}

#[tokio::test]
async fn disallowed_tools_remove_tools_by_name_at_spawn() {
    let (_runtime, factory, model, _cwd) = production_child_factory();
    let mut definition = contract_definition(None, None, None);
    definition.disallowed_tools = vec!["bash".to_owned()];
    let request = child_request(
        model,
        definition,
        Some(vec![
            "read".to_owned(),
            "bash".to_owned(),
            "edit".to_owned(),
        ]),
    );
    let session = factory(request).await.expect("child session");
    let tools = session.get_all_tools();
    let names = tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>();
    assert!(names.contains(&"read"), "{names:?}");
    assert!(names.contains(&"edit"), "{names:?}");
    assert!(!names.contains(&"bash"), "disallowed tool must be filtered: {names:?}");
}

#[tokio::test]
async fn capability_ceiling_filters_child_tools_by_capability() {
    let (_runtime, factory, model, _cwd) = production_child_factory();
    let mut definition = contract_definition(None, None, None);
    definition.capability_ceiling = Some(CapabilityCeiling {
        read: true,
        write: false,
        exec: true,
    });
    let request = child_request(model, definition, None);
    let session = factory(request).await.expect("child session");
    let tools = session.get_all_tools();
    assert!(
        tools.iter().all(|tool| tool.capability != ToolCapability::Write),
        "ceiling must drop Write tools: {:?}",
        tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.capability))
            .collect::<Vec<_>>()
    );
    assert!(tools.iter().any(|tool| tool.name == "read"));
    assert!(tools.iter().any(|tool| tool.name == "bash"));
}

#[tokio::test]
async fn disallowed_and_ceiling_combine_at_spawn() {
    let (_runtime, factory, model, _cwd) = production_child_factory();
    let mut definition = contract_definition(None, None, None);
    definition.disallowed_tools = vec!["bash".to_owned()];
    definition.capability_ceiling = Some(CapabilityCeiling {
        read: true,
        write: true,
        exec: false,
    });
    let request = child_request(model, definition, None);
    let session = factory(request).await.expect("child session");
    let tools = session.get_all_tools();
    assert!(!tools.is_empty(), "read/write tools must remain");
    let yield_tool = tool(&tools, "yield");
    assert_eq!(
        yield_tool.capability,
        ToolCapability::Exec,
        "yield remains execution plumbing even above the coding-tool ceiling"
    );
    for tool in tools.iter().filter(|tool| tool.name != "yield") {
        assert_ne!(tool.name, "bash", "disallowed tool must never appear");
        assert!(
            matches!(tool.capability, ToolCapability::Read | ToolCapability::Write),
            "exec coding tools must be dropped by the ceiling: {} {:?}",
            tool.name,
            tool.capability,
        );
    }
}

fn tool<'a>(tools: &'a [pi_agent::AgentTool], name: &str) -> &'a pi_agent::AgentTool {
    tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("missing {name} tool"))
}

fn context(id: &str, arguments: Value) -> pi_agent::ToolCallContext {
    let (_, abort) = pi_agent::AbortController::new();
    pi_agent::ToolCallContext {
        tool_call_id: id.to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    }
}

/// Spawn a child through the real `task` tool — the only path where the
/// preferred-role selection is consulted.
async fn spawn_via_task_tool(runtime: &OrchestrationRuntime, name: &str, task: &str) -> TaskSpawn {
    let tools = runtime.agent_tools("Main", 0);
    let task_tool = tool(&tools, "task");
    let spawn_result = (task_tool.execute)(context(
        "spawn-role",
        json!({ "name": name, "task": task }),
    ))
    .await
    .expect("task spawn result");
    let mut spawns: Vec<TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawn details");
    spawns.remove(0)
}

#[tokio::test]
async fn selected_role_becomes_default_for_unnamed_task_spawns() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let task = contract_definition(None, None, None);
    let reviewer = AgentDefinition {
        name: "reviewer".to_owned(),
        description: "code reviewer".to_owned(),
        system_prompt: "review carefully".to_owned(),
        ..contract_definition(None, None, None)
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![task, reviewer]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(
        config,
        Arc::new(|_| Box::pin(async { Err(anyhow::anyhow!("unused factory")) })),
    )
    .expect("runtime");

    // No preference: unnamed task spawns use the configured default agent.
    let spawn = spawn_via_task_tool(&runtime, "DefaultChild", "go over the changes").await;
    assert_eq!(spawn.agent, "task");

    // Explicit /role selection redirects the next unnamed spawn.
    runtime.set_preferred_agent(Some("reviewer"));
    assert_eq!(runtime.preferred_agent().as_deref(), Some("reviewer"));
    let spawn = spawn_via_task_tool(&runtime, "RoleChild", "go over the changes again").await;
    assert_eq!(spawn.agent, "reviewer");

    // An explicit task.agent override still beats the preference.
    let tools = runtime.agent_tools("Main", 0);
    let task_tool = tool(&tools, "task");
    let spawn_result = (task_tool.execute)(context(
        "spawn-explicit",
        json!({ "name": "ExplicitChild", "task": "go over it", "agent": "task" }),
    ))
    .await
    .expect("explicit agent spawn");
    let mut spawns: Vec<TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawn details");
    assert_eq!(spawns.remove(0).agent, "task");

    // Clearing the preference restores default selection.
    runtime.set_preferred_agent(None);
    assert!(runtime.preferred_agent().is_none());
    let spawn = spawn_via_task_tool(&runtime, "ClearedChild", "go over it one last time").await;
    assert_eq!(spawn.agent, "task");

    runtime.shutdown().await;
}

#[tokio::test]
async fn selected_role_ignored_when_disabled_by_settings() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let task = contract_definition(None, None, None);
    let reviewer = AgentDefinition {
        name: "reviewer".to_owned(),
        description: "code reviewer".to_owned(),
        system_prompt: "review carefully".to_owned(),
        ..contract_definition(None, None, None)
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![task, reviewer]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    config.agent_settings.insert(
        "reviewer".to_owned(),
        pi_coding::AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );
    let runtime = OrchestrationRuntime::new(
        config,
        Arc::new(|_| Box::pin(async { Err(anyhow::anyhow!("unused factory")) })),
    )
    .expect("runtime");

    runtime.set_preferred_agent(Some("reviewer"));
    let spawn = spawn_via_task_tool(&runtime, "DisabledChild", "go over the changes").await;
    assert_eq!(
        spawn.agent, "task",
        "a disabled preferred role must fall back to the default"
    );

    runtime.shutdown().await;
}
