//! Generation-bound `AgentTool` coverage for built-in tools whose public
//! `create_tool` factory is a fail-closed stub (`requires an application
//! generation`). These tests publish a hermetic application and call the
//! tool the session actually runs.
//!
//! - `grep` / `find` / `ls` — in-module tests cover the happy path; this file
//!   also checks empty results and missing paths through that same generation.
//! - `ast_grep` / `ast_edit` — in-module tests drive internal helpers; this
//!   file scripts the published tool closures.
//!
//! Tools with existing tool-boundary e2e are intentionally NOT duplicated:
//! `read`/`edit`/`write`/`bash`/`glob` (coding_tools_lifecycle.rs,
//! brush_bash_e2e.rs), `debug` (debug_tool_dap_e2e.rs),
//! `eval` (eval.rs `eval_tool_*`), `memory`/`recall`/`retain`/`reflect`
//! (memory.rs fake-CLI round trips), `mcp` (mcp.rs fake-server client + tool
//! execution), `github` (github.rs real gh smoke), `lsp` (lsp.rs rust-analyzer
//! smoke), `generate_image` (image_gen/tests.rs mock HTTP),
//! `inspect_image`/`notebook` (image.rs / notebook.rs tool execution), `ask`
//! (session.rs ask round trips), `todo`/`task`/`hub`/`process` (todo_dag_*,
//! orchestration_*, role_contracts.rs).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pi_agent::{AbortController, AgentTool, AgentToolResult, ThinkingLevel, ToolCallContext, ToolUpdateFn};
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_ai::{ContentBlock, Model, SimpleStreamOptions};
use pi_coding::{
    Application, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct GovernedTool {
    tool: AgentTool,
    _application: Application,
    _agent_home: TempDir,
    _sessions: TempDir,
    _registration: pi_ai::providers::FauxProviderRegistration,
}

async fn governed_tool(name: &str, cwd: &Path) -> GovernedTool {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
    let api = format!("tool-coverage-api-{suffix}");
    let mut model = Model::default();
    model.id = format!("tool-coverage-{suffix}");
    model.name = "Tool Coverage".into();
    model.api = api.clone();
    model.provider = format!("tool-coverage-provider-{suffix}");
    model.base_url = "http://127.0.0.1:9".into();
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 16,
    });
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
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
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("coverage session");
    let sessions = TempDir::new().expect("coverage sessions");
    session.set_session_dir(sessions.path().to_path_buf());
    let agent_home = TempDir::new().expect("coverage agent home");
    let agent_dir = agent_home.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    options.project_trust_override = Some(true);
    session
        .attach_resources(ResourceManager::new(options).expect("coverage resources"))
        .await
        .expect("attach coverage resources");
    let application = Application::new(session).await;
    let tool = application
        .get_tool_definition(name)
        .unwrap_or_else(|| panic!("{name} requires a published application generation"));
    GovernedTool {
        tool,
        _application: application,
        _agent_home: agent_home,
        _sessions: sessions,
        _registration: registration,
    }
}

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    ToolCallContext {
        tool_call_id: "tool-coverage-e2e".to_owned(),
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

async fn call(tool: &AgentTool, arguments: Value) -> AgentToolResult {
    (tool.execute)(ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("{} call failed: {error:#}", tool.name))
}

async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(ctx(arguments))
        .await
        .expect_err("expected an error")
        .to_string()
}

// ---------------------------------------------------------------------------
// grep / find / ls — published generation boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_tool_matches_lines_and_reports_no_match() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("grep.txt"), b"alpha\nbeta needle\ngamma\n").expect("file");
    let governed = governed_tool("grep", dir.path()).await;
    let tool = &governed.tool;

    let hit = call(&tool, json!({ "pattern": "needle" })).await;
    let text = text_of(&hit);
    assert!(text.contains("grep.txt:2: beta needle"), "got: {text}");

    let miss = call(&tool, json!({ "pattern": "zzz" })).await;
    assert_eq!(text_of(&miss), "No matches found");

    let invalid = call_err(&tool, json!({ "pattern": "(" })).await;
    assert!(invalid.contains("invalid regex"), "got: {invalid}");
}

#[tokio::test]
async fn find_tool_matches_globs_and_reports_empty() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("a.txt"), b"a").expect("file");
    std::fs::create_dir_all(dir.path().join("sub")).expect("subdir");
    std::fs::write(dir.path().join("sub").join("b.rs"), b"b").expect("file");
    let governed = governed_tool("find", dir.path()).await;
    let tool = &governed.tool;

    let hit = call(&tool, json!({ "pattern": "*.rs" })).await;
    let text = text_of(&hit);
    assert!(text.contains("sub/b.rs"), "got: {text}");
    assert!(!text.contains("a.txt"), "got: {text}");

    let miss = call(&tool, json!({ "pattern": "nomatch*" })).await;
    assert_eq!(text_of(&miss), "No files found matching pattern");

    let missing = call_err(&tool, json!({ "pattern": "*.rs", "path": "nope" })).await;
    assert!(
        missing.contains("NotFound"),
        "missing search path must surface as NotFound: {missing}"
    );
}

#[tokio::test]
async fn ls_tool_lists_entries_and_rejects_bad_paths() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("b.txt"), b"b").expect("file");
    std::fs::write(dir.path().join("a.txt"), b"a").expect("file");
    std::fs::create_dir(dir.path().join("sub")).expect("subdir");
    let governed = governed_tool("ls", dir.path()).await;
    let tool = &governed.tool;

    let listed = call(&tool, json!({})).await;
    let text = text_of(&listed);
    assert!(text.contains("a.txt"), "got: {text}");
    assert!(text.contains("b.txt"), "got: {text}");
    assert!(
        text.contains("sub/"),
        "directories must carry a trailing slash: {text}"
    );
    assert!(
        text.find("a.txt").unwrap() < text.find("b.txt").unwrap(),
        "entries must be sorted: {text}"
    );

    let missing = call_err(&tool, json!({ "path": "nope" })).await;
    assert!(
        missing.contains("NotFound"),
        "missing directory must surface as NotFound: {missing}"
    );

    let file = call_err(&tool, json!({ "path": "a.txt" })).await;
    assert!(
        file.contains("NotADirectory"),
        "a file path must be rejected as a directory: {file}"
    );
}

// ---------------------------------------------------------------------------
// ast_grep / ast_edit — published generation boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ast_grep_tool_searches_structure_and_validates() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "fn main() {\n    let x = Some(123);\n}\n",
    )
    .expect("source file");
    let governed = governed_tool("ast_grep", dir.path()).await;
    let tool = &governed.tool;

    let hit = call(&tool, json!({ "pattern": "Some($A)", "path": "lib.rs" })).await;
    assert!(
        text_of(&hit).contains("lib.rs:2:13: Some(123)"),
        "got: {}",
        text_of(&hit)
    );

    let miss = call(
        &tool,
        json!({ "pattern": "let $B = 999;", "path": "lib.rs" }),
    )
    .await;
    assert_eq!(text_of(&miss), "No matches found");

    let invalid = call_err(&tool, json!({ "pattern": "$$$A", "path": "lib.rs" })).await;
    assert!(
        invalid.contains("invalid ast-grep pattern"),
        "got: {invalid}"
    );
}

#[tokio::test]
async fn ast_edit_tool_rewrites_file_on_disk() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, "let a = 1;\nlet b = 1;\n").expect("source file");
    let governed = governed_tool("ast_edit", dir.path()).await;
    let tool = &governed.tool;

    let result = call(
        &tool,
        json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "lib.rs" }),
    )
    .await;
    assert!(
        text_of(&result).contains("2 replacement(s) in lib.rs."),
        "got: {}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("rewritten file"),
        "let a = 2;\nlet b = 2;\n"
    );

    let miss = call(
        &tool,
        json!({ "pattern": "fn nope() {}", "rewrite": "fn nope() { 1 }", "path": "lib.rs" }),
    )
    .await;
    assert!(
        text_of(&miss).contains("No replacements in lib.rs."),
        "got: {}",
        text_of(&miss)
    );
}
