//! End-to-end coverage for the hub `read_history` op (U13): one orchestration
//! agent reads another agent's settled session transcript through the owned
//! `hub` tool, and Main reads a child's history directly.
//!
//! The in-module unit tests in `orchestration/tools.rs` prove the op's
//! parameter validation, bounded rendering, redaction, and traversal guards;
//! these tests prove the two-agent flow through the REAL runtime: Alpha runs a
//! scripted turn (hub send + assistant reply carrying a credential-shaped
//! secret), settles, and Beta's scripted hub `read_history` call returns the
//! rendered transcript — `user:` / `assistant:` / `[tool: ...]` single-line
//! labels with the secret redacted — plus the error surfaces (unknown agent,
//! out-of-range `lines`).
//!
//! Deterministic by construction: faux providers are scripted per child id,
//! Alpha is spawned and settled BEFORE Beta runs so the settle-time
//! `.history.json` snapshot always exists when the read lands, and every wait
//! is bounded. No network, no credentials.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pi_agent::{AbortController, ThinkingLevel, ToolCallContext};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{ContentBlock, Message, Model, SimpleStreamOptions, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, ChildSessionFactory,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions,
};

/// Deterministic secret-shaped text seeded into Alpha's reply; the rendered
/// history must redact it and never surface the raw bytes.
fn alpha_secret() -> String {
    ["s", "k-", "abcdefghijklmnop", "qrstuvwxyz0123456789"].concat()
}

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

/// A scripted child model response that emits one `hub` tool call.
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

/// Child factory keyed by stable child id: each scripted child resolves its
/// own faux provider registration (same shape as `orchestration.rs`'s
/// `faux_factory`, but kept local so this file is self-contained).
fn scripted_factory(
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

/// Register one scripted child provider; returns the registration (kept alive
/// until the test drops it) and the child model id used by the definition.
fn register_child(
    registrations: &mut HashMap<String, pi_ai::providers::FauxProviderRegistration>,
    base: &Model,
    child: &str,
    responses: Vec<FauxResponse>,
) -> pi_ai::providers::FauxProviderRegistration {
    let api = format!("{}-{child}", base.api);
    let provider = format!("{}-{child}", base.provider);
    let id = format!("{}-{child}", base.id);
    let registration = register_faux_provider(FauxProviderOptions {
        api: api.clone(),
        provider: provider.clone(),
        models: vec![Model {
            id: id.clone(),
            api,
            provider,
            ..base.clone()
        }],
        chunk_size: 1,
    });
    registration.set_responses(responses);
    registrations.insert(child.to_owned(), registration.clone());
    registration
}

fn tool_context(id: &str, arguments: serde_json::Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext {
        tool_call_id: id.to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    }
}

/// Spawn one child through Main's owned `task` tool and wait for its job to
/// settle; returns the spawn details (job id + agent id).
async fn spawn_and_settle(
    runtime: &OrchestrationRuntime,
    task: &pi_agent::AgentTool,
    name: &str,
    agent: &str,
    assignment: &str,
) -> pi_coding::TaskSpawn {
    let spawned = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        (task.execute)(tool_context(
            format!("spawn-{name}").as_str(),
            serde_json::json!({
                "name": name,
                "agent": agent,
                "task": assignment,
            }),
        )),
    )
    .await
    .expect("task returns before children settle")
    .expect("spawn child");
    let spawns: Vec<pi_coding::TaskSpawn> =
        serde_json::from_value(spawned.details).expect("spawn details");
    assert_eq!(spawns.len(), 1);
    let spawn = spawns.into_iter().next().expect("one spawn");
    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        runtime.wait_jobs(
            &[spawn.job_id.clone()],
            Some(std::time::Duration::from_secs(8)),
            None,
        ),
    )
    .await
    .expect("job settle bounded")
    .expect("job settles");
    assert!(
        settled[0].status.is_settled(),
        "child {name} must settle: {:?}",
        settled[0].status
    );
    spawn
}

/// Contract: Beta reads Alpha's settled transcript through its OWNED hub tool
/// (`op: "read_history"`), and the rendered output carries the `user:` /
/// `assistant:` / `[tool: ...]` label format with the credential-shaped
/// secret redacted. The same op through Main's hub tool renders the identical
/// transcript, and an unknown agent / out-of-range `lines` fail actionably.
#[tokio::test]
async fn beta_reads_alpha_history_through_owned_hub_tool_and_redacts() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let alpha_secret = alpha_secret();
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("read-history-model-{suffix}"),
        name: "Read History E2E".to_owned(),
        api: format!("read-history-api-{suffix}"),
        provider: format!("read-history-provider-{suffix}"),
        ..Model::default()
    };
    let mut registrations = HashMap::new();
    let alpha_registration = register_child(
        &mut registrations,
        &model,
        "Alpha",
        vec![
            // Alpha's turn: one owned hub send (produces a [tool: hub] result
            // in its history), then an assistant reply carrying the secret.
            hub_tool_call(
                "alpha-send-main",
                serde_json::json!({"op": "send", "to": "Main", "message": "alpha says hi"}),
            ),
            FauxResponse::text(format!("alpha-reply-marker {alpha_secret}")),
        ],
    );
    let beta_registration = register_child(
        &mut registrations,
        &model,
        "Beta",
        vec![
            // Beta's first turn action: read Alpha's history through its own
            // owned hub tool.
            hub_tool_call(
                "beta-read-alpha",
                serde_json::json!({"op": "read_history", "agentId": "Alpha", "lines": 50}),
            ),
            FauxResponse::text("beta-done"),
        ],
    );
    let mut alpha_definition = definition("alpha");
    alpha_definition.model = Some(vec![format!("{}-Alpha", model.id)]);
    let mut beta_definition = definition("beta");
    beta_definition.model = Some(vec![format!("{}-Beta", model.id)]);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![alpha_definition, beta_definition]),
        artifacts.path(),
    );
    config.default_agent = "alpha".to_owned();
    config.max_concurrency = 2;
    config.mailbox_capacity = 8;
    config.max_recursion_depth = 2;
    config.parent_model = model;
    let runtime = OrchestrationRuntime::new(
        config,
        scripted_factory(Arc::new(Mutex::new(registrations))),
    )
    .expect("runtime");
    let tools = runtime.agent_tools("Main", 0);
    let task = tools.iter().find(|tool| tool.name == "task").expect("task tool");
    let hub = tools.iter().find(|tool| tool.name == "hub").expect("hub tool");

    // Alpha settles FIRST so its settle-time history snapshot exists before
    // Beta's read_history runs (deterministic ordering, no race).
    let alpha_spawn =
        spawn_and_settle(&runtime, task, "Alpha", "alpha", "Do alpha work, then report back.")
            .await;
    assert_eq!(alpha_spawn.agent_id, "Alpha");
    let beta_spawn = spawn_and_settle(
        &runtime,
        task,
        "Beta",
        "beta",
        "Read Alpha's transcript and summarize it.",
    )
    .await;
    assert_eq!(beta_spawn.agent_id, "Beta");

    // --- Beta's owned hub read_history result ---------------------------------
    let beta_history = runtime
        .resolve_history_reference("Beta")
        .expect("beta history path");
    let messages: Vec<Message> =
        serde_json::from_slice(&std::fs::read(&beta_history).expect("read beta history"))
            .expect("parse beta history");
    let read_result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == "hub" => Some(result),
            _ => None,
        })
        .expect("beta must hold a hub tool result");
    let rendered = read_result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        rendered.contains("alpha-reply-marker"),
        "rendered history must carry Alpha's assistant reply: {rendered}"
    );
    assert!(
        rendered.contains("[tool: hub]"),
        "rendered history must label Alpha's hub tool result: {rendered}"
    );
    assert!(
        rendered.contains("user:"),
        "rendered history must label Alpha's assignment user turn: {rendered}"
    );
    // The seeded secret is redacted in the rendered output; the raw bytes
    // never leak into Beta's tool result.
    assert!(
        rendered.contains("[REDACTED]"),
        "rendered history must redact the credential: {rendered}"
    );
    assert!(
        !rendered.contains(alpha_secret.as_str()),
        "raw secret must never reach the reader: {rendered}"
    );
    assert_eq!(
        read_result.details.as_ref().and_then(|v| v.get("op")).and_then(serde_json::Value::as_str),
        Some("read_history"),
        "hub result details must identify the op"
    );

    // --- Main reads the same child history through its own hub tool ----------
    let main_read = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        (hub.execute)(tool_context(
            "main-read-alpha",
            serde_json::json!({"op": "read_history", "agentId": "Alpha", "lines": 50}),
        )),
    )
    .await
    .expect("main read bounded")
    .expect("main read_history succeeds");
    let main_text = main_read
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        main_text.contains("alpha-reply-marker"),
        "Main's read must render the same transcript: {main_text}"
    );
    assert!(
        !main_text.contains(alpha_secret.as_str()),
        "Main's read must redact the secret: {main_text}"
    );

    // --- Error surfaces ------------------------------------------------------
    let unknown = (hub.execute)(tool_context(
        "main-read-ghost",
        serde_json::json!({"op": "read_history", "agentId": "Ghost", "lines": 50}),
    ))
    .await
    .expect_err("unknown agent must fail read_history");
    assert!(
        unknown.to_string().contains("unknown orchestration agent"),
        "unknown agent error: {unknown:#}"
    );

    for bad_lines in [0, pi_coding::MAX_HISTORY_LINES + 1] {
        let error = (hub.execute)(tool_context(
            "main-read-bad-lines",
            serde_json::json!({"op": "read_history", "agentId": "Alpha", "lines": bad_lines}),
        ))
        .await
        .expect_err("out-of-range lines must fail");
        assert!(
            error.to_string().contains("lines must be between 1 and"),
            "lines={bad_lines} error: {error:#}"
        );
    }

    runtime.shutdown().await;
    alpha_registration.unregister();
    beta_registration.unregister();
}

/// Contract: `lines` bounds the rendered window — a small `lines` value keeps
/// only the most recent entries, so Beta's own settled history renders its
/// final assistant reply but drops the earlier hub tool call.
#[tokio::test]
async fn read_history_lines_bounds_render_window() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("read-history-lines-model-{suffix}"),
        name: "Read History Lines".to_owned(),
        api: format!("read-history-lines-api-{suffix}"),
        provider: format!("read-history-lines-provider-{suffix}"),
        ..Model::default()
    };
    let mut registrations = HashMap::new();
    let alpha_registration = register_child(
        &mut registrations,
        &model,
        "Alpha",
        vec![
            hub_tool_call(
                "alpha-send-main",
                serde_json::json!({"op": "send", "to": "Main", "message": "alpha one"}),
            ),
            FauxResponse::text("alpha-final-marker"),
        ],
    );
    let mut alpha_definition = definition("alpha");
    alpha_definition.model = Some(vec![format!("{}-Alpha", model.id)]);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![alpha_definition]),
        artifacts.path(),
    );
    config.default_agent = "alpha".to_owned();
    config.max_concurrency = 2;
    config.mailbox_capacity = 8;
    config.max_recursion_depth = 2;
    config.parent_model = model;
    let runtime = OrchestrationRuntime::new(
        config,
        scripted_factory(Arc::new(Mutex::new(registrations))),
    )
    .expect("runtime");
    let tools = runtime.agent_tools("Main", 0);
    let task = tools.iter().find(|tool| tool.name == "task").expect("task tool");
    let hub = tools.iter().find(|tool| tool.name == "hub").expect("hub tool");
    spawn_and_settle(&runtime, task, "Alpha", "alpha", "Bound the render window.").await;

    // lines=1 keeps only the most recent entry: the assistant final reply.
    let last = (hub.execute)(tool_context(
        "read-lines-1",
        serde_json::json!({"op": "read_history", "agentId": "Alpha", "lines": 1}),
    ))
    .await
    .expect("read_history lines=1");
    let last_text = last
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        last_text.contains("alpha-final-marker"),
        "lines=1 must keep the final reply: {last_text}"
    );
    assert!(
        !last_text.contains("[tool: hub]"),
        "lines=1 must drop the earlier tool result: {last_text}"
    );

    // lines=100 keeps everything (only two entries exist).
    let all = (hub.execute)(tool_context(
        "read-lines-100",
        serde_json::json!({"op": "read_history", "agentId": "Alpha", "lines": 100}),
    ))
    .await
    .expect("read_history lines=100");
    let all_text = all
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        all_text.contains("alpha-final-marker") && all_text.contains("[tool: hub]"),
        "lines=100 must keep the full window: {all_text}"
    );

    runtime.shutdown().await;
    alpha_registration.unregister();
}
