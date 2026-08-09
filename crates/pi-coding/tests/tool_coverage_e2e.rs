//! Public `create_tool(name, cwd)` boundary coverage for built-in tools that
//! lack an end-to-end test at that boundary.
//!
//! The per-tool coverage audit lives in the D15 batch report; this file adds
//! the genuinely missing public-boundary cases:
//!
//! - `grep` / `find` / `ls` — exercised at the `AgentTool` boundary in-module
//!   (tools.rs), but never through the public `create_tool` factory.
//! - `ast_grep` / `ast_edit` — in-module tests drive the internal run
//!   functions; this file scripts the real tool closures.
//! - `browser` — no tool-execution test exists anywhere (unit tests only
//!   cover parsing/validation/discovery); skip-guarded on Chrome and driven
//!   against a deterministic `data:` URL so no network is involved.
//!   (`web_search` stays covered by its in-module offline-seam + parsing
//!   tests; a live-network call would be non-deterministic.)
//!
//! Tools with existing tool-boundary e2e are intentionally NOT duplicated:
//! `read`/`edit`/`write`/`bash`/`glob` (coding_tools_lifecycle.rs,
//! brush_bash_e2e.rs, sandbox_smoke.rs), `debug` (debug_tool_dap_e2e.rs),
//! `eval` (eval.rs `eval_tool_*`), `memory`/`recall`/`retain`/`reflect`
//! (memory.rs fake-CLI round trips), `mcp` (mcp.rs fake-server client + tool
//! execution), `github` (github.rs real gh smoke), `lsp` (lsp.rs rust-analyzer
//! smoke), `generate_image` (image_gen/tests.rs mock HTTP),
//! `inspect_image`/`notebook` (image.rs / notebook.rs tool execution), `ask`
//! (session.rs ask round trips), `todo`/`task`/`hub`/`process` (todo_dag_*,
//! orchestration_*, role_contracts.rs).

use std::sync::Arc;

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::ContentBlock;
use pi_coding::create_tool;
use serde_json::{Value, json};
use tempfile::TempDir;

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
// grep / find / ls — public `create_tool` boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_tool_matches_lines_and_reports_no_match() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("grep.txt"), b"alpha\nbeta needle\ngamma\n").expect("file");
    let tool = create_tool("grep", dir.path().to_str().expect("utf8")).expect("grep tool");

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
    let tool = create_tool("find", dir.path().to_str().expect("utf8")).expect("find tool");

    let hit = call(&tool, json!({ "pattern": "*.rs" })).await;
    let text = text_of(&hit);
    assert!(text.contains("sub/b.rs"), "got: {text}");
    assert!(!text.contains("a.txt"), "got: {text}");

    let miss = call(&tool, json!({ "pattern": "nomatch*" })).await;
    assert_eq!(text_of(&miss), "No files found matching pattern");

    let missing = call_err(&tool, json!({ "pattern": "*.rs", "path": "nope" })).await;
    assert!(missing.contains("Path not found"), "got: {missing}");
}

#[tokio::test]
async fn ls_tool_lists_entries_and_rejects_bad_paths() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("b.txt"), b"b").expect("file");
    std::fs::write(dir.path().join("a.txt"), b"a").expect("file");
    std::fs::create_dir(dir.path().join("sub")).expect("subdir");
    let tool = create_tool("ls", dir.path().to_str().expect("utf8")).expect("ls tool");

    let listed = call(&tool, json!({})).await;
    let text = text_of(&listed);
    assert!(text.contains("a.txt"), "got: {text}");
    assert!(text.contains("b.txt"), "got: {text}");
    assert!(text.contains("sub/"), "directories must carry a trailing slash: {text}");
    assert!(
        text.find("a.txt").unwrap() < text.find("b.txt").unwrap(),
        "entries must be sorted: {text}"
    );

    let missing = call_err(&tool, json!({ "path": "nope" })).await;
    assert!(missing.contains("Path not found"), "got: {missing}");

    let file = call_err(&tool, json!({ "path": "a.txt" })).await;
    assert!(file.contains("Not a directory"), "got: {file}");
}

// ---------------------------------------------------------------------------
// ast_grep / ast_edit — public `create_tool` boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ast_grep_tool_searches_structure_and_validates() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "fn main() {\n    let x = Some(123);\n}\n",
    )
    .expect("source file");
    let tool = create_tool("ast_grep", dir.path().to_str().expect("utf8")).expect("ast_grep tool");

    let hit = call(&tool, json!({ "pattern": "Some($A)", "path": "lib.rs" })).await;
    assert!(
        text_of(&hit).contains("lib.rs:2:13: Some(123)"),
        "got: {}",
        text_of(&hit)
    );

    let miss = call(&tool, json!({ "pattern": "let $B = 999;", "path": "lib.rs" })).await;
    assert_eq!(text_of(&miss), "No matches found");

    let invalid = call_err(&tool, json!({ "pattern": "$$$A", "path": "lib.rs" })).await;
    assert!(invalid.contains("invalid ast-grep pattern"), "got: {invalid}");
}

#[tokio::test]
async fn ast_edit_tool_rewrites_file_on_disk() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, "let a = 1;\nlet b = 1;\n").expect("source file");
    let tool = create_tool("ast_edit", dir.path().to_str().expect("utf8")).expect("ast_edit tool");

    let result = call(
        &tool,
        json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "lib.rs" }),
    )
    .await;
    assert!(text_of(&result).contains("2 replacement(s) in lib.rs."), "got: {}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(&path).expect("rewritten file"),
        "let a = 2;\nlet b = 2;\n"
    );

    let miss = call(
        &tool,
        json!({ "pattern": "fn nope() {}", "rewrite": "fn nope() { 1 }", "path": "lib.rs" }),
    )
    .await;
    assert!(text_of(&miss).contains("No replacements in lib.rs."), "got: {}", text_of(&miss));
}

// ---------------------------------------------------------------------------
// browser — skip-guarded on a Chrome/Chromium binary; deterministic data: URL
// ---------------------------------------------------------------------------

/// Chrome/Chromium discovery mirroring the tool's own candidates: `CHROME_PATH`
/// first, then well-known Linux/macOS install locations and PATH entries.
fn chrome_usable() -> bool {
    let candidates = std::env::var("CHROME_PATH")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(|p| vec![std::path::PathBuf::from(p)])
        .unwrap_or_else(|| {
            ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/opt/google/chrome/chrome"]
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect()
        });
    let on_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .flat_map(|dir| [dir.join("google-chrome"), dir.join("chromium")])
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    candidates
        .into_iter()
        .chain(on_path)
        .any(|p| p.is_file() && {
            std::process::Command::new(&p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
}

fn data_url(html: &str) -> String {
    let mut encoded = String::with_capacity(html.len());
    for byte in html.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' | b':' | b';' | b'/' | b'?' | b'@' | b'!' | b'$' | b'\'' | b'(' | b')' | b'*' | b'+' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push_str("%20"),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("data:text/html,{encoded}")
}

#[tokio::test]
async fn browser_tool_navigates_data_url_and_interacts() {
    if !chrome_usable() {
        eprintln!("browser e2e: SKIP (no Chrome/Chromium binary found)");
        return;
    }
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path().to_str().expect("utf8");
    let tool = create_tool("browser", cwd).expect("browser tool");
    // The click handler mutates the button's own text so the tool's same-call
    // post-click text poll observes the change (each call spawns a fresh
    // browser, so page state does not survive across calls).
    let page = data_url(
        r#"<html><body><h1 id="t">Hello Browser</h1><button id="b" onclick="this.textContent='clicked'">Go</button></body></html>"#,
    );

    let navigated = call(&tool, json!({ "action": "navigate", "url": page })).await;
    assert!(text_of(&navigated).contains("Navigated to"), "got: {}", text_of(&navigated));

    // Each call spawns a fresh browser at about:blank, so non-navigate
    // actions re-navigate first via their optional `url` argument.
    let extracted = call(&tool, json!({ "action": "extract", "url": page })).await;
    assert!(text_of(&extracted).contains("Hello Browser"), "got: {}", text_of(&extracted));

    let clicked = call(&tool, json!({ "action": "click", "selector": "#b", "url": page })).await;
    let click_text = text_of(&clicked);
    assert!(click_text.contains("Clicked"), "got: {click_text}");
    assert!(
        click_text.contains("clicked"),
        "the click handler must run and the tool must report the changed text: {click_text}"
    );

    let shot = call(&tool, json!({ "action": "screenshot", "path": "shot.png", "url": page })).await;
    assert!(text_of(&shot).contains("shot.png"), "got: {}", text_of(&shot));
    let bytes = std::fs::read(dir.path().join("shot.png")).expect("screenshot file");
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "screenshot must be a PNG");

    let tabs = call(&tool, json!({ "action": "list_tabs", "url": page })).await;
    assert!(text_of(&tabs).contains("tab(s)"), "got: {}", text_of(&tabs));

    let closed = call(&tool, json!({ "action": "close", "url": page })).await;
    assert!(text_of(&closed).contains("browser closed"), "got: {}", text_of(&closed));
}
