//! Deterministic integration coverage for coding-tool lifecycles.
//!
//! Each case scripts tool calls through the public `AgentTool` boundary and
//! defends on-disk bytes (or spill-file presence/absence), not mock echo.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use pi_agent::{
    AbortController, AgentTool, AgentToolResult, ThinkingLevel, ToolCallContext, ToolUpdateFn,
};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{ContentBlock, Model, SimpleStreamOptions};
use pi_coding::{
    AgentDefinition, Application, ApplicationRuntimeCandidate, ChildSessionRequest,
    OrchestrationRuntime, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, ToolSelection, YieldState, cleanup_full_output_path,
};
use serde_json::{Value, json};

fn session_options(cwd: &Path, model: Model) -> SessionOptions {
    SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
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
    }
}

/// Agent configuration and SessionStore directories are kept OUTSIDE the
/// writable `cwd` workspace (published-generation hermetic pattern). A
/// process-lifetime tempdir root stays alive for the whole suite, so no
/// per-test TempDir needs to be threaded through the callers.
fn external_config_roots() -> (PathBuf, PathBuf) {
    static ROOT: LazyLock<tempfile::TempDir> =
        LazyLock::new(|| tempfile::tempdir().expect("coding tools lifecycle config root"));
    let suffix = uuid::Uuid::now_v7().to_string();
    let base = ROOT.path().join(suffix);
    let agent_dir = base.join("agent");
    let session_dir = base.join("sessions");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    (agent_dir, session_dir)
}
async fn isolated_application(
    cwd: &Path,
    selection: ToolSelection,
) -> (
    Application,
    Vec<AgentTool>,
    FauxProviderRegistration,
    ResourceManager,
    Vec<String>,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("coding-tools-lifecycle-api-{suffix}");
    let provider = format!("coding-tools-lifecycle-provider-{suffix}");
    let model = Model {
        id: "coding-tools-lifecycle".to_owned(),
        name: "Coding Tools Lifecycle".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });

    let (agent_dir, session_dir) = external_config_roots();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"todo":false}}"#,
    )
    .expect("danger settings");

    let mut resource_options = ResourceManagerOptions::new(cwd);
    resource_options.agent_dir = agent_dir;
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("resource manager");

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        session_options(cwd, model.clone()),
        Vec::new(),
        selection.clone(),
        ResourceDiscovery::Disabled,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("bootstrap application");
    let requested_tool_names = if selection.enable_glob {
        vec![
            "read",
            "bash",
            "browser",
            "edit",
            "write",
            "ast_edit",
            "generate_image",
            "memory",
            "glob",
        ]
    } else {
        vec![
            "read",
            "bash",
            "browser",
            "edit",
            "write",
            "ast_edit",
            "generate_image",
            "memory",
        ]
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();

    let tools_session = OrchestrationRuntime::child_factory_from_session(&application.session())(
        ChildSessionRequest {
            child_id: "CodingToolsLifecycle".to_owned(),
            parent_id: "Main".to_owned(),
            max_tools_per_agent: 16,
            depth: 1,
            definition: AgentDefinition::default(),
            assignment: "exercise governed coding tools".to_owned(),
            system_prompt: String::new(),
            requested_tool_names: Some(requested_tool_names),
            orchestration_tools: Vec::new(),
            persona_skill_store: None,
            persona_skills: Arc::new(Vec::new()),
            shared_skills: Arc::new(Vec::new()),
            thinking_level: Some(ThinkingLevel::Off),
            model,
            resolved_model_sequence: Vec::new(),
            resolved_model_selected_index: 0,
            yield_state: Arc::new(YieldState::default()),
            output_schema: None,
            schema_mode: None,
        },
    )
    .await
    .expect("generation-bound tools session");
    let active_tool_names = tools_session
        .get_active_tool_names()
        .into_iter()
        .filter(|name| name != "yield")
        .collect();
    let tools = tools_session
        .get_all_tools()
        .into_iter()
        .filter(|tool| tool.name != "yield")
        .collect();
    (
        application,
        tools,
        registration,
        resources,
        active_tool_names,
    )
}

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    ToolCallContext {
        tool_call_id: "coding-tools-lifecycle".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
        approval_token: None,
    }
}

fn make_aborted_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    controller.abort();
    ToolCallContext {
        tool_call_id: "coding-tools-aborted".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
        approval_token: None,
    }
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

fn tool_named(tools: &[AgentTool], name: &str) -> AgentTool {
    tools
        .iter()
        .find(|tool| tool.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("missing tool {name}"))
}

async fn call(tool: &AgentTool, arguments: Value) -> AgentToolResult {
    (tool.execute)(make_ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("{} call failed: {error:#}", tool.name))
}

async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(make_ctx(arguments))
        .await
        .expect_err("expected tool error")
        .to_string()
}

fn extract_full_output_path(text: &str) -> Option<String> {
    text.split("Full output: ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
}

/// read → edit → read must land the edit on disk and re-read the new bytes.
#[tokio::test]
async fn read_edit_read_campaign_persists_and_rereads_on_disk_bytes() {
    let cwd = tempfile::tempdir().expect("cwd");
    let path = cwd.path().join("src/lib.rs");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"fn answer() -> i32 {\n    1\n}\n").expect("seed");

    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let read = tool_named(&tools, "read");
    let edit = tool_named(&tools, "edit");

    let before = call(&read, json!({ "path": "src/lib.rs" })).await;
    assert!(
        text_of(&before).contains("    1"),
        "initial read must surface seeded bytes: {}",
        text_of(&before)
    );

    let edited = call(
        &edit,
        json!({
            "path": "src/lib.rs",
            "edits": [{ "oldText": "    1", "newText": "    42" }]
        }),
    )
    .await;
    assert!(
        text_of(&edited).contains("Successfully replaced 1 block"),
        "edit success text missing: {}",
        text_of(&edited)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read disk after edit"),
        "fn answer() -> i32 {\n    42\n}\n",
        "edit must rewrite on-disk bytes"
    );

    let after = call(&read, json!({ "path": "src/lib.rs" })).await;
    let after_text = text_of(&after);
    assert!(
        after_text.contains("    42"),
        "follow-up read must observe edited bytes: {after_text}"
    );
    assert!(
        !after_text.contains("    1\n"),
        "follow-up read must not keep stale seed: {after_text}"
    );
}

/// write must create missing parent directories and leave the requested bytes.
#[tokio::test]
async fn write_creates_missing_parent_directories_and_file_bytes() {
    let cwd = tempfile::tempdir().expect("cwd");
    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let write = tool_named(&tools, "write");
    let read = tool_named(&tools, "read");

    let nested = PathBuf::from("deep/nested/out/note.txt");
    let absolute = cwd.path().join(&nested);
    assert!(!absolute.exists(), "fixture must start without target");
    assert!(
        !absolute.parent().expect("parent").exists(),
        "fixture must start without parent dirs"
    );

    let written = call(
        &write,
        json!({
            "path": nested.to_string_lossy(),
            "content": "parent-created\n"
        }),
    )
    .await;
    assert!(
        text_of(&written).contains("Successfully wrote"),
        "write success missing: {}",
        text_of(&written)
    );
    assert!(
        absolute.parent().expect("parent").is_dir(),
        "parents must exist"
    );
    assert_eq!(
        std::fs::read_to_string(&absolute).expect("disk bytes"),
        "parent-created\n"
    );

    let reread = call(&read, json!({ "path": nested.to_string_lossy() })).await;
    assert!(
        text_of(&reread).contains("parent-created"),
        "read after write missed bytes: {}",
        text_of(&reread)
    );
}

/// Non-unique edit oldText must fail without mutating the file.
#[tokio::test]
async fn edit_non_unique_old_text_fails_and_leaves_disk_unchanged() {
    let cwd = tempfile::tempdir().expect("cwd");
    let path = cwd.path().join("dup.txt");
    let original = "alpha\nalpha\nbeta\n";
    std::fs::write(&path, original).expect("seed");

    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let edit = tool_named(&tools, "edit");

    let error = call_err(
        &edit,
        json!({
            "path": "dup.txt",
            "edits": [{ "oldText": "alpha", "newText": "ALPHA" }]
        }),
    )
    .await;
    assert!(
        error.contains("2 occurrences") && error.contains("must be unique"),
        "expected non-unique contract error, got: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("disk after failed edit"),
        original,
        "failed non-unique edit must not rewrite disk"
    );
}

/// Successful truncated bash publishes generation scratch; cleanup releases it.
#[tokio::test]
async fn bash_success_spill_publishes_then_cleanup_removes_on_disk_bytes() {
    let cwd = tempfile::tempdir().expect("cwd");
    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let bash = tool_named(&tools, "bash");
    let read = tool_named(&tools, "read");

    // >50 KiB display cap forces a detached full-output spill on success.
    let result = call(
        &bash,
        json!({ "command": "yes x | head -c 60000", "timeout": 10 }),
    )
    .await;
    let text = text_of(&result);
    let path = extract_full_output_path(&text)
        .expect("successful truncated bash must publish Full output path");

    let spill = call(&read, json!({ "path": path })).await;
    let spill_text = text_of(&spill);
    assert!(
        spill_text.contains("[Showing lines 1-2000 of "),
        "generation-bound read must resolve the complete registered spill: {spill_text}"
    );

    cleanup_full_output_path(&path);
    call_err(&read, json!({ "path": path })).await;
    // Idempotent second cleanup must not panic or recreate the registered spill.
    cleanup_full_output_path(&path);
    call_err(&read, json!({ "path": path })).await;
}

/// Aborted bash must not leave a published full-output spill behind.
#[tokio::test]
async fn bash_abort_cleans_spill_and_reports_command_aborted() {
    let cwd = tempfile::tempdir().expect("cwd");
    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let bash = tool_named(&tools, "bash");

    let (controller, abort) = AbortController::new();
    let abort_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        controller.abort();
    });
    let error = (bash.execute)(ToolCallContext {
        tool_call_id: "bash-abort".to_owned(),
        // Continuous large output so a spill is created before abort wins.
        arguments: json!({ "command": "yes x", "timeout": 30 }),
        on_update: noop_update(),
        abort,
        model: None,
        approval_token: None,
    })
    .await
    .expect_err("aborted bash must error")
    .to_string();
    abort_task.await.expect("abort task");

    assert!(
        error.contains("Command aborted"),
        "expected abort status, got: {error}"
    );
    assert!(
        !error.contains("Full output:"),
        "abort path must not publish a dead full-output path: {error}"
    );
}

/// Pre-aborted bash context must fail before mutating the workspace.
#[tokio::test]
async fn bash_pre_aborted_context_does_not_create_workspace_side_effects() {
    let cwd = tempfile::tempdir().expect("cwd");
    let marker = cwd.path().join("should-not-exist.txt");
    let (_application, tools, _registration, _resources, _active_tool_names) =
        isolated_application(cwd.path(), ToolSelection::default()).await;
    let bash = tool_named(&tools, "bash");

    let error = (bash.execute)(make_aborted_ctx(json!({
        "command": format!("printf boom > '{}'", marker.display())
    })))
    .await
    .expect_err("pre-aborted bash must fail")
    .to_string();
    assert_eq!(error, "Operation aborted");
    assert!(
        !marker.exists(),
        "pre-aborted bash must not create workspace files"
    );
}

/// Glob stays opt-in on the parent (main) session catalog — the
/// `ToolSelection.enable_glob` gate — and the governed generation-bound
/// invocation matches on-disk files. The parent session is built with the
/// caller's selection, so a regression that always/never adds glob to the
/// main catalog fails here.
#[tokio::test]
async fn opt_in_glob_tool_is_absent_by_default_and_matches_disk_when_enabled() {
    let cwd = tempfile::tempdir().expect("cwd");
    std::fs::write(cwd.path().join("keep.rs"), b"fn keep() {}\n").expect("keep");
    std::fs::write(cwd.path().join("skip.ts"), b"const skip = 1;\n").expect("skip");

    let (
        _default_application,
        default_tools,
        _default_registration,
        _default_resources,
        default_active_tool_names,
    ) = isolated_application(cwd.path(), ToolSelection::default()).await;
    assert_eq!(
        default_active_tool_names,
        [
            "read",
            "bash",
            "browser",
            "edit",
            "write",
            "ast_edit",
            "generate_image",
            "memory"
        ]
    );
    assert!(
        default_tools.iter().all(|tool| tool.name != "glob"),
        "glob must stay opt-in off the governed tool catalog"
    );

    let (_with_glob, glob_tools, _glob_registration, _glob_resources, glob_active_tool_names) =
        isolated_application(
            cwd.path(),
            ToolSelection {
                enable_glob: true,
                ..ToolSelection::default()
            },
        )
        .await;
    assert!(
        glob_active_tool_names.iter().any(|name| name == "glob"),
        "enable_glob must surface glob on the active catalog"
    );
    assert!(
        glob_tools.iter().any(|tool| tool.name == "glob"),
        "enable_glob must surface glob on the governed tool catalog"
    );
    let glob = tool_named(&glob_tools, "glob");

    let matched = call(&glob, json!({ "pattern": "*.rs" })).await;
    let text = text_of(&matched);
    assert!(
        text.contains("keep.rs"),
        "opt-in glob must match on-disk rust file: {text}"
    );
    assert!(
        !text.contains("skip.ts"),
        "opt-in glob must not match ts when pattern is *.rs: {text}"
    );

    // A second generation-bound invocation stays aligned with opt-in execution.
    let again = call(&glob, json!({ "pattern": "keep.rs" })).await;
    assert!(
        text_of(&again).contains("keep.rs"),
        "generation-bound glob must also observe on-disk bytes"
    );
}
