//! Cross-module subagent tool reliability regression (hard_task-style child sessions).
//!
//! Every scenario drives the REAL parent→child orchestration path end to end:
//! `OrchestrationRuntime::child_factory_from_session` builds each child from a
//! hermetic parent session whose ambient tool set is the governed
//! generation-bound coding tool set (`Session` →
//! `tools::create_coding_tools_for_workspace_with_generation`, which includes
//! the generation-bound `grep`/`bash` tools), with the parent's published
//! `Application` generation as the sandbox authority. Children are spawned as
//! hard tasks through `OrchestrationRuntime::run_tasks` — the same
//! spawn+settle path the orchestration `task` tool executes — with per-child
//! faux providers scripting the real agent loop.
//!
//! Contracts pinned here (all deterministic, no network, no real provider):
//!
//! 1. **Admitted additional workspace root.** A child running the real
//!    generation-bound Grep tool against an absolute `<extra>/src` directory
//!    with `glob: Bridge.sol`, `context`, and `limit` succeeds when the
//!    parent's workspace explicitly admits the additional root, and the rows
//!    carry line numbers plus context lines without leaking the basename-
//!    filtered sibling.
//! 2. **Stable scope rejection without admission.** A second child whose
//!    parent workspace does not admit the additional root receives the stable
//!    `path_outside_workspace: <absolute path> ...` error — never a generic
//!    "path resolved to no target" — and never discloses the workspace
//!    configuration (neither the extra root nor the parent cwds).
//! 3. **Doom-loop termination on schema-invalid retries.** One schema-invalid
//!    extra-field Grep call scripted twice through the real agent loop trips
//!    the deterministic after-hook/doom path on the SECOND identical failure:
//!    the turn terminates before a third scripted model response and the Grep
//!    executor count stays zero (every recorded result is the loop's schema
//!    validation rejection, never an executor output).
//! 4. **Shell search gating.** Inside a child that owns both `bash` and
//!    `grep`, `grep -R … | head` and `grep -n -A/-B` bash calls are blocked
//!    before the executor while a benign `echo` in the same child executes —
//!    the executor is reachable, the search commands never reach it.
//! 5. **Delivered-yield outputSchema validation** is asserted separately: the
//!    child's delivered `yield` payload is validated against the invocation's
//!    `outputSchema` and reported via `TaskResult::structured_output`
//!    (`schema_source: "task"`), with strict mode surfacing a rejected
//!    payload as the job error while the child still settles.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_agent::{AbortController, ThinkingLevel};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Message, Model, SimpleStreamOptions, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, Application,
    ApplicationRuntimeCandidate, OrchestrationConfig, OrchestrationRuntime, ResourceDiscovery,
    ResourceManager, ResourceManagerOptions, Session, SessionOptions, TaskItem, TaskResult,
    ToolSelection, WorkspaceRoots,
};
use serde_json::{Value, json};

/// Danger-full-access sandbox skips the launcher probe (no bubblewrap
/// dependency), so a hermetic Application bootstrap succeeds on every host.
const HERMETIC_DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

/// Deterministic Bridge.sol fixture: the `_materializeBucket` needle lives on
/// line 3 of `src/Bridge.sol`; `src/Other.sol` carries the same needle so a
/// broken basename glob would leak it.
fn write_bridge_src(root: &Path) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("src dir");
    std::fs::write(
        src.join("Bridge.sol"),
        "contract Bridge {\n    // setup\n    function _materializeBucket() internal {\n        uint256 x = 1;\n    }\n}\n",
    )
    .expect("Bridge.sol");
    std::fs::write(src.join("Other.sol"), "function _materializeBucket() {}\n")
        .expect("Other.sol");
}

/// Hermetic resource manager for `cwd` with the agent configuration home as
/// a SIBLING of the writable workspace (`agent resource root resolves inside
/// the project boundary` — the agent dir must never sit inside a workspace).
fn hermetic_resources(cwd: &Path, agent_dir: &Path) -> ResourceManager {
    std::fs::create_dir_all(agent_dir).expect("hermetic agent dir");
    std::fs::write(agent_dir.join("settings.json"), HERMETIC_DANGER_SETTINGS)
        .expect("hermetic settings");
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir.to_path_buf();
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    ResourceManager::new(options).expect("hermetic resources")
}

/// Hermetic parent session owning the generation-bound tool factory: the
/// ambient tool set (generation-bound `grep`/`bash` included) is built over
/// the parent's `WorkspaceRoots` (cwd plus explicitly admitted additional
/// roots), and a danger-full-access Application generation is published so
/// children inherit a live generation capture through
/// `child_factory_from_session`. The parent model's provider anchors child
/// credential inheritance.
async fn hermetic_parent(
    root: &Path,
    parent_name: &str,
    parent_model: &Model,
    additional_roots: Vec<PathBuf>,
) -> (Session, Application, FauxProviderRegistration) {
    let cwd = root.join(parent_name);
    std::fs::create_dir_all(&cwd).expect("hermetic parent cwd");
    let session_dir = root.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("hermetic session dir");
    // The parent model's api must resolve HTTP authorities for the sandbox
    // staging inside the Application bootstrap, so register a dedicated faux
    // provider for it (the parent itself never streams, so its response
    // queue stays untouched).
    let parent_registration = register_faux_provider(FauxProviderOptions {
        api: parent_model.api.clone(),
        provider: parent_model.provider.clone(),
        models: vec![parent_model.clone()],
        chunk_size: 16,
    });
    let workspace = WorkspaceRoots::new(&cwd, additional_roots).expect("parent workspace");
    let parent = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
        SessionOptions {
            model: parent_model.clone(),
            cwd,
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".to_owned(),
            compaction: None,
            stream_options: SimpleStreamOptions::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
        workspace,
        None,
        true,
    )
    .expect("hermetic parent session");
    parent.set_session_dir(session_dir);
    // Agent home lives as a sibling of the writable workspace, never inside
    // it (the resource manager rejects an agent root inside the project
    // boundary).
    let agent_dir = root.join(format!("agent-home-{parent_name}"));
    parent
        .attach_resources(hermetic_resources(parent.cwd(), &agent_dir))
        .await
        .expect("attach parent resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(parent.clone()))
            .await
            .expect("hermetic application bootstrap");
    (parent, application, parent_registration)
}

fn scenario_definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: format!("{name} description"),
        system_prompt:
            "Complete the assigned hard task with the authorized tools and deliver the result."
                .to_owned(),
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
        path: None,
        trusted: true,
        kind: AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

/// One scripted child role: its own faux provider registration (response
/// queue) and an agent definition selecting that exact model, so concurrent
/// children never interleave scripted responses. The model's provider matches
/// the parent's so the child inherits the parent's session credentials.
fn scenario(
    provider: &str,
    agent: &str,
    tools: &[&str],
    responses: Vec<FauxResponse>,
) -> (AgentDefinition, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("subagent-reliability-{agent}-{suffix}"),
        name: format!("Subagent Reliability {agent}"),
        api: format!("subagent-reliability-api-{agent}-{suffix}"),
        provider: provider.to_owned(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: provider.to_owned(),
        models: vec![model],
        chunk_size: 16,
    });
    registration.set_responses(responses);
    let mut definition = scenario_definition(agent);
    definition.tools = Some(tools.iter().map(|name| name.to_string()).collect());
    definition.model = Some(vec![format!("subagent-reliability-{agent}-{suffix}")]);
    (definition, registration)
}

struct ReliabilityFixture {
    runtime: OrchestrationRuntime,
    registrations: Vec<FauxProviderRegistration>,
    _parent: Session,
    _application: Application,
}

async fn reliability_fixture(
    root: &Path,
    parent_name: &str,
    parent_model: &Model,
    additional_roots: Vec<PathBuf>,
    scenarios: Vec<(AgentDefinition, FauxProviderRegistration)>,
) -> ReliabilityFixture {
    let (parent, application, parent_registration) =
        hermetic_parent(root, parent_name, parent_model, additional_roots).await;
    let mut agents = vec![scenario_definition("task")];
    agents.extend(scenarios.iter().map(|(definition, _)| definition.clone()));
    let mut config =
        OrchestrationConfig::new(AgentCatalog::from_agents(agents), root.join("artifacts"));
    config.parent_model = parent_model.clone();
    let runtime = OrchestrationRuntime::new(
        config,
        OrchestrationRuntime::child_factory_from_session(&parent),
    )
    .expect("orchestration runtime");
    let mut registrations = vec![parent_registration];
    registrations.extend(
        scenarios
            .into_iter()
            .map(|(_, registration)| registration),
    );
    ReliabilityFixture {
        runtime,
        registrations,
        _parent: parent,
        _application: application,
    }
}

async fn run_hard_tasks(
    fixture: &ReliabilityFixture,
    items: Vec<TaskItem>,
) -> Vec<TaskResult> {
    let (_, abort) = AbortController::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        fixture.runtime.run_tasks("Main", 0, items, abort),
    )
    .await
    .expect("hard-task batch deadline")
    .expect("hard-task batch run")
}

async fn shutdown(mut fixture: ReliabilityFixture) {
    for registration in std::mem::take(&mut fixture.registrations) {
        registration.unregister();
    }
    fixture.runtime.shutdown().await;
}

fn tool_response(id: &str, name: &str, arguments: Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn child_messages(fixture: &ReliabilityFixture, child_id: &str) -> Vec<Message> {
    let path = fixture
        .runtime
        .resolve_history_reference(child_id)
        .expect("child history path");
    serde_json::from_slice(&std::fs::read(&path).expect("read child history"))
        .expect("parse child history")
}

fn result_text(result: &pi_ai::ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_results<'a>(messages: &'a [Message], name: &str) -> Vec<&'a pi_ai::ToolResultMessage> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == name => Some(result),
            _ => None,
        })
        .collect()
}

fn tool_call_count(messages: &[Message], name: &str) -> usize {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| assistant.content.iter())
        .filter(|block| matches!(block, ContentBlock::ToolCall(call) if call.name == name))
        .count()
}

fn result_by_id<'a>(results: &'a [TaskResult], id: &str) -> &'a TaskResult {
    results
        .iter()
        .find(|result| result.id == id)
        .unwrap_or_else(|| panic!("missing hard-task result {id}: {:?}", results))
}

fn hard_task(id: &str, agent: &str, assignment: &str) -> TaskItem {
    TaskItem {
        index: 0,
        id: id.to_owned(),
        agent: agent.to_owned(),
        assignment: assignment.to_owned(),
        ..TaskItem::default()
    }
}

// ---------------------------------------------------------------------------
// 1 + 2: admitted additional-root Grep success; unadmitted stable scope error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hard_task_child_grep_on_admitted_additional_root_succeeds_and_unadmitted_child_gets_stable_scope_error()
{
    let root = tempfile::tempdir().expect("root");
    let extra = root.path().join("extra");
    write_bridge_src(&extra);
    let extra_src = extra.join("src");
    let provider = format!("subagent-tool-reliability-{}", uuid::Uuid::now_v7());
    let parent_model = Model {
        id: format!("subagent-reliability-parent-{}", uuid::Uuid::now_v7()),
        name: "Subagent Reliability Parent".to_owned(),
        api: format!("{provider}-parent-api"),
        provider: provider.clone(),
        ..Model::default()
    };
    let grep_arguments = json!({
        "pattern": "function _materializeBucket",
        "path": extra_src.display().to_string(),
        "glob": "Bridge.sol",
        "context": 8,
        "limit": 20
    });

    let (admitted_definition, admitted_registration) = scenario(
        &provider,
        "grep_reader",
        &["grep"],
        vec![
            tool_response("grep-admitted-call", "grep", grep_arguments.clone()),
            FauxResponse::text("done"),
        ],
    );
    let (plain_definition, plain_registration) = scenario(
        &provider,
        "grep_scoped_out",
        &["grep"],
        vec![
            tool_response("grep-unadmitted-call", "grep", grep_arguments),
            FauxResponse::text("done"),
        ],
    );

    let admitted = reliability_fixture(
        root.path(),
        "parent-admitted",
        &parent_model,
        vec![extra.clone()],
        vec![(admitted_definition, admitted_registration)],
    )
    .await;
    let plain = reliability_fixture(
        root.path(),
        "parent-plain",
        &parent_model,
        Vec::new(),
        vec![(plain_definition, plain_registration)],
    )
    .await;

    // Admitted child: the real generation-bound Grep runs against the
    // absolute additional-root directory and succeeds.
    let admitted_results = run_hard_tasks(
        &admitted,
        vec![hard_task(
            "GrepAdmitted",
            "grep_reader",
            "Run the generation-bound Grep tool against the admitted additional workspace root.",
        )],
    )
    .await;
    let admitted_result = result_by_id(&admitted_results, "GrepAdmitted");
    assert!(
        admitted_result.error.is_none(),
        "{:?}",
        admitted_result.error
    );
    let messages = child_messages(&admitted, "GrepAdmitted");
    let admitted_grep = tool_results(&messages, "grep");
    assert_eq!(admitted_grep.len(), 1, "exactly one scripted grep call");
    assert!(!admitted_grep[0].is_error, "{:?}", admitted_grep[0].content);
    let admitted_text = result_text(admitted_grep[0]);
    assert!(
        admitted_text
            .contains("Bridge.sol:3:     function _materializeBucket() internal {"),
        "got: {admitted_text}"
    );
    assert!(
        admitted_text.contains("Bridge.sol-2-     // setup"),
        "context rows missing: {admitted_text}"
    );
    assert!(
        !admitted_text.contains("Other.sol"),
        "basename glob leaked Other.sol: {admitted_text}"
    );
    assert!(
        !admitted_text.contains("path resolved to no target"),
        "{admitted_text}"
    );

    // Unadmitted child: same absolute path, workspace without the additional
    // root — stable scope error, redacted, never a generic no-target error.
    let plain_results = run_hard_tasks(
        &plain,
        vec![hard_task(
            "GrepUnadmitted",
            "grep_scoped_out",
            "Run the generation-bound Grep tool against the unadmitted directory.",
        )],
    )
    .await;
    let plain_result = result_by_id(&plain_results, "GrepUnadmitted");
    let messages = child_messages(&plain, "GrepUnadmitted");
    let plain_grep = tool_results(&messages, "grep");
    assert_eq!(plain_grep.len(), 1, "exactly one scripted grep call");
    assert!(plain_grep[0].is_error, "scope rejection must be an error");
    let rejected_text = result_text(plain_grep[0]);
    assert!(
        rejected_text.starts_with("path_outside_workspace:"),
        "{rejected_text}"
    );
    assert!(
        rejected_text.contains("<absolute path>"),
        "rejected absolute path is redacted: {rejected_text}"
    );
    assert!(
        rejected_text.contains("--add-dir <project-root>"),
        "{rejected_text}"
    );
    assert!(
        !rejected_text.contains("path resolved to no target"),
        "must keep the stable scope error, never a generic no-target: {rejected_text}"
    );
    for undisclosed in [extra_src.as_path(), extra.as_path(), root.path()] {
        assert!(
            !rejected_text.contains(&undisclosed.display().to_string()),
            "must not disclose workspace configuration {undisclosed:?}: {rejected_text}"
        );
    }
    assert!(
        !plain_result.output.contains(&extra_src.display().to_string()),
        "job output must not disclose the unadmitted path: {:?}",
        plain_result.output
    );

    shutdown(admitted).await;
    shutdown(plain).await;
}

// ---------------------------------------------------------------------------
// 3: schema-invalid extra-field call scripted twice → doom before a third
//
// Contract (session.rs doom design): schema-validation rejections are
// deterministic — "an identical retry can never succeed, so the second
// attempt ends the turn instead of the third"
// (DOOM_LOOP_DETERMINISTIC_THRESHOLD / is_deterministic_error doc). The real
// agent loop's rejection text is `Invalid grep arguments: …`
// (loop_runtime::tool_validation_error), so `is_deterministic_error` must
// classify the colon form; the turn must terminate on the SECOND identical
// call, the third scripted model response stays unconsumed, and the grep
// executor count stays zero (every recorded result is the loop's schema
// validation rejection, never an executor output).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn schema_invalid_grep_extra_field_dooms_after_second_identical_call_with_zero_executor_runs()
{
    let root = tempfile::tempdir().expect("root");
    let provider = format!("subagent-tool-reliability-{}", uuid::Uuid::now_v7());
    let parent_model = Model {
        id: format!("subagent-reliability-parent-{}", uuid::Uuid::now_v7()),
        name: "Subagent Reliability Parent".to_owned(),
        api: format!("{provider}-parent-api"),
        provider: provider.clone(),
        ..Model::default()
    };
    let invalid_arguments = json!({ "pattern": "needle", "unknownField": true });
    let (definition, registration) = scenario(
        &provider,
        "doom_grep",
        &["grep"],
        vec![
            tool_response("doom-call-1", "grep", invalid_arguments.clone()),
            tool_response("doom-call-2", "grep", invalid_arguments),
            // Tripwire: consumed only if the after-hook/doom path fails to
            // terminate the turn after the second identical failure.
            FauxResponse::text("must-never-stream"),
        ],
    );
    let fixture = reliability_fixture(
        root.path(),
        "parent",
        &parent_model,
        Vec::new(),
        vec![(definition, registration)],
    )
    .await;

    let results = run_hard_tasks(
        &fixture,
        vec![hard_task(
            "DoomGrep",
            "doom_grep",
            "Attempt the same schema-invalid grep call until the run stops.",
        )],
    )
    .await;
    let result = result_by_id(&results, "DoomGrep");
    let error = result.error.as_deref().expect("doom path must fail the job");
    assert!(
        error.contains("repeated identical /grep arguments (2× identical errors)"),
        "deterministic doom must trip on the second identical failure: {error}"
    );
    assert!(
        !error.contains("No more faux responses queued"),
        "a third scripted turn must never happen: {error}"
    );
    assert!(
        !result.output.contains("must-never-stream"),
        "the tripwire response must stay unconsumed: {:?}",
        result.output
    );

    let messages = child_messages(&fixture, "DoomGrep");
    assert_eq!(
        tool_call_count(&messages, "grep"),
        2,
        "exactly two scripted attempts: {messages:?}"
    );
    let grep_results = tool_results(&messages, "grep");
    assert!(
        !grep_results.is_empty(),
        "the validation layer must record the rejected calls"
    );
    let validation_hits = grep_results
        .iter()
        .filter(|result| result_text(result).starts_with("Invalid grep arguments: "))
        .count();
    assert!(
        validation_hits >= 1,
        "schema validation must reject before the executor: {grep_results:?}"
    );
    let executor_runs = grep_results
        .iter()
        .filter(|result| {
            let text = result_text(result);
            !text.starts_with("Invalid grep arguments: ")
                && !text.contains("repeated identical /grep")
        })
        .count();
    assert_eq!(
        executor_runs, 0,
        "grep executor count must stay zero: {grep_results:?}"
    );
    // The rejection text pins the REAL generation-bound grep schema: the
    // stable reason plus the declared allowed fields, never executor output.
    let validation_text = result_text(
        grep_results
            .iter()
            .find(|result| result_text(result).starts_with("Invalid grep arguments: "))
            .expect("validation result"),
    );
    assert!(
        validation_text.contains("additional property not allowed"),
        "{validation_text}"
    );
    assert!(
        validation_text.contains(
            "Allowed fields: pattern, path, glob, ignoreCase, literal, context, limit"
        ),
        "{validation_text}"
    );

    shutdown(fixture).await;
}

// ---------------------------------------------------------------------------
// 5: delivered-yield outputSchema validation (separate assertion)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delivered_yield_validates_against_task_output_schema() {
    let root = tempfile::tempdir().expect("root");
    let provider = format!("subagent-tool-reliability-{}", uuid::Uuid::now_v7());
    let parent_model = Model {
        id: format!("subagent-reliability-parent-{}", uuid::Uuid::now_v7()),
        name: "Subagent Reliability Parent".to_owned(),
        api: format!("{provider}-parent-api"),
        provider: provider.clone(),
        ..Model::default()
    };
    let (valid_definition, valid_registration) = scenario(
        &provider,
        "schema_ok",
        &[],
        vec![tool_response(
            "yield-valid",
            "yield",
            json!({ "text": r#"{"summary":"bridge grep delivered"}"# }),
        )],
    );
    let (invalid_definition, invalid_registration) = scenario(
        &provider,
        "schema_bad",
        &["grep"],
        vec![tool_response(
            "yield-invalid",
            "yield",
            json!({ "text": r#"{"summary":42}"# }),
        )],
    );
    let fixture = reliability_fixture(
        root.path(),
        "parent",
        &parent_model,
        Vec::new(),
        vec![
            (valid_definition, valid_registration),
            (invalid_definition, invalid_registration),
        ],
    )
    .await;

    let output_schema = json!({
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
        "additionalProperties": false
    });
    let mut valid_item = hard_task(
        "SchemaValid",
        "schema_ok",
        "Deliver a conforming summary payload through yield.",
    );
    valid_item.output_schema = Some(output_schema.clone());
    valid_item.schema_mode = Some("strict".to_owned());
    let mut invalid_item = hard_task(
        "SchemaInvalid",
        "schema_bad",
        "Deliver a payload that violates the summary type.",
    );
    invalid_item.output_schema = Some(output_schema);
    invalid_item.schema_mode = Some("strict".to_owned());

    let results = run_hard_tasks(&fixture, vec![valid_item, invalid_item]).await;

    // Conforming delivery: validated, source "task", strict mode.
    let valid = result_by_id(&results, "SchemaValid");
    let structured = valid
        .structured_output
        .as_ref()
        .expect("delivered-yield validation report");
    assert_eq!(structured.schema_source, "task");
    assert_eq!(structured.schema_mode, "strict");
    assert!(structured.valid, "{:?}", structured.error);
    assert_eq!(
        structured.data.as_ref(),
        Some(&json!({ "summary": "bridge grep delivered" }))
    );
    assert_eq!(structured.error.as_deref(), None);
    assert!(valid.error.is_none(), "{:?}", valid.error);
    assert_eq!(valid.output, r#"{"summary":"bridge grep delivered"}"#);

    // Rejected delivery: report carries the payload and the failure; strict
    // mode surfaces it as the job error while the child still settled.
    let invalid = result_by_id(&results, "SchemaInvalid");
    let structured = invalid
        .structured_output
        .as_ref()
        .expect("delivered-yield validation report");
    assert_eq!(structured.schema_source, "task");
    assert_eq!(structured.schema_mode, "strict");
    assert!(!structured.valid);
    assert_eq!(structured.data.as_ref(), Some(&json!({ "summary": 42 })));
    let validation_error = structured.error.as_deref().expect("validation error");
    assert!(
        validation_error.contains("delivered payload failed outputSchema validation"),
        "{validation_error}"
    );
    let error = invalid.error.as_deref().expect("strict mode must fail the job");
    assert!(
        error.contains("delivered payload failed outputSchema validation"),
        "{error}"
    );
    assert_eq!(invalid.output, r#"{"summary":42}"#);

    shutdown(fixture).await;
}