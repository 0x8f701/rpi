//! Built-in coding tools (port of pi's `coding/tools.go`): read, write, edit,
//! bash, ls, find, and grep. Relative filesystem paths resolve from `cwd`.
//!
//! Tool definitions, parameter schemas, prompt guidelines, path resolution,
//! edit unique/non-overlapping replacement semantics, bounded output,
//! binary/image recognition, bash timeout/cancellation with streaming updates,
//! and gitignore-aware find/grep. Reusable helpers live in private sibling
//! modules; this module assembles the tools and exposes the factory API used
//! by the Session facade.

mod ast_edit;
mod ast_grep;
mod web_search;
mod bash;
mod browser;
mod debug;
mod doc_convert;
mod editdiff;
mod editmatch;
mod eval;
/// Shared `Content-Length` JSON-RPC framing (LSP/MCP/DAP/ACP). Public so the
/// ACP server in pi-cli reuses the same wire format instead of reimplementing
/// header parsing.
pub mod framing;
pub(crate) mod ask;
mod github;
mod glob;
mod image;
mod image_gen;
mod imageresize;
mod lsp;
mod lsp_client;
mod mime;
mod mutation_queue;
mod notebook;
mod paths;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use pi_agent::{
    AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability, ToolExecutionMode,
    ToolUpdateFn,
};
use pi_ai::{ConstrainedSampling, ConstrainedSamplingStrictness, ContentBlock, Schema};

use crate::truncate::{
    format_size, truncate_head, truncate_line, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES, FIND_DEFAULT_LIMIT, GLOB_DEFAULT_LIMIT, GLOB_MAX_LIMIT,
    GREP_DEFAULT_LIMIT, GREP_MAX_LINE_LENGTH, LS_DEFAULT_LIMIT, utf16_len,
};
use crate::todo::{TodoRuntime, TodoToolDetails, TODO_ERROR_MARKER, deserialize_todo_op, tool_failure_result};
use crate::SandboxConfig;
use crate::sandbox::SandboxRunOutcome;

use bash::{OutputAccumulator, OutputSnapshot};
use doc_convert::{extract_doc_text, is_doc};
use editdiff::generate_edit_details;
use editmatch::{
    apply_edits_to_normalized_content, detect_line_ending, normalize_to_lf, restore_line_endings,
    strip_bom, EditEntry,
};
use glob::{match_fd_glob, match_rg_glob, IgnoreStack};
use github::github_tool;
use browser::browser_tool_for_workspace;
use image::inspect_image_tool;
use ast_edit::ast_edit_tool;
use ast_grep::ast_grep_tool;
use debug::debug_tool;
use eval::eval_tool;
use image_gen::{generate_image_tool, generate_image_tool_for_workspace};
use notebook::notebook_tool;
use lsp::{lsp_tool, lsp_tool_with_rules};
use crate::memory::{
    memory_tool, memory_tool_with_session_env, memory_tools_for, memory_tools_for_persona,
    recall_tool, reflect_tool, retain_tool, MemoryConfig,
};
use crate::mcp::{McpRegistry, mcp_tool};
use web_search::web_search_tool;
use imageresize::process_image;
use mime::{detect_supported_image_mime_type_from_file, is_pdf};
use mutation_queue::with_file_mutation_queue;
use paths::{resolve_mutation_path, resolve_read_path, resolve_scoped_path};

pub use image_gen::{ImageGenConfig, ImageGenConfigFn};

/// pi `MAX_TIMEOUT_MS` (INT32_MAX): the bash tool rejects any timeout that
/// would exceed this many milliseconds (`resolveTimeoutMs`).
const MAX_BASH_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// Renders pi's `MAX_TIMEOUT_SECONDS` (2147483.647) for the rejection message.
const MAX_BASH_TIMEOUT_SECONDS: &str = "2147483.647";
/// JavaScript's maximum exactly representable integer, used before numeric casts.
const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const NON_VISION_IMAGE_NOTE: &str =
    "[Current model does not support images. The image will be omitted from this request.]";
const MAX_INLINE_VIDEO_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INLINE_VIDEO_BASE64_BYTES: usize = 3 * 1024 * 1024;

/// The PI_* session metadata variables pi manages for bash commands. They are
/// stripped from the inherited environment before every run so a parent
/// process's stale values never leak into a child.
const PI_SESSION_ENV_KEYS: &[&str] = &[
    "PI_SESSION_ID",
    "PI_SESSION_FILE",
    "PI_PROVIDER",
    "PI_MODEL",
    "PI_REASONING_LEVEL",
];

/// Throttle window for partial bash `onUpdate` emits (leading + trailing edge).
const BASH_UPDATE_THROTTLE: Duration = Duration::from_millis(100);

/// Supplies the PI_* session metadata exposed to bash commands. Called per
/// execution so a mid-session change is reflected. `None` for standalone
/// construction (matches pi's `exposeSessionEnvironment && ctx` guard).
pub type SessionEnvFn = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;
/// Resolves the current bash sandbox configuration from live settings. Called
/// per execution so a settings reload takes effect on the next spawn (the
/// sandbox flags apply per spawn). `None` means the sandbox is not configured
/// (settings.sandbox absent); a per-call `sandboxed` parameter still works in
/// that case with default allowed paths (cwd + agent dir).
pub type SandboxConfigFn = Arc<dyn Fn() -> Option<crate::SandboxConfig> + Send + Sync>;
pub type SkillSnapshotFn = Arc<dyn Fn() -> Vec<crate::Skill> + Send + Sync>;
/// Resolves internal `agent://`, `history://`, and `artifact://` URIs passed to
/// the `read` tool to a local file path. Recognized internal schemes MUST
/// propagate errors (no fall-through); unrecognized schemes fall through to
/// ordinary file-path resolution. `None` for standalone construction.
pub type InternalUriResolverFn = Arc<dyn Fn(&str) -> anyhow::Result<std::path::PathBuf> + Send + Sync>;
/// Resolves the live `settings.memory` configuration for memory-tool builds
/// and turn-start injection. Returns `None` before settings are attached
/// (fall back to the built-in `local` backend).
pub type MemoryConfigFn = Arc<dyn Fn() -> Option<MemoryConfig> + Send + Sync>;
#[derive(Clone)]
pub struct BashProcessContext {
    pub manager: crate::ProcessManager,
    pub owner_id: crate::ProcessOwnerId,
}

fn factory_workspace(cwd: &str) -> crate::WorkspaceRoots {
    crate::WorkspaceRoots::for_tool_factory(cwd)
}

/// Built-in coding tool identifiers.
pub const TOOL_NAMES: &[&str] = &["read", "inspect_image", "generate_image", "bash", "edit", "write", "grep", "find", "glob", "ls", "browser", "web_search", "ast_grep", "ast_edit", "ask", "lsp", "github", "memory", "recall", "retain", "reflect", "mcp", "debug", "eval", "notebook"];

/// One-line prompt snippets keyed by tool name.
#[must_use]
pub fn tool_snippet(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => "Read file contents",
        "inspect_image" => "Inspect an image file's metadata and statistics without rendering it (format, dimensions, color type, brightness, dominant colors)",
        "generate_image" => "Generate images through the configured OpenAI-compatible provider (images/generations): saves PNG/JPEG/WebP/GIF/BMP to the workspace (default <cwd>/images), returns paths, dimensions, and a bounded prompt echo; prompts capped at 4096 characters, n 1-4, sizes 256x256/512x512/1024x1024",
        "bash" => "Execute finite foreground shell commands, or supervised long-running commands with background=true",
        "edit" => "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        "write" => "Create or overwrite files",
        "grep" => "Search file contents for patterns (respects .gitignore)",
        "find" => "Find files by glob pattern (respects .gitignore)",
        "glob" => "Match files by glob pattern under workspace roots (sandboxed, bounded)",
        "ls" => "List directory contents",
        "browser" => "Automate a headless Chrome/Chromium browser: navigate, click, fill, screenshot, extract, list_tabs, close",
        "web_search" => "Search the web via the DuckDuckGo Instant Answer API (disabled while PI_OFFLINE is set)",
        "ast_grep" => "Search code structurally with ast-grep patterns ($-metavariables, tree-sitter)",
        "ast_edit" => "Rewrite a single file structurally with ast-grep pattern→rewrite (metavariables substituted)",
        "ask" => "Ask the user a question and use their answer (interactive sessions only)",
        "lsp" => "Query a language server: hover, definition, references, diagnostics, symbols, rename, code actions (spawns a per-language server per call)",
        "github" => "Query the GitHub API via the gh CLI (GH_TOKEN/reqwest fallback): search/get/list issues and PRs, create issues and comments, list commits, view files, search code",
        "mcp" => "Call Model Context Protocol (MCP) servers configured in settings mcpServers: list_servers, list_tools, call (stdio child processes, session-scoped)",
        "debug" => "Drive a DAP debug adapter over stdio (gdb, lldb-dap, debugpy): launch, set_breakpoint, continue_, pause, step_over/step_in/step_out, stack_trace, variables, evaluate, threads, terminate (session-scoped, one adapter at a time)",
        "eval" => "Evaluate code in a persistent session-scoped kernel: python (python3 subprocess, full stdlib) or js (embedded QuickJS); state persists across calls, output bounded at 64 KiB and redacted, errors classified (syntax/runtime/timeout)",
        "notebook" => "Read, execute, and edit Jupyter notebooks (.ipynb): read lists cells, execute runs code cells through a session-scoped Python kernel (write=true persists outputs), edit appends a markdown/code cell",
        "memory" => "Persistent per-project memory: learn, recall, list, and forget note-style entries that survive across sessions in the same repository",
        "recall" => "Recall related memories from the configured Hindsight HTTP API",
        "retain" => "Record a memory through the configured Hindsight HTTP API",
        "reflect" => "Reflect on stored memories through the configured Hindsight HTTP API",
        "process" => "Start and control supervised long-running processes listed by /ps",
        "todo" => "Track phased session work and completion state",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Schema helpers (pi's TypeBox `Object`/`Prop`/`Opt`/`String`/... analogues)
// ---------------------------------------------------------------------------

pub(crate) fn s_string(desc: &str) -> Schema {
    Schema {
        schema_type: Some(Value::String("string".into())),
        description: Some(desc.to_string()),
        ..Default::default()
    }
}
pub(crate) fn s_number(desc: &str) -> Schema {
    Schema {
        schema_type: Some(Value::String("number".into())),
        description: Some(desc.to_string()),
        ..Default::default()
    }
}
pub(crate) fn s_boolean(desc: &str) -> Schema {
    Schema {
        schema_type: Some(Value::String("boolean".into())),
        description: Some(desc.to_string()),
        ..Default::default()
    }
}
pub(crate) fn s_array(item: Schema, desc: &str) -> Schema {
    Schema {
        schema_type: Some(Value::String("array".into())),
        items: Some(Box::new(item)),
        description: Some(desc.to_string()),
        ..Default::default()
    }
}
fn nullable(mut schema: Schema) -> Schema {
    schema.nullable = true;
    schema
}

fn fill_missing_with_null(mut arguments: Value) -> Result<Value> {
    let object = arguments.as_object_mut().ok_or_else(|| anyhow!("tool arguments must be an object"))?;
    for key in ["list", "task", "phase", "items", "dependsOn"] {
        object.entry(key).or_insert(Value::Null);
    }
    if object.get("cascade").is_none_or(Value::is_null) {
        object.insert("cascade".to_owned(), Value::Bool(false));
    }
    Ok(arguments)
}
/// Builds an object schema from ordered `(name, schema)` pairs. Names in
/// `required` are required (the rest are optional, like pi's `Opt`).
pub(crate) fn s_object(props: Vec<(&str, Schema)>, required: Vec<&str>) -> Schema {
    let properties = props
        .into_iter()
        .map(|(n, s)| (n.to_string(), s))
        .collect::<HashMap<_, _>>();
    let required = required.into_iter().map(String::from).collect();
    Schema {
        schema_type: Some(Value::String("object".into())),
        properties,
        required,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Argument helpers (operate on the decoded `serde_json::Value` object)
// ---------------------------------------------------------------------------

pub(crate) fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default()
}

/// Reads a required string argument, distinguishing a missing key from an
/// explicitly empty value. A missing required field fails actionably (the
/// tool schema declares the key required); an explicitly empty value passes
/// through unchanged so schema-valid empty payloads (e.g. `write` with
/// `content: ""`) keep working.
pub(crate) fn required_arg_str(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .ok_or_else(|| anyhow!("missing required argument {key:?}"))?;
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("invalid {key}: must be a string"))?;
    Ok(text.to_owned())
}
pub(crate) fn arg_int(args: &Value, key: &str) -> Result<Option<i64>> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let number = value
        .as_f64()
        .ok_or_else(|| anyhow!("Invalid {key}: must be a finite safe number"))?;
    if !number.is_finite() || number.abs() > JS_MAX_SAFE_INTEGER {
        return Err(anyhow!("Invalid {key}: must be a finite safe number"));
    }
    Ok(Some(number.trunc() as i64))
}
fn arg_float(args: &Value, key: &str) -> Option<f64> {
    args.get(key)
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|x| x as f64)))
}
pub(crate) fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub(crate) fn text_result(text: impl Into<String>) -> AgentToolResult {
    AgentToolResult::text(text)
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

/// Builds a single built-in tool by name, rooted at `cwd`. The bash tool built
/// this way exposes no PI_* session metadata (matches pi's standalone guard).
pub fn create_tool(name: &str, cwd: &str) -> Result<AgentTool> {
    create_tool_with_session_env(name, cwd, None)
}

/// `create_tool` with the session-metadata provider the coding session threads
/// into the bash tool (`None` for standalone construction).
pub fn create_tool_with_session_env(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
) -> Result<AgentTool> {
    create_tool_with_session_env_and_rules(name, cwd, session_env, None)
}

/// [`create_tool_with_session_env`] with the session's live path permission
/// rules (`Some(source)`) threaded into the `lsp` tool so its rename preflight
/// obeys the current rules at execution time. `None` keeps the standalone
/// behavior (empty rules).
pub fn create_tool_with_session_env_and_rules(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Result<AgentTool> {
    match name {
        "read" => Ok(read_tool(cwd)),
        "inspect_image" => Ok(inspect_image_tool(cwd)),
        "generate_image" => Ok(generate_image_tool(cwd, None)),
        "bash" => Ok(bash_tool(cwd, session_env, None, None)),
        "edit" => Ok(edit_tool(cwd)),
        "write" => Ok(write_tool(cwd)),
        "grep" => Ok(grep_tool(cwd)),
        "find" => Ok(find_tool(cwd)),
        "glob" => Ok(glob_tool(cwd)),
        "ls" => Ok(ls_tool(cwd)),
        "browser" => Ok(browser_tool_for_workspace(factory_workspace(cwd))),
        "web_search" => Ok(web_search_tool()),
        "ast_grep" => Ok(ast_grep_tool(cwd)),
        "ast_edit" => Ok(ast_edit_tool(cwd)),
        "ask" => Ok(ask::standalone_ask_tool()),
        "lsp" => Ok(match rules {
            Some(rules) => lsp_tool_with_rules(cwd, rules),
            None => lsp_tool(cwd),
        }),
        "github" => Ok(github_tool()),
        "memory" => Ok(memory_tool_with_session_env(cwd, session_env)),
        "recall" => Ok(recall_tool(cwd, MemoryConfig::default())),
        "retain" => Ok(retain_tool(cwd, MemoryConfig::default())),
        "reflect" => Ok(reflect_tool(cwd, MemoryConfig::default())),
        "mcp" => Ok(mcp_tool(McpRegistry::new())),
        "debug" => Ok(debug_tool(cwd)),
        "eval" => Ok(eval_tool(cwd)),
        "notebook" => Ok(notebook_tool(cwd)),
        "todo" => Ok(todo_tool(TodoRuntime::memory())),
        _ => Err(anyhow!("Unknown tool name: {name}")),
    }
}

/// Builds a built-in tool with a live trusted-skill snapshot for `skill://` reads.
pub fn create_tool_with_context(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
) -> Result<AgentTool> {
    match name {
        "read" => Ok(read_tool_with_skills(cwd, skills)),
        _ => create_tool_with_session_env(name, cwd, session_env),
    }
}

/// Like [`create_tool_with_context`] but also threads an internal-URI resolver
/// into the `read` tool for `agent://`, `history://`, and `artifact://` URIs,
/// and a sandbox resolver into the `bash` tool.
pub fn create_tool_with_context_and_resolver(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    sandbox: Option<SandboxConfigFn>,
    resolver: Option<InternalUriResolverFn>,
) -> Result<AgentTool> {
    create_tool_with_context_and_resolver_and_rules(
        name,
        cwd,
        session_env,
        skills,
        sandbox,
        resolver,
        None,
    )
}

/// [`create_tool_with_context_and_resolver`] with the session's live path
/// permission rules threaded into the `lsp` tool (see
/// [`create_tool_with_session_env_and_rules`]).
pub fn create_tool_with_context_and_resolver_and_rules(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    sandbox: Option<SandboxConfigFn>,
    resolver: Option<InternalUriResolverFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Result<AgentTool> {
    match name {
        "read" => Ok(read_tool_with_resolver(cwd, skills, resolver)),
        "bash" => Ok(bash_tool(cwd, session_env, None, sandbox)),
        _ => create_tool_with_session_env_and_rules(name, cwd, session_env, rules),
    }
}

/// Persona-aware child-tool factory for non-memory built-ins. Orchestration
/// resolves any requested memory-family name as the selected family once.
pub(crate) fn create_tool_with_context_and_resolver_and_rules_for_persona(
    name: &str,
    cwd: &str,
    _persona_root: &Path,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    sandbox: Option<SandboxConfigFn>,
    resolver: Option<InternalUriResolverFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Result<AgentTool> {
    create_tool_with_context_and_resolver_and_rules(
        name, cwd, session_env, skills, sandbox, resolver, rules,
    )
}

/// Default child coding tools with memory selected from the parent's live
/// configuration and local storage redirected to the durable persona root.
pub(crate) fn create_coding_tools_with_context_and_resolver_for_persona(
    cwd: &str,
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    sandbox: Option<SandboxConfigFn>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let mut tools = vec![
        read_tool_with_resolver(cwd, skills, resolver),
        bash_tool(cwd, session_env.clone(), process, sandbox),
        browser_tool_for_workspace(factory_workspace(cwd)),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        generate_image_tool(cwd, image_gen),
    ];
    tools.extend(memory_tools_for_persona(
        cwd,
        persona_root,
        session_env,
        memory.as_ref().and_then(|resolver| resolver()),
    ));
    tools
}

/// Workspace-aware variant used by sessions with explicit additional roots.
/// `sandbox` supplies live `settings.sandbox` resolution for the bash tool;
/// `None` for standalone construction (per-call `sandboxed` still works with
/// default allowed paths). `image_gen` supplies the `generate_image` tool's
/// model/endpoint/credential resolution; `None` falls back to the turn's
/// model and env credentials.
pub fn create_coding_tools_for_workspace_with_context_and_resolver(
    workspace: crate::WorkspaceRoots,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    sandbox: Option<SandboxConfigFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let cwd = workspace.cwd().to_string_lossy().into_owned();
    let mut tools = vec![
        read_tool_for_workspace(workspace.clone(), skills, resolver),
        bash_tool(&cwd, session_env.clone(), process, sandbox),
        browser_tool_for_workspace(workspace.clone()),
        edit_tool_for_workspace(workspace.clone()),
        write_tool_for_workspace(workspace.clone()),
        ast_edit_tool(&cwd),
        generate_image_tool_for_workspace(workspace.clone(), image_gen),
    ];
    // Memory tools follow the live settings.memory.backend: off → none,
    // local → `memory`, hindsight → `recall`/`retain`/`reflect`. A resolver
    // that is not yet attached (no settings) falls back to `local`.
    tools.extend(memory_tools_for(
        &cwd,
        session_env.clone(),
        memory.as_ref().and_then(|resolver| resolver()),
    ));
    tools
}

/// Default coding tools with live session metadata and safe `skill://` resolution.
pub fn create_coding_tools_with_context(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
) -> Vec<AgentTool> {
    vec![
        read_tool_with_skills(cwd, skills),
        bash_tool(cwd, session_env.clone(), process, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        memory_tool_with_session_env(cwd, session_env),
    ]
}

/// Like [`create_coding_tools_with_context`] but the `read` tool also resolves
/// internal URIs, bash receives the sandbox resolver, and memory follows the
/// parent session's live backend selection.
pub fn create_coding_tools_with_context_and_resolver(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    sandbox: Option<SandboxConfigFn>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let mut tools = vec![
        read_tool_with_resolver(cwd, skills, resolver),
        bash_tool(cwd, session_env.clone(), process, sandbox),
        browser_tool_for_workspace(factory_workspace(cwd)),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        generate_image_tool(cwd, image_gen),
    ];
    tools.extend(memory_tools_for(
        cwd,
        session_env,
        memory.as_ref().and_then(|resolver| resolver()),
    ));
    tools
}

/// The default coding tool set `[read, bash, edit, write, ast_edit, memory]`.
pub fn create_coding_tools(cwd: &str) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        bash_tool(cwd, None, None, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        memory_tool(cwd),
    ]
}
/// The default coding tool set with live session metadata for bash.
pub fn create_coding_tools_with_session_env(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        bash_tool(cwd, session_env.clone(), None, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        memory_tool_with_session_env(cwd, session_env),
    ]
}

/// Read-only built-ins in upstream order: `[read, grep, find, glob, ls, web_search, ast_grep]`.
pub fn create_read_only_tools(cwd: &str) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
        web_search_tool(),
        ast_grep_tool(cwd),
    ]
}

/// Serializable definitions for the read-only built-ins.
pub fn create_read_only_tool_definitions(cwd: &str) -> Vec<pi_ai::ToolDefinition> {
    create_read_only_tools(cwd)
        .into_iter()
        .map(|tool| tool.as_tool_definition())
        .collect()
}

/// Serializable definitions for the default coding tool set.
pub fn create_coding_tool_definitions(cwd: &str) -> Vec<pi_ai::ToolDefinition> {
    create_coding_tools(cwd)
        .into_iter()
        .map(|tool| tool.as_tool_definition())
        .collect()
}

/// Name-keyed built-in tools without changing the existing Vec factories.
pub fn create_all_tools_by_name(cwd: &str) -> HashMap<String, AgentTool> {
    create_all_tools(cwd)
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect()
}

/// Alias using the conventional map suffix for name-keyed tool consumers.
pub fn create_all_tools_map(cwd: &str) -> HashMap<String, AgentTool> {
    create_all_tools_by_name(cwd)
}

/// A single built-in's serializable definition, keyed by the same name API.
pub fn create_tool_definition(name: &str, cwd: &str) -> Result<pi_ai::ToolDefinition> {
    Ok(create_tool(name, cwd)?.as_tool_definition())
}

/// Name-keyed serializable definitions for all built-in tools.
pub fn create_all_tool_definitions_by_name(cwd: &str) -> HashMap<String, pi_ai::ToolDefinition> {
    create_all_tools_by_name(cwd)
        .into_iter()
        .map(|(name, tool)| (name, tool.as_tool_definition()))
        .collect()
}

/// Name-keyed serializable definitions, matching upstream's Record shape.
pub fn create_all_tool_definitions(cwd: &str) -> HashMap<String, pi_ai::ToolDefinition> {
    create_all_tool_definitions_by_name(cwd)
}

/// Explicit map alias retained for callers that distinguish Vec and map factories by suffix.
pub fn create_all_tool_definitions_map(cwd: &str) -> HashMap<String, pi_ai::ToolDefinition> {
    create_all_tool_definitions_by_name(cwd)
}


/// All built-in tools (including `glob`), no session metadata.
pub fn create_all_tools(cwd: &str) -> Vec<AgentTool> {
    create_all_tools_with_rules(cwd, None)
}

/// [`create_all_tools`] with the session's live path permission rules threaded
/// into the `lsp` tool (see [`create_tool_with_session_env_and_rules`]).
pub fn create_all_tools_with_rules(
    cwd: &str,
    rules: Option<crate::PermissionRulesSource>,
) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        inspect_image_tool(cwd),
        generate_image_tool(cwd, None),
        bash_tool(cwd, None, None, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
        browser_tool_for_workspace(factory_workspace(cwd)),
        web_search_tool(),
        ast_grep_tool(cwd),
        ask::standalone_ask_tool(),
        match rules {
            Some(rules) => lsp_tool_with_rules(cwd, rules),
            None => lsp_tool(cwd),
        },
        github_tool(),
        memory_tool(cwd),
        recall_tool(cwd, MemoryConfig::default()),
        retain_tool(cwd, MemoryConfig::default()),
        reflect_tool(cwd, MemoryConfig::default()),
        mcp_tool(McpRegistry::new()),
        debug_tool(cwd),
        eval_tool(cwd),
        notebook_tool(cwd),
    ]
}

/// All built-in tools with the session-metadata provider threaded into bash.
pub fn create_all_tools_with_session_env(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
) -> Vec<AgentTool> {
    create_all_tools_with_session_env_and_rules(cwd, session_env, None)
}

/// [`create_all_tools_with_session_env`] with the session's live path
/// permission rules threaded into the `lsp` tool (see
/// [`create_tool_with_session_env_and_rules`]).
pub fn create_all_tools_with_session_env_and_rules(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        inspect_image_tool(cwd),
        generate_image_tool(cwd, None),
        bash_tool(cwd, session_env.clone(), None, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
        browser_tool_for_workspace(factory_workspace(cwd)),
        web_search_tool(),
        ast_grep_tool(cwd),
        ask::standalone_ask_tool(),
        match rules {
            Some(rules) => lsp_tool_with_rules(cwd, rules),
            None => lsp_tool(cwd),
        },
        github_tool(),
        memory_tool_with_session_env(cwd, session_env),
        recall_tool(cwd, MemoryConfig::default()),
        retain_tool(cwd, MemoryConfig::default()),
        reflect_tool(cwd, MemoryConfig::default()),
        mcp_tool(McpRegistry::new()),
        debug_tool(cwd),
        eval_tool(cwd),
        notebook_tool(cwd),
    ]
}

/// All built-in tools with session metadata, skills, the bash process context,
/// and an internal-URI resolver threaded into the `read` tool for `agent://`,
/// `history://`, and `artifact://` URIs.
pub fn create_all_tools_with_context_and_resolver(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
) -> Vec<AgentTool> {
    create_all_tools_with_context_and_resolver_and_rules(
        cwd, session_env, skills, process, resolver, None,
    )
}

/// [`create_all_tools_with_context_and_resolver`] with the session's live path
/// permission rules threaded into the `lsp` tool (see
/// [`create_tool_with_session_env_and_rules`]).
pub fn create_all_tools_with_context_and_resolver_and_rules(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Vec<AgentTool> {
    vec![
        read_tool_with_resolver(cwd, skills, resolver),
        inspect_image_tool(cwd),
        generate_image_tool(cwd, None),
        bash_tool(cwd, session_env.clone(), process, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
        browser_tool_for_workspace(factory_workspace(cwd)),
        web_search_tool(),
        ast_grep_tool(cwd),
        ask::standalone_ask_tool(),
        match rules {
            Some(rules) => lsp_tool_with_rules(cwd, rules),
            None => lsp_tool(cwd),
        },
        github_tool(),
        memory_tool_with_session_env(cwd, session_env),
        recall_tool(cwd, MemoryConfig::default()),
        retain_tool(cwd, MemoryConfig::default()),
        reflect_tool(cwd, MemoryConfig::default()),
        mcp_tool(McpRegistry::new()),
        debug_tool(cwd),
        eval_tool(cwd),
        notebook_tool(cwd),
    ]
}

/// Creates the parent-owned conversational todo tool over canonical session state.
#[must_use]
pub fn create_todo_tool(runtime: TodoRuntime) -> AgentTool {
    todo_tool(runtime)
}

fn todo_tool(runtime: TodoRuntime) -> AgentTool {
    let init_phase = Schema {
        schema_type: Some(json!("object")),
        properties: HashMap::from([
            ("phase".to_owned(), s_string("phase name")),
            (
                "items".to_owned(),
                Schema {
                    min_items: Some(1),
                    ..s_array(s_string("task content"), "tasks for this phase")
                },
            ),
            (
                "agents".to_owned(),
                Schema {
                    min_items: Some(1),
                    ..s_array(s_string("agent name"), "optional explicit agent that must execute each task; parallel to items (agents[i] names the agent for items[i]); length must equal items")
                },
            ),
        ]),
        property_order: vec!["phase".to_owned(), "items".to_owned(), "agents".to_owned()],
        required: vec!["phase".to_owned(), "items".to_owned()],
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    };
    let parameters = Schema {
        schema_type: Some(json!("object")),
        properties: HashMap::from([
            (
                "op".to_owned(),
                Schema {
                    schema_type: Some(json!("string")),
                    enum_values: ["init", "start", "done", "drop", "rm", "append", "add_dependency", "remove_dependency", "update_dependencies", "view"]
                        .into_iter()
                        .map(|value| json!(value))
                        .collect(),
                    description: Some("operation to apply".to_owned()),
                    ..Schema::default()
                },
            ),
            ("list".to_owned(), nullable(s_array(init_phase, "phased task list (init)"))),
            ("task".to_owned(), nullable(s_string("stable task ID (preferred), or exact task content for start/done/drop/rm compatibility"))),
            ("phase".to_owned(), nullable(s_string("phase name"))),
            (
                "items".to_owned(),
                nullable(s_array(s_string("task content"), "tasks to append or initialize")),
            ),
            ("dependsOn".to_owned(), nullable(s_array(s_string("stable dependency task ID"), "dependency task IDs for add/remove/update operations"))),
            ("cascade".to_owned(), nullable(s_boolean("For rm, explicitly remove dependency edges from surviving tasks before removing dependency targets"))),
        ]),
        property_order: vec![
            "op".to_owned(),
            "list".to_owned(),
            "task".to_owned(),
            "phase".to_owned(),
            "items".to_owned(),
            "dependsOn".to_owned(),
            "cascade".to_owned(),
        ],
        required: vec!["op", "list", "task", "phase", "items", "dependsOn", "cascade"].into_iter().map(str::to_owned).collect(),
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    };
    let mut tool = AgentTool::new(
        "todo",
        "Maintain the durable session todo DAG while preserving phased presentation. Every returned task has a stable id, dependsOn, ready, and blockedBy. Phase order is not a dependency: any ready task may proceed. Completed and dropped dependencies both satisfy dependents. The attached executor runs every ready task concurrently up to its configured limit, so keep independent work in separate tasks without dependsOn edges — reserve dependencies for genuine data or control ordering (a task consumes another's output, or must not start before it); never chain unrelated steps. Use init/append, start/done/drop/rm, add_dependency/remove_dependency/update_dependencies, or read-only view. Dependency operations require stable task IDs. rm rejects dependency targets unless cascade=true explicitly removes the surviving dependency edges. init phase entries may carry an optional agents array parallel to items to bind each task to a specific agent that must execute it. This tool changes canonical readiness/status; an attached parent application may orchestrate ready items. Use concise task titles (≤60 chars): the task content is the title the panel shows — longer titles are truncated.",
        parameters,
        move |context| {
            let runtime = runtime.clone();
            async move {
                match deserialize_todo_op(context.arguments) {
                    Ok(op) => {
                        let op_name = op.name();
                        match runtime.apply(op) {
                            Ok(result) => {
                                let state = runtime.state();
                                let mut details = serde_json::to_value(TodoToolDetails {
                                    phases: result.phases,
                                    storage: state.storage,
                                    completed_tasks: result.completed_tasks,
                                })?;
                                details["op"] = Value::String(op_name.to_owned());
                                Ok(AgentToolResult {
                                    content: vec![ContentBlock::text(result.summary)],
                                    details,
                                    ..AgentToolResult::default()
                                })
                            }
                            Err(error) => {
                                runtime.schedule_reminder();
                                let (summary, details) = tool_failure_result(&runtime, &error);
                                let mut details = serde_json::to_value(details)?;
                                details["op"] = Value::String(op_name.to_owned());
                                details[TODO_ERROR_MARKER] = Value::Bool(true);
                                Ok(AgentToolResult {
                                    content: vec![ContentBlock::text(summary)],
                                    details,
                                    ..AgentToolResult::default()
                                })
                            }
                        }
                    }
                    Err(error) => {
                        runtime.schedule_reminder();
                        let (summary, details) = tool_failure_result(&runtime, &error);
                        let mut details = serde_json::to_value(details)?;
                        details[TODO_ERROR_MARKER] = Value::Bool(true);
                        Ok(AgentToolResult {
                            content: vec![ContentBlock::text(summary)],
                            details,
                            ..AgentToolResult::default()
                        })
                    }
                }
            }
        },
    )
    .with_capability(ToolCapability::Write)
    .with_label("Todo")
    .with_execution_mode(ToolExecutionMode::Sequential)
    .with_prepare_arguments(fill_missing_with_null);
    tool.constrained_sampling = Some(ConstrainedSampling::json_schema(ConstrainedSamplingStrictness::Prefer));
    tool
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

fn read_tool(cwd: &str) -> AgentTool {
    read_tool_for_workspace(factory_workspace(cwd), None, None)
}

fn read_tool_with_skills(cwd: &str, skills: Option<SkillSnapshotFn>) -> AgentTool {
    read_tool_for_workspace(factory_workspace(cwd), skills, None)
}

/// Builds the `read` tool with an optional internal-URI resolver for
/// `agent://`, `history://`, and `artifact://` URIs (resolved to a local file
/// path) in addition to the `skill://` resolution provided by `skills`.
/// Recognized internal schemes propagate resolver errors; unrecognized
/// schemes fall through to ordinary file-path resolution. Pass `None` for
/// standalone construction (matches the existing factories).
pub fn read_tool_with_resolver(
    cwd: &str,
    skills: Option<SkillSnapshotFn>,
    resolver: Option<InternalUriResolverFn>,
) -> AgentTool {
    read_tool_for_workspace(factory_workspace(cwd), skills, resolver)
}

fn read_tool_for_workspace(
    workspace: crate::WorkspaceRoots,
    skills: Option<SkillSnapshotFn>,
    resolver: Option<InternalUriResolverFn>,
) -> AgentTool {
    let params = s_object(
        vec![
            ("path", s_string("Path to the file to read (relative or absolute)")),
            ("offset", s_number("Line number to start reading from (1-indexed)")),
            ("limit", s_number("Maximum number of lines to read")),
        ],
        vec!["path"],
    );
    let description = format!(
        "Read the contents of a file or trusted skill:// URI. Supports text files, images (jpg, png, gif, webp, bmp), workspace-scoped videos (mp4, webm, ogg), PDFs (text layer extracted via pdftotext; requires poppler-utils), Office documents (docx, xlsx, pptx, odt, ods, odp, rtf; extracted via pandoc or LibreOffice), EPUB ebooks (via pandoc), and Jupyter notebooks (ipynb; via jupyter nbconvert). Images are sent as attachments; accepted videos are returned as bounded media details for authenticated Web preview. For text, PDF, and converted document files, output is truncated to {} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("read", description, params, move |ctx| {
        let workspace = workspace.clone();
        let skills = skills.clone();
        let resolver = resolver.clone();
        async move {
            run_read(
                &workspace,
                ctx.arguments,
                skills.as_ref(),
                resolver.as_ref(),
                ctx.model.as_ref(),
                ctx.abort,
            )
            .await
        }
    })
    .with_capability(ToolCapability::Read)
    .with_prompt_guidelines(vec!["Use read to examine files instead of cat or sed.".to_string()])
}

fn internal_uri_scheme(path: &str) -> Option<&'static str> {
    if path.starts_with("agent://") {
        Some("agent")
    } else if path.starts_with("history://") {
        Some("history")
    } else if path.starts_with("artifact://") {
        Some("artifact")
    } else {
        None
    }
}

fn video_mime(path: &Path, prefix: &[u8]) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "mp4" if prefix.get(4..8) == Some(b"ftyp") => Some("video/mp4"),
        "webm" if prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) => Some("video/webm"),
        "ogg"
            if prefix.starts_with(b"OggS")
                && prefix.windows(7).any(|window| window == b"\x80theora") =>
        {
            Some("video/ogg")
        }
        _ => None,
    }
}

fn scoped_media_path(path: &str, workspace: &crate::WorkspaceRoots) -> Result<PathBuf> {
    let scoped = resolve_scoped_path(path, workspace)?;
    let canonical = std::fs::canonicalize(&scoped)
        .with_context(|| format!("Could not resolve media path {path}"))?;
    if !workspace.roots().iter().any(|root| canonical.starts_with(root)) {
        return Err(anyhow!("Path escapes workspace roots: {path}"));
    }
    Ok(canonical)
}

fn inline_video_result(path: &str, workspace: &crate::WorkspaceRoots) -> Result<Option<AgentToolResult>> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    if !matches!(extension.as_deref(), Some("mp4" | "webm" | "ogg")) {
        return Ok(None);
    }
    let canonical = scoped_media_path(path, workspace)?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| anyhow!("{error}"))?;
    if metadata.len() > MAX_INLINE_VIDEO_BYTES {
        return Err(anyhow!(
            "video exceeds the {} MiB Web preview limit",
            MAX_INLINE_VIDEO_BYTES / 1024 / 1024
        ));
    }
    let mut prefix = [0_u8; 64];
    let prefix_len = std::fs::File::open(&canonical)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut prefix))
        .map_err(|error| anyhow!("{error}"))?;
    let Some(mime_type) = video_mime(&canonical, &prefix[..prefix_len]) else {
        return Err(anyhow!("video extension does not match supported media content"));
    };
    let bytes = std::fs::read(&canonical).map_err(|error| anyhow!("{error}"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    if data.len() > MAX_INLINE_VIDEO_BASE64_BYTES {
        return Err(anyhow!("video exceeds the inline Web transport limit"));
    }
    let name = canonical
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video")
        .to_owned();
    Ok(Some(AgentToolResult {
        content: vec![ContentBlock::text(format!(
            "Read video file [{mime_type}] {name} ({})",
            format_size(metadata.len() as usize)
        ))],
        details: json!({
            "media": [{
                "kind": "video",
                "mimeType": mime_type,
                "data": data,
                "name": name,
                "sizeBytes": metadata.len(),
            }]
        }),
        ..Default::default()
    }))
}

async fn run_read(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    skills: Option<&SkillSnapshotFn>,
    resolver: Option<&InternalUriResolverFn>,
    model: Option<&pi_ai::Model>,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = arg_str(&args, "path");
    let abs = if path.starts_with("skill://") {
        let skills = skills
            .ok_or_else(|| anyhow!("skill:// resolution is unavailable for this read tool"))?();
        crate::resolve_skill_uri(&path, &skills)?
            .to_string_lossy()
            .into_owned()
    } else if let Some(scheme) = internal_uri_scheme(&path) {
        let resolver = resolver
            .ok_or_else(|| anyhow!("{scheme}:// resolution is unavailable for this read tool"))?;
        resolver(&path)?.to_string_lossy().into_owned()
    } else {
        resolve_read_path(&path, workspace)?
    };
    check_aborted(&abort)?;
    let info = std::fs::metadata(&abs).map_err(|e| anyhow!("{}", e))?;
    check_aborted(&abort)?;
    if info.is_dir() {
        return Err(anyhow!("EISDIR: illegal operation on a directory, read"));
    }
    if !path.starts_with("skill://") && internal_uri_scheme(&path).is_none() {
        if let Some(result) = inline_video_result(&path, workspace)? {
            return Ok(result);
        }
    }
    // Local filesystem images must stay inside workspace roots. Trusted URI
    // resolvers already enforce their own containment and may resolve outside it.
    if let Some(mime) = detect_supported_image_mime_type_from_file(Path::new(&abs)) {
        let image_path = if path.starts_with("skill://") || internal_uri_scheme(&path).is_some() {
            PathBuf::from(&abs)
        } else {
            scoped_media_path(&path, workspace)?
        };
        check_aborted(&abort)?;
        let data = std::fs::read(&image_path).map_err(|e| anyhow!("{}", e))?;
        check_aborted(&abort)?;
        let processed = process_image(&data, &mime, true);
        check_aborted(&abort)?;
        let non_vision_note = model
            .is_some_and(|model| !model.input.iter().any(|input| input == "image"))
            .then_some(NON_VISION_IMAGE_NOTE);
        if !processed.ok {
            let mut note = format!("Read image file [{mime}]\n{}", processed.message);
            if let Some(non_vision_note) = non_vision_note {
                note.push('\n');
                note.push_str(non_vision_note);
            }
            return Ok(AgentToolResult {
                content: vec![ContentBlock::text(note)],
                ..Default::default()
            });
        }
        let mut note = format!("Read image file [{}]", processed.mime_type);
        if !processed.hints.is_empty() {
            note.push('\n');
            note.push_str(&processed.hints.join("\n"));
        }
        if let Some(non_vision_note) = non_vision_note {
            note.push('\n');
            note.push_str(non_vision_note);
        }
        let img = ContentBlock::Image {
            data: base64::engine::general_purpose::STANDARD.encode(&processed.data),
            mime_type: processed.mime_type,
        };
        return Ok(AgentToolResult {
            content: vec![ContentBlock::text(note), img],
            ..Default::default()
        });
    }

    // PDF path: extract the text layer with pdftotext (poppler-utils), then
    // apply the same offset/limit/truncate selection as the text path.
    if is_pdf(Path::new(&abs)) {
        check_aborted(&abort)?;
        let text = extract_pdf_text(&abs, abort).await?;
        // The hint is spliced into a `sed -n 'Np' <(...) | head -c N` bash
        // escape hatch as a process substitution, so the raw path must be
        // shell-quoted (single quotes, embedded quotes escaped).
        let quoted_abs = format!("'{}'", abs.replace('\'', "'\\''"));
        let first_line_hint = format!("<(pdftotext -layout -nopgbrk {} -)", quoted_abs);
        return render_read_result(&first_line_hint, &text, &args);
    }

    // Office/notebook path: extract the text layer with an external converter
    // (pandoc, LibreOffice, or jupyter nbconvert), then apply the same
    // offset/limit/truncate selection as the text path. The converter is
    // reported back so the `sed -n` escape hatch can re-run the same command.
    if is_doc(Path::new(&abs)) {
        check_aborted(&abort)?;
        let extracted = extract_doc_text(&abs, abort).await?;
        let quoted_abs = format!("'{}'", abs.replace('\'', "'\\''"));
        let first_line_hint = extracted.converter.sed_hint(&quoted_abs);
        return render_read_result(&first_line_hint, &extracted.text, &args);
    }

    // Text path.
    let data = std::fs::read(&abs).map_err(|e| anyhow!("{}", e))?;
    check_aborted(&abort)?;
    let text = String::from_utf8_lossy(&data);
    render_read_result(&path, &text, &args)
}

/// Applies the read tool's offset/limit/truncate selection to `text` and
/// renders the result, honoring the `[Showing lines ...]` / `[N more lines in
/// file. Use offset=...]` contracts. Shared by the text and PDF branches.
/// `first_line_hint` names what the `sed -n` escape hatch should operate on
/// when a single line exceeds the byte limit: the original path for text, a
/// `<(pdftotext ... -)` process substitution for PDFs.
fn render_read_result(first_line_hint: &str, text: &str, args: &Value) -> Result<AgentToolResult> {
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_file_lines = all_lines.len();
    let (offset, has_offset) = match arg_int(args, "offset")? {
        Some(o) => (o, true),
        None => (0, false),
    };
    let mut start_line = 0usize;
    if has_offset && offset > 0 {
        start_line = (offset - 1) as usize;
    }
    let start_line_display = start_line + 1;
    if start_line >= all_lines.len() {
        return Err(anyhow!(
            "Offset {} is beyond end of file ({} lines total)",
            offset,
            all_lines.len()
        ));
    }

    let mut has_limit = false;
    let mut user_limited_lines = 0i64;
    let selected: String;
    if let Some(limit) = arg_int(args, "limit")? {
        has_limit = true;
        // endLine = min(startLine + limit, allLines.length); JS slice semantics for
        // negative ends; an end before start yields an empty slice.
        let mut end_line = start_line as i64 + limit;
        if end_line > all_lines.len() as i64 {
            end_line = all_lines.len() as i64;
        }
        let mut eff_end = end_line;
        if eff_end < 0 {
            eff_end = all_lines.len() as i64 + eff_end;
            if eff_end < 0 {
                eff_end = 0;
            }
        }
        if eff_end < start_line as i64 {
            eff_end = start_line as i64;
        }
        let eff_end = eff_end.max(0) as usize;
        selected = all_lines[start_line..eff_end].join("\n");
        user_limited_lines = end_line - start_line as i64;
    } else {
        selected = all_lines[start_line..].join("\n");
    }

    let tr = truncate_head(&selected, 0, 0);
    let out = if tr.first_line_exceeds_limit {
        let first_line_size = format_size(all_lines[start_line].len());
        format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {first_line_hint} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES)
        )
    } else if tr.truncated {
        let end_line_display = start_line_display + tr.output_lines - 1;
        let next_offset = end_line_display + 1;
        if tr.truncated_by == "lines" {
            format!(
                "{}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]",
                tr.content
            )
        } else {
            format!(
                "{}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                tr.content,
                format_size(DEFAULT_MAX_BYTES)
            )
        }
    } else if has_limit && (start_line as i64 + user_limited_lines) < all_lines.len() as i64 {
        let remaining = all_lines.len() as i64 - (start_line as i64 + user_limited_lines);
        let next_offset = start_line as i64 + user_limited_lines + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
            tr.content
        )
    } else {
        tr.content.clone()
    };

    let mut res = text_result(out);
    if tr.truncated {
        res.details = json!({ "truncation": tr });
    }
    Ok(res)
}

/// pdftotext (poppler-utils) deadline for extracting a PDF's text layer.
const PDF_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on the bytes read from `pdftotext` stdout. Bounds per-extraction
/// memory so a runaway or malicious PDF cannot exhaust the heap; extraction
/// stops at the bound and the result carries a cap notice (mirrors the bash
/// full-output disk cap contract in `tools/bash.rs`).
const PDF_EXTRACT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Copies from `reader` into `out`, stopping once `out` reaches `max_bytes`.
/// Returns `true` if the cap was hit (more input remained), `false` on EOF.
/// The caller is responsible for closing/terminating the reader's producer so
/// it does not block on a full pipe. Shared with the document-converter
/// extraction in `tools/doc_convert.rs`.
pub(crate) async fn copy_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<bool> {
    let mut buf = [0u8; 8192];
    loop {
        if out.len() >= max_bytes {
            return Ok(true);
        }
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(false);
        }
        let room = max_bytes - out.len();
        if n > room {
            out.extend_from_slice(&buf[..room]);
            return Ok(true);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Prepends the cap notice to extracted text when the extraction bound was
/// hit. Prefixed (not appended) because `render_read_result` truncates from
/// the head, so the notice must lead to stay visible.
fn apply_pdf_cap_notice(mut text: String, capped: bool) -> String {
    if capped {
        text.insert_str(
            0,
            &format!("[PDF text output capped at {}]\n", format_size(PDF_EXTRACT_MAX_BYTES)),
        );
    }
    text
}

/// Runs `pdftotext -layout -nopgbrk <abs> -` and returns the extracted text.
/// Errors are actionable: a missing binary (poppler-utils not installed), a
/// nonzero exit (encrypted/corrupted PDF), a timeout, or cancellation. The
/// process runs in its own group with `kill_on_drop` so a timeout or abort
/// reaps the whole tree (same pattern as `run_bash_core`).
async fn extract_pdf_text(abs: &str, abort: AbortSignal) -> Result<String> {
    extract_pdf_text_with("pdftotext", abs, abort).await
}

/// `extract_pdf_text` with an injectable binary name so tests can exercise
/// the missing-binary rejection deterministically (a name that never
/// resolves on PATH).
async fn extract_pdf_text_with(bin: &str, abs: &str, abort: AbortSignal) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(["-layout", "-nopgbrk", abs, "-"]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group: a timeout/abort kill reaps pdftotext and any
    // descendants sharing the output pipes.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    let mut child = match spawn_with_etxtbsy_retry(&mut cmd, "pdftotext").await {
        Ok(child) => child,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(anyhow!(
                    "PDF extraction requires poppler-utils (pdftotext); install it or use bash: pdftotext -layout -nopgbrk {} -",
                    abs
                ));
            }
            return Err(anyhow!("failed to start pdftotext: {}", e));
        }
    };

    // Collect stdout/stderr on separate tasks so `child` stays available for
    // the timeout/abort kill (same structure as `run_bash_core`). stdout is
    // hard-capped at `PDF_EXTRACT_MAX_BYTES` so a runaway PDF cannot exhaust
    // memory; the (out, capped) pair lets the caller attach the cap notice.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let capped = copy_capped(&mut stdout, &mut out, PDF_EXTRACT_MAX_BYTES)
            .await
            .unwrap_or(false);
        (out, capped)
    });
    let err_task = tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = tokio::io::copy(&mut stderr, &mut err).await;
        err
    });

    // Timeout, abort, and the stdout cap win over extraction; each kills the
    // process group so the piped output closes and the readers finish. The
    // wait future is scoped so its `&mut child` borrow ends before any kill
    // below (same structure as `run_bash_core`). `biased` with the abort
    // branch first makes an already-cancelled signal deterministically win
    // over a fast child exit (the abort test aborts before extraction). A
    // single deadline bounds both this select and the post-EOF reap below,
    // so a child that closes stdout then hangs cannot exceed the budget.
    enum ExtractionOutcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Aborted,
        // The stdout reader finished inside the select; its JoinHandle output
        // is carried here so the handle is never polled twice (polling a
        // completed JoinHandle panics). The carried `capped` flag distinguishes
        // the byte cap (we stopped draining; the child must be killed so it
        // does not block on a full pipe) from EOF (the child closed stdout; do
        // NOT kill — it is already exiting, and killing would race a clean
        // exit into a "terminated by signal" status and discard the real exit
        // status that distinguishes a genuine failure from an empty PDF).
        Output(Result<(Vec<u8>, bool), tokio::task::JoinError>),
    }
    let deadline = tokio::time::Instant::now() + PDF_EXTRACT_TIMEOUT;
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        let abort_fut = abort.cancelled();
        tokio::pin!(abort_fut);
        tokio::select! {
            biased;
            _ = &mut abort_fut => ExtractionOutcome::Aborted,
            _ = &mut sleep => ExtractionOutcome::TimedOut,
            res = &mut wait => ExtractionOutcome::Exited(res),
            joined = &mut out_task => ExtractionOutcome::Output(joined),
        }
    };

    // Kill the process group only for the cases that need it: timeout, abort,
    // a capped reader (we stopped draining stdout, so leaving the child
    // running would block it on a full pipe until the timeout), and a panicked
    // output task. EOF does NOT kill: the child closed stdout and is already
    // exiting — killing it would race a clean exit into a signal death and
    // discard the status that distinguishes a genuine pdftotext failure from
    // an empty (scanned/encrypted) PDF. Exited already reaped the child.
    match &outcome {
        ExtractionOutcome::TimedOut | ExtractionOutcome::Aborted => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        ExtractionOutcome::Output(Ok((_, capped))) => {
            if *capped {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        ExtractionOutcome::Output(Err(_)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        ExtractionOutcome::Exited(_) => {}
    }
    let stderr = err_task.await.map_err(|e| anyhow!("pdftotext error task failed: {e}"))?;
    let (stdout, capped, wait_status) = match outcome {
        // The Exited branch won the biased select before out_task was polled,
        // so awaiting the handle here is safe (it has not completed).
        ExtractionOutcome::Exited(Ok(status)) => {
            let (stdout, capped) = out_task
                .await
                .map_err(|e| anyhow!("pdftotext output task failed: {e}"))?;
            (stdout, capped, Some(status))
        }
        ExtractionOutcome::Exited(Err(e)) => {
            return Err(anyhow!("failed to wait for pdftotext: {e}"));
        }
        // The Output branch already consumed the handle output in the select;
        // reuse it instead of polling the handle again. `capped` selects
        // between the cap kill (status reflects our intentional kill, so
        // nothing to check) and EOF (reap the real status so a genuine non-zero
        // pdftotext exit stays actionable instead of being masked as "no
        // extractable text"). The EOF reap still honors the remaining timeout
        // budget and abort; we do NOT kill, so a clean exit can never be raced
        // into a signal death.
        ExtractionOutcome::Output(joined) => {
            let (stdout, capped) = joined
                .map_err(|e| anyhow!("pdftotext output task failed: {e}"))?;
            if capped {
                (stdout, capped, None)
            } else {
                // EOF: reap the real exit status (still honoring the remaining
                // timeout budget and abort) without killing. A scoped enum
                // lets the `wait` borrow end before any kill below, matching the
                // outer-select structure; killing would race a clean exit into
                // a signal death and discard the status that distinguishes a
                // genuine pdftotext failure from an empty (scanned/encrypted)
                // PDF.
                enum EofReap {
                    Status(std::process::ExitStatus),
                    TimedOut,
                    Aborted,
                }
                let reap = {
                    let wait = child.wait();
                    tokio::pin!(wait);
                    let sleep = tokio::time::sleep_until(deadline);
                    tokio::pin!(sleep);
                    let abort_fut = abort.cancelled();
                    tokio::pin!(abort_fut);
                    tokio::select! {
                        biased;
                        _ = &mut abort_fut => EofReap::Aborted,
                        _ = &mut sleep => EofReap::TimedOut,
                        res = &mut wait => match res {
                            Ok(s) => EofReap::Status(s),
                            Err(e) => return Err(anyhow!("failed to wait for pdftotext: {e}")),
                        },
                    }
                };
                match reap {
                    EofReap::Status(status) => (stdout, capped, Some(status)),
                    EofReap::TimedOut => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(anyhow!(
                            "PDF text extraction timed out after {}s (PDF may be corrupted or encrypted). Use bash: pdftotext -layout -nopgbrk {} -",
                            PDF_EXTRACT_TIMEOUT.as_secs(),
                            abs
                        ));
                    }
                    EofReap::Aborted => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(anyhow!("PDF text extraction cancelled"));
                    }
                }
            }
        }
        ExtractionOutcome::TimedOut => {
            return Err(anyhow!(
                "PDF text extraction timed out after {}s (PDF may be corrupted or encrypted). Use bash: pdftotext -layout -nopgbrk {} -",
                PDF_EXTRACT_TIMEOUT.as_secs(),
                abs
            ));
        }
        ExtractionOutcome::Aborted => {
            return Err(anyhow!("PDF text extraction cancelled"));
        }
    };

    if let Some(wait_status) = wait_status {
        if !wait_status.success() {
            let status_desc = match wait_status.code() {
                Some(code) => format!("exit status {code}"),
                None => "terminated by signal".to_string(),
            };
            let detail = String::from_utf8_lossy(&stderr).trim().to_string();
            return Err(anyhow!(
                "pdftotext failed ({status_desc}): {detail}. Use bash: pdftotext -layout -nopgbrk {} - to inspect (PDF may be encrypted or corrupted).",
                abs
            ));
        }
    }
    let text = String::from_utf8_lossy(&stdout).into_owned();
    if text.trim().is_empty() {
        return Err(anyhow!("PDF contains no extractable text (scanned or encrypted)"));
    }
    Ok(apply_pdf_cap_notice(text, capped))
}

/// Spawns a command, briefly retrying on `ETXTBSY` ("Text file busy", os
/// error 26). The kernel can transiently report a just-touched executable as
/// busy on overlay/tmp filesystems under parallel load (same fix class as
/// hooks.rs `spawn_with_etxtbsy_retry`); the binary is closed for writing by
/// the time spawn runs, so a short retry resolves it. Shared with the
/// document-converter extraction in `tools/doc_convert.rs`; `program` names
/// the executable in the retry log line.
pub(crate) async fn spawn_with_etxtbsy_retry(
    command: &mut Command,
    program: &str,
) -> std::io::Result<tokio::process::Child> {
    let mut attempts = 0u32;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if retryable_etxtbsy(&error) => {
                attempts += 1;
                if attempts >= 5 {
                    return Err(error);
                }
                eprintln!("{program} hit ETXTBSY (attempt {attempts}); retrying");
                tokio::time::sleep(Duration::from_millis(10 * u64::from(attempts))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Detects `ETXTBSY` (Text file busy, os error 26) from a spawn error via the
/// raw OS error, since `ErrorKind` maps unclassified errno values to
/// `Uncategorized`.
fn retryable_etxtbsy(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

fn write_tool(cwd: &str) -> AgentTool {
    write_tool_for_workspace(factory_workspace(cwd))
}

fn write_tool_for_workspace(workspace: crate::WorkspaceRoots) -> AgentTool {
    let params = s_object(
        vec![
            ("path", s_string("Path to the file to write. Relative paths resolve from the current working directory; absolute and parent-relative paths may target ordinary filesystem locations outside workspace roots. Existing symlinks to regular files are followed.")),
            ("content", s_string("Content to write to the file")),
        ],
        vec!["path", "content"],
    );
    AgentTool::new(
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            async move { run_write(&workspace, ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec!["Use write only for new files or complete rewrites.".to_string()])
}

async fn run_write(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    let path = required_arg_str(&args, "path")?;
    let content = required_arg_str(&args, "content")?;
    let abs = resolve_mutation_path(&path, workspace)?;
    with_file_mutation_queue(&abs, || async {
        check_aborted(&abort)?;
        ensure_regular_mutation_target(&abs, &path, "write", true)?;
        if let Some(parent) = Path::new(&abs).parent() {
            std::fs::create_dir_all(parent).map_err(|e| anyhow!("{}", e))?;
        }
        check_aborted(&abort)?;
        std::fs::write(&abs, content.as_bytes()).map_err(|e| anyhow!("{}", e))?;
        check_aborted(&abort)?;
        // pi reports `content.length` — JS string length in UTF-16 code units
        // (mislabeled "bytes"); match it exactly, not the UTF-8 byte length.
        Ok(text_result(format!(
            "Successfully wrote {} bytes to {}",
            utf16_len(&content),
            path
        )))
    })
    .await
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

fn edit_tool(cwd: &str) -> AgentTool {
    edit_tool_for_workspace(factory_workspace(cwd))
}

fn edit_tool_for_workspace(workspace: crate::WorkspaceRoots) -> AgentTool {
    let edit_obj = s_object(
        vec![
            ("oldText", s_string("Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.")),
            ("newText", s_string("Replacement text for this targeted edit.")),
        ],
        vec!["oldText", "newText"],
    );
    let params = s_object(
        vec![
            ("path", s_string("Path to the file to edit. Relative paths resolve from the current working directory; absolute and parent-relative paths may target ordinary filesystem locations outside workspace roots. Existing symlinks to regular files are followed.")),
            ("edits", s_array(edit_obj, "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.")),
        ],
        vec!["path", "edits"],
    );
    AgentTool::new(
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            async move { run_edit(&workspace, ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Use edit for precise changes (edits[].oldText must match exactly)".to_string(),
        "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls".to_string(),
        "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".to_string(),
        "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.".to_string(),
    ])
    .with_prepare_arguments(prepare_edit_arguments)
}

async fn run_edit(workspace: &crate::WorkspaceRoots, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    let path = arg_str(&args, "path");
    let raw_edits = args
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Edit tool input is invalid. edits must contain at least one replacement."))?;
    if raw_edits.is_empty() {
        return Err(anyhow!(
            "Edit tool input is invalid. edits must contain at least one replacement."
        ));
    }
    let mut edits = Vec::with_capacity(raw_edits.len());
    for re in raw_edits {
        let old = re.get("oldText").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let new = re.get("newText").and_then(|v| v.as_str()).unwrap_or("").to_string();
        edits.push(EditEntry { old_text: old, new_text: new });
    }
    let abs = resolve_mutation_path(&path, workspace)?;
    // Serialize edits/writes to the same file (different files run in parallel).
    with_file_mutation_queue(&abs, || async {
        check_aborted(&abort)?;
        ensure_regular_mutation_target(&abs, &path, "edit", false)?;
        let data = std::fs::read(&abs)
            .map_err(|e| anyhow!("Could not edit file: {}. {}.", path, fs_error_code(&e)))?;
        check_aborted(&abort)?;
        let text = String::from_utf8_lossy(&data);
        // Strip a leading BOM before matching (the model won't include it).
        let (bom, raw) = strip_bom(&text);
        let ending = detect_line_ending(&raw);
        let normalized = normalize_to_lf(&raw);
        let (base, new_content) = apply_edits_to_normalized_content(&normalized, &edits, &path)?;
        check_aborted(&abort)?;
        let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
        std::fs::write(&abs, final_content.as_bytes()).map_err(|e| anyhow!("{}", e))?;
        check_aborted(&abort)?;
        let details = generate_edit_details(&path, &base, &new_content);
        let mut result = text_result(format!("Successfully replaced {} block(s) in {}.", edits.len(), path));
        result.details = json!({
            "diff": details.diff,
            "patch": details.patch,
            "firstChangedLine": details.first_changed_line,
        });
        Ok(result)
    })
    .await
}

fn ensure_regular_mutation_target(
    absolute_path: &str,
    display_path: &str,
    operation: &str,
    allow_missing: bool,
) -> Result<()> {
    match std::fs::metadata(absolute_path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(anyhow!(
            "Could not {operation} file: {display_path}. Target is not a regular file."
        )),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow!(
            "Could not {operation} file: {display_path}. {}.",
            fs_error_code(&error)
        )),
    }
}

pub(crate) fn check_aborted(abort: &AbortSignal) -> Result<()> {
    if abort.is_aborted() {
        return Err(anyhow!("Operation aborted"));
    }
    Ok(())
}

/// Renders a filesystem error like pi's `Error code: ${error.code}` (Node
/// errno codes), falling back to the raw error text.
fn fs_error_code(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "Error code: ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "Error code: EACCES".to_string(),
        std::io::ErrorKind::IsADirectory => "Error code: EISDIR".to_string(),
        _ => err.to_string(),
    }
}

/// Ports pi's `prepareEditArguments`: when a model sends `edits` as a JSON
/// string, parse it; and fold legacy top-level `oldText`/`newText` into the
/// edits[] array.
fn prepare_edit_arguments(input: Value) -> Result<Value> {
    let mut args = input;
    // Some models send edits as a JSON string instead of an array.
    if let Some(s) = args.get("edits").and_then(|v| v.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if parsed.is_array() {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("edits".to_string(), parsed);
                }
            }
        }
    }
    let old = args.get("oldText").and_then(|v| v.as_str()).map(String::from);
    let new = args.get("newText").and_then(|v| v.as_str()).map(String::from);
    if let (Some(old), Some(new)) = (old, new) {
        let mut edits = args
            .get("edits")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        edits.push(json!({ "oldText": old, "newText": new }));
        if let Some(obj) = args.as_object_mut() {
            obj.insert("edits".to_string(), Value::Array(edits));
            obj.remove("oldText");
            obj.remove("newText");
        }
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

fn bash_tool(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    process: Option<BashProcessContext>,
    sandbox: Option<SandboxConfigFn>,
) -> AgentTool {
    let cwd = cwd.to_string();
    let params = s_object(
        vec![
            ("command", s_string("Bash command to execute. Do not use nohup, setsid, disown, or shell background '&' syntax.")),
            ("timeout", s_number("Timeout in seconds (optional, no default timeout)")),
            ("background", s_boolean("Start the command under the supervised process manager, return immediately, and list it in /ps")),
            ("sandboxed", s_boolean("Run this command inside the Linux filesystem sandbox (filesystem confined to sandbox.allowedPaths, network off unless sandbox.network). Overrides the sandbox.enabled setting for this call; unsupported on non-Linux platforms.")),
            ("pty", s_boolean("When true, run the command in a pseudo-terminal (PTY) so interactive programs like sudo can prompt for input (e.g. a password). The PTY merges stdout and stderr; provide stdin up front via the input parameter. Not available with background=true or inside the sandbox.")),
            ("input", s_string("Optional stdin written to the command (followed by a newline) before its output is read. Intended for interactive commands with pty=true (e.g. a sudo password); ignored otherwise.")),
        ],
        vec!["command"],
    );
    let description = format!(
        "Execute a bash command in the current working directory. Finite commands run in the foreground and return stdout and stderr exactly as before. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. For interactive commands that prompt (e.g. sudo), set pty=true and pass stdin via the input parameter. For servers, watchers, and other long-running commands, set background=true to return a stable supervised process id visible in /ps. Unsupervised nohup, setsid, disown, and shell '&' detachment are rejected.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("bash", description, params, move |ctx| {
        let cwd = cwd.clone();
        let session_env = session_env.clone();
        let process = process.clone();
        let sandbox = sandbox.clone();
        async move {
            run_bash(&cwd, ctx.arguments, ctx.on_update, ctx.abort, session_env, process, sandbox).await
        }
    })
    .with_capability(ToolCapability::Exec)
    .with_prompt_guidelines(vec![
        "Inspect PI_* environment variables for current model and session details.".to_string(),
        "Use foreground bash only for finite commands. For servers, watchers, or commands intended to outlive the tool call, set background=true; never use nohup, setsid, disown, or shell '&'. Supervised commands are visible in /ps with logs, signal, stop, and wait controls.".to_string(),
        "When the sandbox is enabled (sandbox.enabled or the sandboxed parameter), commands see only sandbox.allowedPaths; system binaries are read-only; the network is loopback-only unless sandbox.network is true.".to_string(),
        "Interactive commands that prompt for input (e.g. sudo) run with pty=true; provide the stdin (e.g. a password) via the input parameter. pty is a foreground-only mode and cannot be combined with background=true or the sandbox.".to_string(),
    ])
}

const NON_INTERACTIVE_COMMAND_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("SYSTEMD_PAGER", "cat"),
    ("BAT_PAGER", "cat"),
    ("DELTA_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("GLAB_PAGER", "cat"),
    ("PSQL_PAGER", "cat"),
    ("MYSQL_PAGER", "cat"),
    ("AWS_PAGER", ""),
    ("HOMEBREW_PAGER", "cat"),
    ("LESS", "FRX"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PYTHONUNBUFFERED", "1"),
    ("GIT_EDITOR", "true"),
    ("VISUAL", "true"),
    ("EDITOR", "true"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("SSH_ASKPASS", "/usr/bin/false"),
    ("CI", "1"),
    ("npm_config_yes", "true"),
    ("npm_config_update_notifier", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_audit", "false"),
    ("npm_config_progress", "false"),
    ("PNPM_DISABLE_SELF_UPDATE_CHECK", "true"),
    ("PNPM_UPDATE_NOTIFIER", "false"),
    ("YARN_ENABLE_TELEMETRY", "0"),
    ("YARN_ENABLE_PROGRESS_BARS", "0"),
    ("CARGO_TERM_PROGRESS_WHEN", "never"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PIP_NO_INPUT", "1"),
    ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
    ("TF_INPUT", "0"),
    ("TF_IN_AUTOMATION", "1"),
    ("GH_PROMPT_DISABLED", "1"),
    ("COMPOSER_NO_INTERACTION", "1"),
    ("CLOUDSDK_CORE_DISABLE_PROMPTS", "1"),
];

/// Builds the child environment: the inherited environment minus the PI_*
/// session keys, plus whatever the session currently provides.
fn bash_command_env(session_env: Option<&SessionEnvFn>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (k, v) in std::env::vars() {
        if PI_SESSION_ENV_KEYS.contains(&k.as_str()) {
            continue;
        }
        env.push((k, v));
    }
    if let Some(provided) = session_env {
        let provided = provided();
        // Stable order for readable diffs.
        for k in PI_SESSION_ENV_KEYS {
            if let Some(v) = provided.get(*k) {
                if !v.is_empty() {
                    env.push((k.to_string(), v.clone()));
                }
            }
        }
    }
    env
}

/// Resolves the shell, mirroring pi's `getShellConfig` (Unix): `/bin/bash`,
/// then `bash` on PATH, then `sh`. Returns `(shell, args)`.
fn get_shell_config() -> (String, Vec<String>) {
    if path_exists("/bin/bash") {
        return ("/bin/bash".to_string(), vec!["-c".to_string()]);
    }
    if let Some(p) = look_path("bash") {
        return (p, vec!["-c".to_string()]);
    }
    ("sh".to_string(), vec!["-c".to_string()])
}

fn path_exists(p: &str) -> bool {
    std::fs::metadata(p).is_ok()
}

/// Searches `$PATH` for an executable named `name`.
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Mutex-guarded bash output state shared across the reader task(s). Holds the
/// rolling accumulator plus optional callbacks: `on_chunk` for raw merged
/// chunks (RPC `execute_bash`) and `on_update` for throttled partial
/// `AgentToolResult` snapshots (the bash tool).
struct BashState {
    acc: OutputAccumulator,
    on_update: Option<ToolUpdateFn>,
    on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    last_emit: Option<Instant>,
}

impl BashState {
    fn new(
        on_update: Option<ToolUpdateFn>,
        on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    ) -> Self {
        Self {
            acc: OutputAccumulator::new(0, 0, "pi-bash"),
            on_update,
            on_chunk,
            last_emit: None,
        }
    }
    fn append(&mut self, data: &[u8]) {
        self.acc.append(data);
        // Raw merged chunk, in arrival order (RPC live stream).
        if let Some(on_chunk) = &self.on_chunk {
            on_chunk(data);
        }
        // Throttled partial-result snapshots for the bash tool.
        if self.on_update.is_some() {
            let now = Instant::now();
            let should = self
                .last_emit
                .map(|t| now.duration_since(t) >= BASH_UPDATE_THROTTLE)
                .unwrap_or(true);
            if should {
                self.emit();
                self.last_emit = Some(now);
            }
        }
    }
    fn emit(&mut self) {
        let Some(on_update) = &self.on_update else { return };
        let snap = self.acc.snapshot(true);
        let details = if snap.truncation.truncated {
            json!({ "truncation": snap.truncation, "fullOutputPath": snap.full_output_path })
        } else {
            Value::Null
        };
        (on_update)(AgentToolResult {
            content: vec![ContentBlock::text(snap.content)],
            details,
            ..Default::default()
        });
    }
    fn finish(&mut self) -> OutputSnapshot {
        self.acc.finish();
        self.acc.snapshot(true)
    }
    fn get_last_line_bytes(&self) -> usize {
        self.acc.get_last_line_bytes()
    }
    /// Detaches the accumulator's temp file so it outlives the accumulator; the
    /// caller owns cleanup (see `cleanup_full_output_path`).
    fn take_temp_file(&mut self) -> Option<String> {
        self.acc.take_temp_file()
    }
}

/// The low-level outcome of running a command: bounded output snapshot, exit
/// status, and whether it was cancelled/timed out. Spawn and wait I/O errors
/// propagate as `Err`; nonzero exit and abort are encoded in the fields so each
/// wrapper can map them to its own result shape.
struct BashRunOutput {
    content: String,
    truncation: TruncationResult,
    full_output_path: String,
    disk_truncated: bool,
    exit_code: Option<i32>,
    cancelled: bool,
    timed_out: bool,
    last_line_bytes: usize,
}

/// Runs `command` in `cwd`, streaming merged stdout+stderr through the
/// accumulator (and the optional callbacks), enforcing the optional timeout and
/// abort. Shared by the bash tool (`on_update` + timeout) and `execute_bash`
/// (`on_chunk`, no timeout). With `pty` set, the command runs in an opt-in
/// pseudo-terminal (`bash::pty`) so interactive programs like sudo can prompt,
/// with `pty_input` written to the PTY's stdin; a PTY spawn failure without
/// input falls back to the standard paths with a note, while one with input
/// configured errors the call (the fallback could not deliver the input).
/// Returns the bounded output snapshot and exit status; abort wins over
/// timeout.
async fn run_bash_core(
    cwd: &str,
    command: &str,
    session_env: Option<&SessionEnvFn>,
    timeout: Option<f64>,
    abort: AbortSignal,
    on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    on_update: Option<ToolUpdateFn>,
    sandbox: Option<&SandboxConfig>,
    pty: bool,
    pty_input: Option<String>,
) -> Result<BashRunOutput> {
    let (shell, shell_args) = get_shell_config();
    let mut argv = Vec::with_capacity(shell_args.len() + 2);
    argv.push(shell);
    argv.extend(shell_args);
    argv.push(command.to_owned());
    // The child environment: the inherited environment minus the PI_* session
    // keys, plus the session metadata and the unattended-command contract.
    // Foreground bash follows OMP's unattended command contract; these are
    // applied after inherited/session values so pagers, editors, and
    // credential prompts cannot reclaim the parent TUI.
    let mut env = bash_command_env(session_env);
    for (key, value) in NON_INTERACTIVE_COMMAND_ENV {
        env.push(((*key).to_owned(), (*value).to_owned()));
    }
    // With a sandbox config the shell runs behind `unshare` (fresh
    // mount/pid/net namespaces) through the shared runner; the wrapper is
    // spawned through the same Command machinery, so streaming, timeout,
    // abort, and process-group handling are unchanged. Without a sandbox, the
    // command runs through the embedded brush shell (OMP/pi parity); see
    // `tools/bash/brush.rs` for the session model, sandbox split, fallback
    // policy, and timeout/abort reaping.
    //
    // Every bash execution is serialized process-wide (brush and subprocess
    // paths alike): brush's timeout/abort descendant reaping is only sound
    // when no other bash run is spawning descendants concurrently. The wait
    // is bounded by the run's own timeout, so a run that cannot even start in
    // time reports timed out instead of queueing forever; an aborted call
    // reports cancelled.
    let timeout_duration = timeout.map(Duration::from_secs_f64);
    let Some(_exec_guard) = bash::brush::acquire_bash_exec_lock(timeout_duration, &abort).await?
    else {
        // Could not start within the timeout (or abort fired while queued);
        // nothing ran, so there is no output to publish.
        return Ok(BashRunOutput {
            content: String::new(),
            truncation: TruncationResult::default(),
            full_output_path: String::new(),
            disk_truncated: false,
            exit_code: None,
            cancelled: abort.is_aborted(),
            timed_out: !abort.is_aborted(),
            last_line_bytes: 0,
        });
    };
    let state = Arc::new(Mutex::new(BashState::new(on_update, on_chunk)));
    let stream: Arc<dyn Fn(&[u8]) + Send + Sync> = {
        let state = state.clone();
        Arc::new(move |data: &[u8]| state.lock().append(data))
    };
    // PTY mode (opt-in, `pty: true`) runs the command in a pseudo-terminal so
    // interactive programs like sudo can prompt. The PTY environment keeps the
    // normal command contract except TERM: NON_INTERACTIVE_COMMAND_ENV sets
    // TERM=dumb, which breaks interactive rendering — a PTY needs a real
    // terminal type. Built up front because `run_normal` below takes
    // ownership of `env`; `run_bash` rejects pty with background=true and with
    // an active sandbox, so pty is only ever reached in the unsandboxed
    // branch.
    let pty_env: Vec<(String, String)> = if pty {
        let mut pty_env = env.clone();
        pty_env.retain(|(key, _)| key != "TERM");
        pty_env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
        pty_env
    } else {
        Vec::new()
    };
    let pty_abort = abort.clone();
    let pty_stream = stream.clone();
    let fallback_stream = stream.clone();

    // The standard execution paths (sandboxed subprocess, or the embedded
    // brush shell with the plain /bin/bash fallback). Shared by the default
    // path and the PTY spawn-failure fallback so both behave identically.
    let run_normal = async {
        Ok::<SandboxRunOutcome, anyhow::Error>(match sandbox {
            // Sandboxed invocations keep the subprocess path (sandbox wins):
            // the in-process brush shell cannot be wrapped by `unshare`.
            Some(config) => crate::sandbox::run_in_sandbox(
                Some(config),
                Path::new(cwd),
                &argv,
                env,
                timeout_duration,
                abort,
                Some(stream),
            )
            .await?,
            // Unsandboxed default: the embedded brush shell. If brush cannot
            // parse the command (or descendant reaping is unavailable), fall
            // back to the plain /bin/bash subprocess path for an identical
            // observable result (documented policy in `tools/bash/brush.rs`).
            None => {
                use bash::brush::BrushRunOutcome;
                match bash::brush::run_brush_command(
                    Path::new(cwd),
                    command,
                    &env,
                    timeout_duration,
                    abort.clone(),
                    stream.clone(),
                )
                .await?
                {
                    BrushRunOutcome::Executed { exit_code } => SandboxRunOutcome {
                        exit_code,
                        timed_out: false,
                        cancelled: false,
                    },
                    BrushRunOutcome::TimedOut => SandboxRunOutcome {
                        exit_code: None,
                        timed_out: true,
                        cancelled: false,
                    },
                    BrushRunOutcome::Cancelled => SandboxRunOutcome {
                        exit_code: None,
                        timed_out: false,
                        cancelled: true,
                    },
                    BrushRunOutcome::Fallback => crate::sandbox::run_in_sandbox(
                        None,
                        Path::new(cwd),
                        &argv,
                        env,
                        timeout_duration,
                        abort,
                        Some(stream),
                    )
                    .await?,
                }
            }
        })
    };

    let outcome = if pty {
        // The PTY merges stdout+stderr; `pty_input` (e.g. a sudo password) is
        // written to the PTY's stdin up front (see `bash::pty`).
        match bash::pty::run_pty_command(
            Path::new(cwd),
            &argv,
            &pty_env,
            pty_input.as_deref(),
            timeout_duration,
            pty_abort,
            pty_stream,
        )
        .await
        {
            bash::pty::PtyRunOutcome::Executed { exit_code } => SandboxRunOutcome {
                exit_code,
                timed_out: false,
                cancelled: false,
            },
            bash::pty::PtyRunOutcome::TimedOut => SandboxRunOutcome {
                exit_code: None,
                timed_out: true,
                cancelled: false,
            },
            bash::pty::PtyRunOutcome::Cancelled => SandboxRunOutcome {
                exit_code: None,
                timed_out: false,
                cancelled: true,
            },
            bash::pty::PtyRunOutcome::SpawnFailed(err) => {
                if pty_input.is_some() {
                    // The input was meant for the PTY's stdin; the normal
                    // paths wire stdin to /dev/null, so a fallback would
                    // leave the command hanging on input that never arrives.
                    // Fail the call instead.
                    anyhow::bail!("pty spawn failed with input configured: {err}");
                }
                fallback_stream(
                    format!("[PTY spawn failed: {err}; falling back to normal execution]\n")
                        .as_bytes(),
                );
                run_normal.await?
            }
        }
    } else {
        run_normal.await?
    };

    let snap = state.lock().finish();
    let last_line_bytes = state.lock().get_last_line_bytes();
    // Detach the temp file so the returned `full_output_path` stays valid for
    // the agent to `read`; the caller owns cleanup. The runner only returns
    // `Err` on spawn/wait I/O failure, in which case this is never reached and
    // the accumulator's Drop deletes the spill file (no leak on failure).
    let _ = state.lock().take_temp_file();

    Ok(BashRunOutput {
        content: snap.content,
        truncation: snap.truncation,
        full_output_path: snap.full_output_path,
        disk_truncated: snap.disk_truncated,
        exit_code: outcome.exit_code,
        cancelled: outcome.cancelled,
        timed_out: outcome.timed_out,
        last_line_bytes,
    })
}

fn contains_detaching_shell_substitution(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'\'' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'\'' {
                    index += 1;
                }
                index = (index + 1).min(bytes.len());
            }
            b'$' if bytes.get(index + 1) == Some(&b'(') => {
                let opening = index + 1;
                let end = skip_dollar_substitution(command, opening);
                let content_end = end.saturating_sub(1).max(opening + 1);
                if command[opening + 1..content_end].contains(['&', ';']) {
                    return true;
                }
                index = end;
            }
            b'`' => {
                let end = skip_backtick_substitution(command, index);
                let content_end = end.saturating_sub(1).max(index + 1);
                if command[index + 1..content_end].contains(['&', ';']) {
                    return true;
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellOperator {
    Background,
    Control,
    Redirection,
    GroupOpen,
    GroupClose,
}

#[derive(Debug)]
enum ShellTokenKind {
    Word(String),
    Operator(ShellOperator),
}

#[derive(Debug)]
struct ShellToken {
    kind: ShellTokenKind,
    start: usize,
    end: usize,
}

#[derive(Default)]
struct ShellDetachAnalysis {
    backgrounds: Vec<std::ops::Range<usize>>,
    nohups: Vec<std::ops::Range<usize>>,
    setsids: Vec<std::ops::Range<usize>>,
    disowns: Vec<std::ops::Range<usize>>,
    leading_nohup: Option<std::ops::Range<usize>>,
    leading_nohup_separator: Option<std::ops::Range<usize>>,
    terminal_background: Option<std::ops::Range<usize>>,
}

impl ShellDetachAnalysis {
    fn has_detach_intent(&self) -> bool {
        !self.backgrounds.is_empty()
            || !self.nohups.is_empty()
            || !self.setsids.is_empty()
            || !self.disowns.is_empty()
    }

    fn detected_constructs(&self) -> String {
        let mut constructs = Vec::new();
        if !self.nohups.is_empty() {
            constructs.push("nohup");
        }
        if !self.setsids.is_empty() {
            constructs.push("setsid");
        }
        if !self.disowns.is_empty() {
            constructs.push("disown");
        }
        if !self.backgrounds.is_empty() {
            constructs.push("shell background '&'");
        }
        constructs.join(", ")
    }
}

fn shell_detach_analysis(command: &str) -> ShellDetachAnalysis {
    let tokens = shell_tokens(command);
    let first_non_whitespace = command
        .char_indices()
        .find_map(|(index, character)| (!character.is_whitespace()).then_some(index));
    let mut analysis = ShellDetachAnalysis::default();
    let mut command_position = true;
    let mut prefix_options = false;
    let mut redirection_target = false;

    for (index, token) in tokens.iter().enumerate() {
        match &token.kind {
            ShellTokenKind::Operator(operator) => match operator {
                ShellOperator::Background => {
                    analysis.backgrounds.push(token.start..token.end);
                    command_position = true;
                    prefix_options = false;
                    redirection_target = false;
                }
                ShellOperator::Control | ShellOperator::GroupOpen => {
                    command_position = true;
                    prefix_options = false;
                    redirection_target = false;
                }
                ShellOperator::Redirection => redirection_target = true,
                ShellOperator::GroupClose => {
                    command_position = false;
                    prefix_options = false;
                    redirection_target = false;
                }
            },
            ShellTokenKind::Word(word) => {
                if redirection_target {
                    redirection_target = false;
                    continue;
                }
                if !command_position {
                    continue;
                }
                if shell_assignment_word(word) {
                    continue;
                }
                if prefix_options && word.starts_with('-') {
                    continue;
                }
                if shell_command_prefix(word) {
                    prefix_options = matches!(word.as_str(), "builtin" | "command" | "env" | "exec");
                    continue;
                }

                let range = token.start..token.end;
                match word.as_str() {
                    "nohup" => {
                        analysis.nohups.push(range.clone());
                        if first_non_whitespace == Some(token.start) {
                            analysis.leading_nohup = Some(range);
                            if let Some(ShellToken {
                                kind: ShellTokenKind::Word(separator),
                                start,
                                end,
                            }) = tokens.get(index + 1)
                                && separator == "--"
                            {
                                analysis.leading_nohup_separator = Some(*start..*end);
                            }
                        }
                    }
                    "setsid" => analysis.setsids.push(range),
                    "disown" => analysis.disowns.push(range),
                    _ => {}
                }
                command_position = false;
                prefix_options = false;
            }
        }
    }

    if let Some(ShellToken {
        kind: ShellTokenKind::Operator(ShellOperator::Background),
        start,
        end,
    }) = tokens.last()
    {
        analysis.terminal_background = Some(*start..*end);
    }
    analysis
}

fn normalize_supervised_bash(command: &str, analysis: &ShellDetachAnalysis) -> Result<String> {
    if !analysis.setsids.is_empty() || !analysis.disowns.is_empty() {
        return Err(anyhow!(
            "Cannot safely supervise shell detachment using {}. Remove the detach command and run the long-lived command with background=true so it remains visible in /ps.",
            analysis.detected_constructs()
        ));
    }
    if !analysis.nohups.is_empty()
        && (analysis.nohups.len() != 1 || analysis.leading_nohup.is_none())
    {
        return Err(anyhow!(
            "Cannot safely supervise nested or compound nohup execution. Remove nohup and run the long-lived command with background=true so it remains visible in /ps."
        ));
    }
    if !analysis.backgrounds.is_empty()
        && (analysis.backgrounds.len() != 1
            || analysis.terminal_background.as_ref() != analysis.backgrounds.first())
    {
        return Err(anyhow!(
            "Cannot safely supervise a compound shell background job. Remove shell '&' syntax and run the long-lived command with background=true so it remains visible in /ps."
        ));
    }

    let mut removals = Vec::new();
    if let Some(range) = &analysis.leading_nohup {
        removals.push(range.clone());
    }
    if let Some(range) = &analysis.leading_nohup_separator {
        removals.push(range.clone());
    }
    if let Some(range) = &analysis.terminal_background {
        removals.push(range.clone());
    }
    removals.sort_by(|left, right| right.start.cmp(&left.start));
    let mut normalized = command.to_owned();
    for range in removals {
        normalized.replace_range(range, "");
    }
    if normalized.trim().is_empty() {
        return Err(anyhow!("background bash requires a command to supervise"));
    }
    Ok(normalized)
}

fn reject_unsupervised_shell_detach(analysis: &ShellDetachAnalysis) -> Result<()> {
    if analysis.has_detach_intent() {
        return Err(anyhow!(
            "Unsupervised background or detached bash execution is not allowed (detected {}). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls.",
            analysis.detected_constructs()
        ));
    }
    Ok(())
}

fn shell_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn shell_command_prefix(word: &str) -> bool {
    matches!(
        word,
        "!" | "builtin" | "command" | "coproc" | "do" | "elif" | "else" | "env"
            | "exec" | "if" | "then" | "time" | "until" | "while"
    )
}

fn shell_tokens(command: &str) -> Vec<ShellToken> {
    let bytes = command.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            if bytes[index] == b'\n' {
                tokens.push(ShellToken {
                    kind: ShellTokenKind::Operator(ShellOperator::Control),
                    start: index,
                    end: index + 1,
                });
            }
            index += 1;
            continue;
        }
        if bytes[index] == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'`' {
            let end = skip_backtick_substitution(command, index);
            tokens.push(ShellToken {
                kind: ShellTokenKind::Word(command[index..end].to_owned()),
                start: index,
                end,
            });
            index = end;
            continue;
        }
        if let Some((operator, end)) = shell_operator(command, index) {
            tokens.push(ShellToken {
                kind: ShellTokenKind::Operator(operator),
                start: index,
                end,
            });
            index = end;
            continue;
        }
        if bytes[index].is_ascii_digit() {
            let mut end = index;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end < bytes.len()
                && matches!(bytes[end], b'<' | b'>')
                && let Some((ShellOperator::Redirection, redirection_end)) = shell_operator(command, end)
            {
                tokens.push(ShellToken {
                    kind: ShellTokenKind::Operator(ShellOperator::Redirection),
                    start: index,
                    end: redirection_end,
                });
                index = redirection_end;
                continue;
            }
        }

        let start = index;
        let mut word = Vec::new();
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'&' | b'|' | b';' | b'(' | b')' | b'<' | b'>' | b'`')
        {
            if bytes[index] == b'\'' {
                index += 1;
                while index < bytes.len() && bytes[index] != b'\'' {
                    let width = command[index..].chars().next().map_or(1, char::len_utf8);
                    word.extend_from_slice(&bytes[index..index + width]);
                    index += width;
                }
                if index < bytes.len() {
                    index += 1;
                }
                continue;
            }
            if bytes[index] == b'"' {
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\'
                        && index + 1 < bytes.len()
                        && matches!(bytes[index + 1], b'$' | b'`' | b'"' | b'\\' | b'\n')
                    {
                        index += 1;
                    }
                    let width = command[index..].chars().next().map_or(1, char::len_utf8);
                    word.extend_from_slice(&bytes[index..index + width]);
                    index += width;
                }
                if index < bytes.len() {
                    index += 1;
                }
                continue;
            }
            match bytes[index] {
                b'\\' if index + 1 < bytes.len() => {
                    index += 1;
                    let width = command[index..].chars().next().map_or(1, char::len_utf8);
                    word.extend_from_slice(&bytes[index..index + width]);
                    index += width;
                }
                b'\\' => {
                    word.push(b'\\');
                    index += 1;
                }
                b'$' if bytes.get(index + 1) == Some(&b'(') => {
                    word.push(b'$');
                    let opening = index + 1;
                    let end = skip_dollar_substitution(command, opening);
                    word.extend_from_slice(&bytes[opening..end]);
                    index = end;
                }
                b'`' => {
                    let end = skip_backtick_substitution(command, index);
                    word.extend_from_slice(&bytes[index..end]);
                    index = end;
                }
                _ => {
                    let width = command[index..].chars().next().map_or(1, char::len_utf8);
                    word.extend_from_slice(&bytes[index..index + width]);
                    index += width;
                }
            }
        }
        tokens.push(ShellToken {
            kind: ShellTokenKind::Word(String::from_utf8(word).expect("shell token preserves UTF-8")),
            start,
            end: index,
        });
    }
    tokens
}

fn shell_operator(command: &str, index: usize) -> Option<(ShellOperator, usize)> {
    let bytes = command.as_bytes();
    let current = *bytes.get(index)?;
    let next = bytes.get(index + 1).copied();
    match current {
        b'&' if next == Some(b'&') => Some((ShellOperator::Control, index + 2)),
        b'&' if next == Some(b'>') => {
            let end = if bytes.get(index + 2) == Some(&b'>') { index + 3 } else { index + 2 };
            Some((ShellOperator::Redirection, end))
        }
        b'&' => Some((ShellOperator::Background, index + 1)),
        b'|' => {
            let end = if matches!(next, Some(b'|' | b'&')) { index + 2 } else { index + 1 };
            Some((ShellOperator::Control, end))
        }
        b';' if next == Some(b'&') => Some((ShellOperator::Background, index + 2)),
        b';' if next == Some(b';') && bytes.get(index + 2) == Some(&b'&') => {
            Some((ShellOperator::Background, index + 3))
        }
        b';' => {
            let end = if next == Some(b';') { index + 2 } else { index + 1 };
            Some((ShellOperator::Control, end))
        }
        b'(' if index > 0 && bytes[index - 1] == b'$' => None,
        b'(' => Some((ShellOperator::GroupOpen, index + 1)),
        b')' => Some((ShellOperator::GroupClose, index + 1)),
        b'`' => None,
        b'<' | b'>' => {
            let mut end = index + 1;
            if bytes.get(end) == Some(&current) {
                end += 1;
                if current == b'<' && bytes.get(end) == Some(&b'-') {
                    end += 1;
                }
            }
            if matches!(bytes.get(end), Some(b'&' | b'|')) {
                end += 1;
            }
            Some((ShellOperator::Redirection, end))
        }
        _ => None,
    }
}

fn skip_dollar_substitution(command: &str, opening: usize) -> usize {
    let bytes = command.as_bytes();
    let mut depth = 1_usize;
    let mut index = opening + 1;
    let mut quote = None;
    while index < bytes.len() {
        match (quote, bytes[index]) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => quote = None,
            (None, b'\'' | b'"') => quote = Some(bytes[index]),
            (Some(b'"'), b'\\') | (None, b'\\') => index += 1,
            (None, b'(') => depth += 1,
            (None, b')') => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

fn skip_backtick_substitution(command: &str, opening: usize) -> usize {
    let bytes = command.as_bytes();
    let mut index = opening + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'`' => return index + 1,
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

/// Bash tool wrapper: validates the tool timeout, checks the working directory,
/// emits the initial empty update, runs the command, and maps the low-level
/// outcome to the agent's result contract (abort/timeout/nonzero-exit become
/// errors; a signal-killed child is treated as success). Formatting matches pi.
async fn run_bash(
    cwd: &str,
    args: Value,
    on_update: ToolUpdateFn,
    abort: AbortSignal,
    session_env: Option<SessionEnvFn>,
    process: Option<BashProcessContext>,
    sandbox: Option<SandboxConfigFn>,
) -> Result<AgentToolResult> {
    let command = required_arg_str(&args, "command")?;
    let timeout = arg_float(&args, "timeout");
    if let Some(t) = timeout {
        if !t.is_finite() || t <= 0.0 {
            return Err(anyhow!("Invalid timeout: must be a finite number of seconds"));
        }
        if t * 1000.0 > MAX_BASH_TIMEOUT_MS {
            return Err(anyhow!("Invalid timeout: maximum is {MAX_BASH_TIMEOUT_SECONDS} seconds"));
        }
    }
    check_aborted(&abort)?;
    let background = arg_bool(&args, "background");
    let pty = arg_bool(&args, "pty");
    let pty_input = arg_str(&args, "input");
    // An absent/empty `input` means "no stdin"; only non-empty strings are
    // written to the PTY.
    let pty_input = (!pty_input.is_empty()).then_some(pty_input);
    // Per-call `sandboxed` overrides the `sandbox.enabled` setting; missing
    // falls back to the setting. Resolution happens per spawn so a settings
    // reload takes effect on the next command (RELOAD apply behavior).
    let mut sandbox_config: Option<crate::SandboxConfig> = None;
    match args.get("sandboxed").and_then(|value| value.as_bool()) {
        Some(true) => {
            let mut config = sandbox
                .as_ref()
                .and_then(|resolve| resolve())
                .unwrap_or_else(|| {
                    crate::SandboxConfig::default_for(Path::new(cwd), &crate::agent_dir_path())
                });
            config.enabled = true;
            sandbox_config = Some(config);
        }
        Some(false) => {}
        None => {
            sandbox_config = sandbox
                .as_ref()
                .and_then(|resolve| resolve())
                .filter(|config| config.enabled);
        }
    }
    let detach = shell_detach_analysis(&command);
    if pty && background {
        return Err(anyhow!(
            "pty bash is not supported with background=true: PTY input and output are only available in the foreground. Run the interactive command without background=true."
        ));
    }
    if pty && sandbox_config.is_some() {
        return Err(anyhow!(
            "pty bash is not supported inside the sandbox: the PTY child cannot be wrapped by unshare. Run the interactive command without sandboxed=true (or disable sandbox.enabled)."
        ));
    }
    if contains_detaching_shell_substitution(&command) {
        return Err(anyhow!(
            "Cannot safely determine supervision for shell substitution containing background or control syntax. Move the long-lived command out of the substitution and run it with background=true so it remains visible in /ps."
        ));
    }
    if !background {
        reject_unsupervised_shell_detach(&detach)?;
    }
    if background {
        if sandbox_config.is_some() {
            return Err(anyhow!(
                "sandboxed background bash is not supported: the supervised process manager runs outside the sandbox. Run the command in the foreground with sandboxed=true, or drop the sandbox for the background run."
            ));
        }
        let process = process.ok_or_else(|| anyhow!("background bash is unavailable in this context"))?;
        if !path_exists(cwd) {
            return Err(anyhow!("Working directory does not exist: {cwd}\nCannot execute bash commands."));
        }
        let command = normalize_supervised_bash(&command, &detach)?;
        let (shell, shell_args) = get_shell_config();
        let mut argv = Vec::with_capacity(shell_args.len() + 2);
        argv.push(shell);
        argv.extend(shell_args);
        argv.push(command);
        let mut env = std::env::vars()
            .filter(|(key, _)| key.starts_with("PI_"))
            .map(|(key, _)| (key, None))
            .collect::<BTreeMap<_, _>>();
        if let Some(session_env) = session_env.as_ref() {
            let provided = session_env();
            for key in PI_SESSION_ENV_KEYS {
                if let Some(value) = provided.get(*key).filter(|value| !value.is_empty()) {
                    env.insert((*key).to_owned(), Some(value.clone()));
                }
            }
        }
        let info = process.manager.spawn(process.owner_id, crate::ProcessSpawnSpec {
            argv,
            cwd: Path::new(cwd).to_path_buf(),
            env,
            tty: false,
            terminal_size: None,
            label: None,
            timeout_ms: timeout.map(|seconds| (seconds * 1000.0) as u64),
            output_bytes: None,
        }).await?;
        return Ok(AgentToolResult {
            content: vec![ContentBlock::text(format!("Process started: {} (visible in /ps)", info.id))],
            details: serde_json::to_value(info)?,
            ..Default::default()
        });
    }
    if !path_exists(cwd) {
        return Err(anyhow!("Working directory does not exist: {cwd}\nCannot execute bash commands."));
    }
    // pi emits an initial empty update before spawning.
    (on_update)(AgentToolResult::default());
    let out = run_bash_core(
        cwd,
        &command,
        session_env.as_ref(),
        timeout,
        abort,
        None,
        Some(on_update),
        sandbox_config.as_ref(),
        pty,
        pty_input,
    )
    .await?;

    let content = out.content;
    let truncation = out.truncation;
    let full_output_path = out.full_output_path;
    let last_line_bytes = out.last_line_bytes;
    let disk_truncated = out.disk_truncated;

    let format_output = |empty_text: &str, publish_path: bool| -> (String, Value) {
        let mut text = if content.is_empty() {
            empty_text.to_string()
        } else {
            content.clone()
        };
        let mut details = Value::Null;
        if truncation.truncated {
            if publish_path {
                details = if disk_truncated {
                    json!({ "truncation": truncation, "fullOutputPath": full_output_path, "diskCapped": true })
                } else {
                    json!({ "truncation": truncation, "fullOutputPath": full_output_path })
                };
                let start_line = truncation.total_lines - truncation.output_lines + 1;
                let end_line = truncation.total_lines;
                if truncation.last_line_partial {
                    let last_line_size = format_size(last_line_bytes);
                    text.push_str(&format!(
                        "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {full_output_path}]",
                        format_size(truncation.output_bytes)
                    ));
                } else if truncation.truncated_by == "lines" {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line}-{end_line} of {} . Full output: {full_output_path}]",
                        truncation.total_lines
                    ));
                } else {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_output_path}]",
                        truncation.total_lines,
                        format_size(DEFAULT_MAX_BYTES)
                    ));
                }
                if disk_truncated {
                    text.push_str(&format!(
                        "\n[Full output was capped to {} on disk; the on-disk copy is partial.]",
                        format_size(bash::MAX_FULL_OUTPUT_DISK_BYTES)
                    ));
                }
            }
            // When `publish_path` is false (error paths) the spill file is about
            // to be deleted, so we deliberately do NOT emit a (dead) full-output
            // path/details — only the bounded tail is shown.
        }
        (text, details)
    };
    let append_status = |t: &str, status: &str| -> String {
        if t.is_empty() {
            status.to_string()
        } else {
            format!("{t}\n\n{status}")
        }
    };

    // Abort wins over timeout when both fired.
    if out.cancelled {
        let (text, _) = format_output("", false);
        cleanup_full_output_path(&full_output_path);
        return Err(anyhow!("{}", append_status(&text, "Command aborted")));
    }
    if out.timed_out {
        let (text, _) = format_output("", false);
        cleanup_full_output_path(&full_output_path);
        // pi prints the raw timeout value: 0.5 renders "0.5", 2 renders "2".
        let t = timeout.unwrap_or(0.0);
        return Err(anyhow!(
            "{}",
            append_status(&text, &format!("Command timed out after {} seconds", format_float(t)))
        ));
    }
    // A signal-killed child has no exit code (pi: exitCode === null) and is
    // treated as success with whatever output was produced.
    if let Some(code) = out.exit_code {
        if code != 0 {
            let (text, _) = format_output("(no output)", false);
            cleanup_full_output_path(&full_output_path);
            return Err(anyhow!(
                "{}",
                append_status(&text, &format!("Command exited with code {code}"))
            ));
        }
    }

    let (text, details) = format_output("(no output)", true);
    let mut res = text_result(text);
    if !details.is_null() {
        res.details = details;
    }
    Ok(res)
}

/// Semantic bash execution result for RPC consumers (port of pi's bash result
/// shape). `output` is the bounded tail (truncated to the last N lines/bytes);
/// when `truncated`, the full output is in `full_output_path`. A nonzero exit
/// is `Ok` with `exit_code` set; abort is `Ok` with `cancelled = true`; spawn
/// errors are `Err`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Executes a bash command in `cwd` and returns a semantic `BashResult`. Raw
/// merged stdout+stderr chunks are emitted in arrival order to `on_chunk` as
/// they arrive. No timeout (RPC callers drive cancellation via `abort`);
/// spawn errors are `Err`, nonzero exit is `Ok(exit_code)`, abort is
/// `Ok(cancelled)`. The bash tool's timeout/formatting/error behavior is
/// unchanged (it uses the shared `run_bash_core` internally).
pub async fn execute_bash(
    cwd: &Path,
    command: &str,
    session_env: Option<SessionEnvFn>,
    sandbox: Option<SandboxConfigFn>,
    on_chunk: Arc<dyn Fn(String) + Send + Sync>,
    abort: AbortSignal,
) -> Result<BashResult> {
    let cwd_str = cwd.to_string_lossy().into_owned();
    let raw: Arc<dyn Fn(&[u8]) + Send + Sync> =
        Arc::new(move |b: &[u8]| on_chunk(String::from_utf8_lossy(b).into_owned()));
    // RPC bash has no per-call override; honor the settings default only.
    let sandbox_config = sandbox
        .as_ref()
        .and_then(|resolve| resolve())
        .filter(|config| config.enabled);
    let out = run_bash_core(
        &cwd_str,
        command,
        session_env.as_ref(),
        None,
        abort,
        Some(raw),
        None,
        sandbox_config.as_ref(),
        false,
        None,
    )
    .await?;
    Ok(BashResult {
        output: out.content,
        exit_code: out.exit_code,
        cancelled: out.cancelled,
        truncated: out.truncation.truncated,
        full_output_path: if out.full_output_path.is_empty() {
            None
        } else {
            Some(out.full_output_path)
        },
    })
}

/// Removes a bash full-output temp file (a detached spill file returned by the
/// `bash` tool or `execute_bash`). Idempotent: a missing file or empty path is a
/// no-op. The application/session should call this for any `full_output_path` it
/// no longer needs (e.g. on shutdown, or once the agent has consumed it) so
/// successful commands don't leak temp files. Also unregisters the path from the
/// process-wide spill registry.
pub use bash::{bash_spill_dir, cleanup_all_bash_spills, cleanup_full_output_path};

/// Formats an f64 the way JS `String(number)` does for the common bash timeout
/// values (shortest round-trippable decimal, no trailing `.0` for integers).
fn format_float(t: f64) -> String {
    if t.fract() == 0.0 {
        format!("{t:.0}")
    } else {
        format!("{t}")
    }
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

fn ls_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            ("path", s_string("Directory to list (default: current directory)")),
            ("limit", s_number("Maximum number of entries to return (default: 500)")),
        ],
        vec![],
    );
    let description = format!(
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {} entries or {}KB (whichever is hit first).",
        LS_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("ls", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_ls(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

fn run_ls(workspace: &crate::WorkspaceRoots, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let dir = match args.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(p) => resolve_scoped_path(p, workspace)?,
        None => workspace.cwd().to_string_lossy().into_owned(),
    };
    let limit = arg_int(&args, "limit")?.map(|l| l.max(0) as usize).unwrap_or(LS_DEFAULT_LIMIT);
    let info = std::fs::metadata(&dir).map_err(|_| anyhow!("Path not found: {dir}"))?;
    check_aborted(&abort)?;
    if !info.is_dir() {
        return Err(anyhow!("Not a directory: {dir}"));
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| anyhow!("Cannot read directory: {e}"))?;
    check_aborted(&abort)?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // pi sorts with toLowerCase().localeCompare; approximate with lowercase
    // lexicographic so punctuation/underscore order matches.
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));

    let mut results = Vec::new();
    let mut entry_limit_reached = false;
    for name in &names {
        check_aborted(&abort)?;
        if results.len() >= limit {
            entry_limit_reached = true;
            break;
        }
        // Stat (follows symlinks) to detect dir-ness, like pi.
        match std::fs::metadata(Path::new(&dir).join(name)) {
            Ok(st) => {
                let suffix = if st.is_dir() { "/" } else { "" };
                results.push(format!("{name}{suffix}"));
            }
            // Skip entries we cannot stat.
            Err(_) => continue,
        }
    }
    if results.is_empty() {
        if entry_limit_reached {
            let mut result = text_result(format!(
                "(empty directory)\n\n[{limit} entries limit reached. Use limit={} for more]",
                limit.saturating_mul(2)
            ));
            result.details = json!({ "entryLimitReached": limit });
            return Ok(result);
        }
        return Ok(text_result("(empty directory)"));
    }
    let raw_output = results.join("\n");
    let tr = truncate_head(&raw_output, usize::MAX, 0);
    let mut output = tr.content.clone();
    let mut details = json!({});
    let mut notices = Vec::new();
    if entry_limit_reached {
        notices.push(format!("{limit} entries limit reached. Use limit={} for more", limit * 2));
        details["entryLimitReached"] = json!(limit);
    }
    if tr.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details["truncation"] = json!(tr);
    }
    if !notices.is_empty() {
        output.push_str("\n\n[");
        output.push_str(&notices.join(". "));
        output.push(']');
    }
    let mut res = text_result(output);
    res.details = details;
    Ok(res)
}

// ---------------------------------------------------------------------------
// find (glob)
// ---------------------------------------------------------------------------

fn find_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            ("pattern", s_string("Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'")),
            ("path", s_string("Directory to search in (default: current directory)")),
            ("limit", s_number("Maximum number of results (default: 1000)")),
        ],
        vec!["pattern"],
    );
    let description = format!(
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {} results or {}KB (whichever is hit first).",
        FIND_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("find", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_find(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

fn run_find(workspace: &crate::WorkspaceRoots, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    let root = match args.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(p) => resolve_scoped_path(p, workspace)?,
        None => workspace.cwd().to_string_lossy().into_owned(),
    };
    // Keep the raw signed limit (Go/pi): non-positive = unlimited; limit=0 still
    // reports the odd "0 results limit reached" notice (fd 0-is-unlimited).
    let limit = arg_int(&args, "limit")?.unwrap_or(FIND_DEFAULT_LIMIT as i64);
    let unlimited = limit <= 0;
    let limit_usize = if unlimited { usize::MAX } else { limit as usize };
    if !path_exists(&root) {
        return Err(anyhow!("Path not found: {root}"));
    }
    // fd: gitignore applies whether or not we are in a repo (--no-require-git
    // outside a repo). Inside a repo, stop parent .gitignore rules at nested
    // repository boundaries.
    let mut ig = IgnoreStack::new(&root, false, true);
    let mut results = Vec::new();

    walk(&root, &mut ig, &mut |abs, rel, _is_dir| {
        if abort.is_aborted() {
            return WalkControl::Stop;
        }
        if !unlimited && results.len() >= limit_usize {
            return WalkControl::Stop;
        }
        if match_fd_glob(&pattern, rel, abs) {
            results.push(rel.replace('\\', "/"));
        }
        WalkControl::Continue
    });
    check_aborted(&abort)?;

    // Deterministic ordering (fd's traversal order is unspecified).
    results.sort();
    // pi: resultLimitReached = len >= effectiveLimit — limit 0 still notices.
    let result_limit_reached = limit >= 0 && results.len() >= limit as usize;
    if results.is_empty() {
        return Ok(text_result("No files found matching pattern"));
    }
    let raw_output = results.join("\n");
    let tr = truncate_head(&raw_output, usize::MAX, 0);
    let mut output = tr.content.clone();
    let mut details = json!({});
    let mut notices = Vec::new();
    if result_limit_reached {
        notices.push(format!("{limit} results limit reached. Use limit={} for more, or refine pattern", limit * 2));
        details["resultLimitReached"] = json!(limit);
    }
    if tr.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details["truncation"] = json!(tr);
    }
    if !notices.is_empty() {
        output.push_str("\n\n[");
        output.push_str(&notices.join(". "));
        output.push(']');
    }
    let mut res = text_result(output);
    res.details = details;
    Ok(res)
}

// ---------------------------------------------------------------------------
// glob (sandboxed native file matcher for child agents / OMP parity)
// ---------------------------------------------------------------------------

fn glob_tool(cwd: &str) -> AgentTool {
    glob_tool_for_workspace(factory_workspace(cwd))
}

/// Builds the sandboxed `glob` tool for an explicit multi-root workspace.
/// Used when the main session optionally enables glob without broadening the
/// default coding tool set.
#[must_use]
pub fn create_glob_tool_for_workspace(workspace: crate::WorkspaceRoots) -> AgentTool {
    glob_tool_for_workspace(workspace)
}

fn glob_tool_for_workspace(workspace: crate::WorkspaceRoots) -> AgentTool {
    let params = s_object(
        vec![
            (
                "pattern",
                s_string("Glob pattern to match, e.g. '*.rs', '**/*.ts', or 'src/**/*.spec.ts'"),
            ),
            (
                "path",
                s_string(
                    "Directory, file, or semicolon-separated targets to search (default: workspace cwd). Each target is confined to workspace roots.",
                ),
            ),
            (
                "hidden",
                s_boolean("Include hidden files and directories (default: false)"),
            ),
            (
                "gitignore",
                s_boolean("Respect .gitignore rules (default: true)"),
            ),
            (
                "limit",
                s_number(&format!(
                    "Maximum number of matches to return (default: {GLOB_DEFAULT_LIMIT}, max: {GLOB_MAX_LIMIT})"
                )),
            ),
        ],
        vec!["pattern"],
    );
    let description = format!(
        "Match files and directories by glob pattern under the configured workspace roots. Sandboxed (no traversal or symlink escape). Does not shell out. Hidden entries are skipped unless hidden=true. Respects .gitignore unless gitignore=false. Output is truncated to at most {GLOB_MAX_LIMIT} results or {}KB.",
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("glob", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_glob(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

fn run_glob(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    if pattern.is_empty() {
        return Err(anyhow!("pattern is required"));
    }
    let include_hidden = arg_bool(&args, "hidden");
    // Default true: treat missing as true (OMP/child-agent default).
    let use_gitignore = args
        .get("gitignore")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let limit = arg_int(&args, "limit")?
        .map(|l| l.max(1) as usize)
        .unwrap_or(GLOB_DEFAULT_LIMIT)
        .min(GLOB_MAX_LIMIT);

    let path_arg = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let targets: Vec<String> = if path_arg.is_empty() {
        vec![workspace.cwd().to_string_lossy().into_owned()]
    } else {
        path_arg
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| resolve_scoped_path(s, workspace))
            .collect::<Result<Vec<_>>>()?
    };
    if targets.is_empty() {
        return Err(anyhow!("path resolved to no targets"));
    }

    let mut results: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut limit_reached = false;

    'targets: for target in &targets {
        check_aborted(&abort)?;
        if !path_exists(target) {
            return Err(anyhow!("Path not found: {target}"));
        }
        let meta =
            std::fs::metadata(target).map_err(|e| anyhow!("Path not found: {target}: {e}"))?;
        if meta.is_file() {
            let name = Path::new(target)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            if match_fd_glob(&pattern, name, target) {
                let rel = pathdiff_rel(workspace.cwd(), target);
                if seen.insert(rel.clone()) {
                    results.push(rel);
                    if results.len() >= limit {
                        limit_reached = true;
                        break 'targets;
                    }
                }
            }
            continue;
        }

        let mut ig = if use_gitignore {
            IgnoreStack::new(target, false, true)
        } else {
            IgnoreStack::without_gitignore(target)
        };
        walk(target, &mut ig, &mut |abs, rel, is_dir| {
            if abort.is_aborted() {
                return WalkControl::Stop;
            }
            if results.len() >= limit {
                limit_reached = true;
                return WalkControl::Stop;
            }
            let base = Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            if !include_hidden && base.starts_with('.') {
                return if is_dir {
                    WalkControl::SkipDir
                } else {
                    WalkControl::Continue
                };
            }
            if match_fd_glob(&pattern, rel, abs) {
                // Paths relative to the search target (OMP/find parity).
                let out = rel.replace('\\', "/");
                if seen.insert(out.clone()) {
                    results.push(out);
                    if results.len() >= limit {
                        limit_reached = true;
                        return WalkControl::Stop;
                    }
                }
            }
            WalkControl::Continue
        });
        if limit_reached {
            break;
        }
    }
    check_aborted(&abort)?;

    results.sort();
    if results.is_empty() {
        return Ok(text_result("No files found matching pattern"));
    }
    let raw_output = results.join("\n");
    let tr = truncate_head(&raw_output, usize::MAX, 0);
    let mut output = tr.content.clone();
    let mut details = json!({ "count": results.len() });
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached (max {GLOB_MAX_LIMIT}). Refine pattern or path"
        ));
        details["resultLimitReached"] = json!(limit);
    }
    if tr.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details["truncation"] = json!(tr);
    }
    if !notices.is_empty() {
        output.push_str("\n\n[");
        output.push_str(&notices.join(". "));
        output.push(']');
    }
    let mut res = text_result(output);
    res.details = details;
    Ok(res)
}

/// Relative path of `abs` under `base` when possible; otherwise the absolute path.
fn pathdiff_rel(base: &Path, abs: &str) -> String {
    let abs_path = Path::new(abs);
    abs_path
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.replace('\\', "/"))
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

fn grep_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            ("pattern", s_string("Search pattern (regex or literal string)")),
            ("path", s_string("Directory or file to search (default: current directory)")),
            ("glob", s_string("Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'")),
            ("ignoreCase", s_boolean("Case-insensitive search (default: false)")),
            ("literal", s_boolean("Treat pattern as literal string instead of regex (default: false)")),
            ("context", s_number("Number of lines to show before and after each match (default: 0)")),
            ("limit", s_number("Maximum number of matches to return (default: 100)")),
        ],
        vec!["pattern"],
    );
    let description = format!(
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {} matches or {}KB (whichever is hit first). Long lines are truncated to {} chars.",
        GREP_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024,
        GREP_MAX_LINE_LENGTH
    );
    AgentTool::new("grep", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_grep(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

/// Scans one file for `re`, appending `path:line: text` / `path-line- text`
/// entries. `skip_binary` mirrors rg's NUL sniff (a NUL byte in the first 8KB
/// marks the file binary; only during directory traversal — explicitly-given
/// files are always searched). Returns false when the match limit is reached.
fn grep_search_file(
    path: &str,
    rel: &str,
    skip_binary: bool,
    re: &regex::Regex,
    ctx_lines: usize,
    limit: usize,
    abort: &AbortSignal,
    match_lines: &mut Vec<String>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    lines_truncated: &mut bool,
) -> bool {
    if abort.is_aborted() {
        return false;
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return true,
    };
    if abort.is_aborted() {
        return false;
    }
    if skip_binary {
        let window = &data[..data.len().min(8 * 1024)];
        if window.contains(&0u8) {
            return true;
        }
    }
    // pi normalizes \r\n and bare \r to \n before splitting.
    let content = String::from_utf8_lossy(&data)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = content.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if abort.is_aborted() {
            return false;
        }
        if *match_count >= limit {
            *match_limit_reached = true;
            return false;
        }
        if re.is_match(line) {
            *match_count += 1;
            let start = if ctx_lines <= 0 { i } else { i.saturating_sub(ctx_lines) };
            let end = if ctx_lines <= 0 {
                i
            } else {
                (i + ctx_lines).min(lines.len().saturating_sub(1))
            };
            for j in start..=end {
                let (text, was) = truncate_line(lines[j], 0);
                if was {
                    *lines_truncated = true;
                }
                if j == i {
                    match_lines.push(format!("{rel}:{}: {text}", j + 1));
                } else {
                    match_lines.push(format!("{rel}-{}- {text}", j + 1));
                }
            }
            if *match_count >= limit {
                *match_limit_reached = true;
                return false;
            }
        }
    }
    true
}

fn run_grep(workspace: &crate::WorkspaceRoots, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern_str = arg_str(&args, "pattern");
    let root = match args.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(p) => resolve_scoped_path(p, workspace)?,
        None => workspace.cwd().to_string_lossy().into_owned(),
    };
    let glob_pat = arg_str(&args, "glob");
    // pi: Math.max(1, limit ?? 100) — non-positive limits clamp to 1.
    let limit = arg_int(&args, "limit")?
        .map(|l| l.max(1) as usize)
        .unwrap_or(GREP_DEFAULT_LIMIT);
    let ctx_lines = arg_int(&args, "context")?.map(|c| c.max(0) as usize).unwrap_or(0);

    let flags = if arg_bool(&args, "ignoreCase") { "(?i)" } else { "" };
    let expr = if arg_bool(&args, "literal") {
        regex::escape(&pattern_str)
    } else {
        pattern_str.clone()
    };
    let re = regex::Regex::new(&format!("{flags}{expr}"))
        .map_err(|e| anyhow!("invalid regex: {e}"))?;

    let info = std::fs::metadata(&root).map_err(|_| anyhow!("Path not found: {root}"))?;
    check_aborted(&abort)?;
    let is_dir = info.is_dir();

    let mut match_lines = Vec::new();
    let mut match_count = 0usize;
    let mut match_limit_reached = false;
    let mut lines_truncated = false;

    if !is_dir {
        let base = Path::new(&root)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let _ = grep_search_file(
            &root, &base, false, &re, ctx_lines, limit,
            &abort, &mut match_lines, &mut match_count, &mut match_limit_reached, &mut lines_truncated,
        );
    } else {
        // rg semantics: gitignore applies only inside a git repository.
        let mut ig = IgnoreStack::new(&root, true, false);
        walk(&root, &mut ig, &mut |abs, rel, is_dir| {
            if abort.is_aborted() {
                return WalkControl::Stop;
            }
            if is_dir {
                return WalkControl::Continue;
            }
            if !glob_pat.is_empty() && !match_rg_glob(&glob_pat, rel) {
                return WalkControl::Continue;
            }
            if match_count >= limit {
                match_limit_reached = true;
                return WalkControl::Stop;
            }
            if !grep_search_file(
                abs, rel, true, &re, ctx_lines, limit,
                &abort, &mut match_lines, &mut match_count, &mut match_limit_reached, &mut lines_truncated,
            ) {
                return WalkControl::Stop;
            }
            WalkControl::Continue
        });
    }
    check_aborted(&abort)?;

    if match_count == 0 {
        return Ok(text_result("No matches found"));
    }
    let raw_output = match_lines.join("\n");
    let tr = truncate_head(&raw_output, usize::MAX, 0);
    let mut output = tr.content.clone();
    let mut details = json!({});
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(format!("{limit} matches limit reached. Use limit={} for more, or refine pattern", limit * 2));
        details["matchLimitReached"] = json!(limit);
    }
    if tr.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details["truncation"] = json!(tr);
    }
    if lines_truncated {
        notices.push(format!("Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"));
        details["linesTruncated"] = json!(true);
    }
    if !notices.is_empty() {
        output.push_str("\n\n[");
        output.push_str(&notices.join(". "));
        output.push(']');
    }
    let mut res = text_result(output);
    res.details = details;
    Ok(res)
}

// ---------------------------------------------------------------------------
// Recursive directory walker (gitignore-aware)
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum WalkControl {
    Continue,
    #[allow(dead_code)]
    SkipDir,
    Stop,
}

fn walk(root: &str, ig: &mut IgnoreStack, visit: &mut dyn FnMut(&str, &str, bool) -> WalkControl) {
    walk_inner(root, "", ig, visit);
}

fn walk_inner(
    dir_abs: &str,
    dir_rel: &str,
    ig: &mut IgnoreStack,
    visit: &mut dyn FnMut(&str, &str, bool) -> WalkControl,
) -> WalkControl {
    let entries = match std::fs::read_dir(dir_abs) {
        Ok(e) => e,
        Err(_) => return WalkControl::Continue,
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let abs = if dir_abs.ends_with('/') {
            format!("{dir_abs}{name}")
        } else {
            format!("{dir_abs}/{name}")
        };
        let rel = if dir_rel.is_empty() {
            name.clone()
        } else {
            format!("{dir_rel}/{name}")
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if ig.ignored(&abs, &rel, is_dir) {
            continue;
        }
        match visit(&abs, &rel, is_dir) {
            WalkControl::Continue => {
                if is_dir {
                    if walk_inner(&abs, &rel, ig, visit) == WalkControl::Stop {
                        return WalkControl::Stop;
                    }
                }
            }
            WalkControl::SkipDir => continue,
            WalkControl::Stop => return WalkControl::Stop,
        }
    }
    WalkControl::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-tools-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn media_details(result: &AgentToolResult) -> &[Value] {
        result.details["media"].as_array().map(Vec::as_slice).unwrap_or(&[])
    }

    fn tiny_webm() -> Vec<u8> {
        let mut bytes = vec![0x1a, 0x45, 0xdf, 0xa3];
        bytes.extend_from_slice(b"bounded-webm-fixture");
        bytes
    }

    fn noop_update() -> ToolUpdateFn {
        Arc::new(|_r: AgentToolResult| {})
    }

    fn make_ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "test".to_string(),
            arguments: args,
            on_update: noop_update(),
            abort,
            model: None,
        }
    }

    fn text_of(res: &AgentToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    fn make_ctx_with_model(args: Value, model: Option<pi_ai::Model>) -> ToolCallContext {
        let mut context = make_ctx(args);
        context.model = model;
        context
    }

    fn image_model(images: bool) -> pi_ai::Model {
        pi_ai::Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            input: if images { vec!["text".into(), "image".into()] } else { vec!["text".into()] },
            ..pi_ai::Model::default()
        }
    }

    fn tiny_png() -> Vec<u8> {
        // `::image` — the local `image` module (inspect_image) shadows the
        // extern crate name inside tools.rs.
        let image = ::image::DynamicImage::new_rgba8(2, 2);
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), ::image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn schema_type<'a>(tool: &'a AgentTool, property: &str) -> Option<&'a str> {
        tool.parameters
            .properties
            .get(property)
            .and_then(|schema| schema.schema_type.as_ref())
            .and_then(Value::as_str)
    }

    #[tokio::test]
    async fn read_text_file_basic() {
        let d = tmpdir();
        fs::write(d.join("hello.txt"), b"first line\nsecond line\nthird line\n").unwrap();
        let tool = read_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "path": "hello.txt" }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("first line"));
        assert!(text.contains("third line"));
    }

    #[tokio::test]
    async fn read_skill_uri_is_bounded_and_supports_hidden_skill() {
        let d = tmpdir();
        let base = d.join("skill");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("SKILL.md"), "secret instructions").unwrap();
        fs::write(base.join("asset.txt"), "asset body").unwrap();
        let skill = crate::Skill {
            name: "hidden".to_owned(),
            description: "hidden test skill".to_owned(),
            file_path: base.join("SKILL.md").to_string_lossy().into_owned(),
            base_dir: base.to_string_lossy().into_owned(),
            globs: Vec::new(),
            always_apply: false,
            hidden: true,
            disable_model_invocation: false,
            source: crate::SkillSource::User,
            trusted: true,
        };
        let provider: SkillSnapshotFn = Arc::new(move || vec![skill.clone()]);
        let tool = read_tool_with_skills(&d.to_string_lossy(), Some(provider));
        let result = (tool.execute)(make_ctx(json!({ "path": "skill://hidden/asset.txt" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("asset body"));
        let error = (tool.execute)(make_ctx(json!({ "path": "skill://hidden/../escape" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes its base directory"));
    }

    #[tokio::test]
    async fn read_internal_uri_resolver_returns_resolved_file_content() {
        let d = tmpdir();
        let target = d.join("agent-out.md");
        fs::write(&target, b"agent transcript body\n").unwrap();
        let target_clone = target.clone();
        let resolver: InternalUriResolverFn = Arc::new(move |_uri| Ok(target_clone.clone()));
        let tool = read_tool_with_resolver(&d.to_string_lossy(), None, Some(resolver));
        let result = (tool.execute)(make_ctx(json!({ "path": "agent://abc" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("agent transcript body"));
    }

    #[tokio::test]
    async fn read_internal_uri_absent_resolver_errors_for_recognized_scheme() {
        let d = tmpdir();
        let tool = read_tool_with_resolver(&d.to_string_lossy(), None, None);
        for (uri, scheme) in [
            ("agent://abc", "agent"),
            ("history://sess", "history"),
            ("artifact://42", "artifact"),
        ] {
            let err = (tool.execute)(make_ctx(json!({ "path": uri })))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("{scheme}:// resolution is unavailable")), "{err}");
        }
    }

    #[tokio::test]
    async fn read_internal_uri_resolver_errors_propagate_without_fallthrough() {
        let d = tmpdir();
        let resolver: InternalUriResolverFn =
            Arc::new(|_uri| Err(anyhow::anyhow!("uri resolution failed: boom")));
        let tool = read_tool_with_resolver(&d.to_string_lossy(), None, Some(resolver));
        let err = (tool.execute)(make_ctx(json!({ "path": "agent://abc" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("uri resolution failed: boom"), "{err}");
    }

    #[tokio::test]
    async fn read_internal_uri_unknown_scheme_falls_through_to_file_path() {
        let d = tmpdir();
        fs::write(d.join("hello.txt"), b"plain file\n").unwrap();
        // A resolver that always fails must NOT be consulted for ordinary file
        // paths or unrecognized schemes: file-path resolution wins.
        let resolver: InternalUriResolverFn = Arc::new(|_uri| Err(anyhow::anyhow!("must not be called")));
        let tool = read_tool_with_resolver(&d.to_string_lossy(), None, Some(resolver));
        let result = (tool.execute)(make_ctx(json!({ "path": "hello.txt" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("plain file"));
    }

    #[tokio::test]
    async fn read_directory_returns_eisdir() {
        let d = tmpdir();
        let tool = read_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "path": "." }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert_eq!(err, "EISDIR: illegal operation on a directory, read");
    }

    #[tokio::test]
    async fn read_offset_beyond_end_errors() {
        let d = tmpdir();
        fs::write(d.join("small.txt"), b"a\nb\nc\n").unwrap();
        let tool = read_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "path": "small.txt", "offset": 99 }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("beyond end of file"));
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn pdftotext_available() -> bool {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[tokio::test]
    async fn read_pdf_extracts_text() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not available");
            return;
        }
        let fixture = fixture_path("sample.pdf");
        assert!(fixture.is_file(), "missing fixture {}", fixture.display());
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy() }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        for i in 1..=6 {
            assert!(
                text.contains(&format!("Line {i} of PDF fixture")),
                "missing extracted line {i} in: {text}"
            );
        }
    }

    #[tokio::test]
    async fn read_pdf_offset_limit_paginates() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not available");
            return;
        }
        let fixture = fixture_path("sample.pdf");
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy(), "offset": 3, "limit": 2 }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("Line 3 of PDF fixture"), "got: {text}");
        assert!(text.contains("Line 4 of PDF fixture"), "got: {text}");
        assert!(!text.contains("Line 2 of PDF fixture"), "got: {text}");
        assert!(!text.contains("Line 5 of PDF fixture"), "got: {text}");
        // The extracted text is 6 lines each terminated by '\n', so the line
        // total includes the trailing empty split element (same convention as
        // text files); offset=3 limit=2 consumes lines 3-4 of 7.
        assert!(text.contains("[3 more lines in file. Use offset=5 to continue.]"), "got: {text}");
    }

    #[tokio::test]
    async fn read_pdf_offset_beyond_end_errors() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not available");
            return;
        }
        let fixture = fixture_path("sample.pdf");
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy(), "offset": 99 }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("Offset 99 is beyond end of file"), "got: {err}");
    }

    #[tokio::test]
    async fn read_pdf_without_text_layer_notices() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not available");
            return;
        }
        let fixture = fixture_path("no-text.pdf");
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy() }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(
            err.contains("PDF contains no extractable text (scanned or encrypted)"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn extract_pdf_missing_binary_is_actionable() {
        let err = extract_pdf_text_with(
            "pdftotext-definitely-missing-xyz",
            "/nonexistent/sample.pdf",
            make_ctx(json!({})).abort,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("poppler-utils"), "got: {err}");
        assert!(err.contains("pdftotext"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_pdf_abort_cancels_extraction() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not available");
            return;
        }
        let fixture = fixture_path("sample.pdf");
        let (ctrl, abort) = pi_agent::AbortController::new();
        ctrl.abort();
        let err = extract_pdf_text(fixture.to_str().unwrap(), abort)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("PDF text extraction cancelled"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_pdf_caps_oversized_stdout() {
        // A reader larger than the bound: copy_capped must stop at the cap and
        // report truncation (no unbounded accumulation).
        let oversized = vec![b'x'; PDF_EXTRACT_MAX_BYTES + 4096];
        let mut reader = oversized.as_slice();
        let mut out = Vec::new();
        let capped = copy_capped(&mut reader, &mut out, PDF_EXTRACT_MAX_BYTES)
            .await
            .expect("in-memory read cannot fail");
        assert!(capped, "oversized reader must hit the cap");
        assert_eq!(out.len(), PDF_EXTRACT_MAX_BYTES, "output must be exactly the cap");
        assert!(out.iter().all(|&b| b == b'x'));

        // The extraction path attaches a leading notice so the truncation is
        // visible in the head-truncated result.
        let notice = apply_pdf_cap_notice(String::new(), true);
        assert!(
            notice.starts_with("[PDF text output capped at 32.0MB]"),
            "cap notice must name the bound: {notice:?}"
        );
        assert_eq!(apply_pdf_cap_notice("body".to_owned(), false), "body");
    }

    #[tokio::test]
    async fn extract_pdf_under_cap_passes_through() {
        let small = b"small pdf text\n".to_vec();
        let mut reader = small.as_slice();
        let mut out = Vec::new();
        let capped = copy_capped(&mut reader, &mut out, PDF_EXTRACT_MAX_BYTES)
            .await
            .expect("in-memory read cannot fail");
        assert!(!capped, "small reader must not hit the cap");
        assert_eq!(out, small, "under-cap output must pass through untouched");
    }

    /// Regression for the wait/stdout-reader race: a child that closes stdout
    /// (EOF) while still running must NOT be killed just because the uncapped
    /// reader finished before `wait`. The old code treated any reader
    /// completion as a "cap" kill, racing a legitimate exit into a signal
    /// death and masking a genuine non-zero pdftotext failure as "no
    /// extractable text". The fix reaps the real exit status on EOF, so a
    /// non-zero exit stays actionable. This would fail on the old code (which
    /// returned "PDF contains no extractable text" for the case below).
    #[cfg(unix)]
    #[tokio::test]
    async fn extract_pdf_eof_before_exit_keeps_nonzero_actionable() {
        let dir = tmpdir();
        let script = dir.join("fake-pdftotext.sh");
        fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                // Close stdout (the pipe the helper reads) so the uncapped
                // reader hits EOF while the child is still alive — the exact
                // race window. Then linger so `wait` is not ready yet, and exit
                // nonzero to prove the real status is preserved.
                "exec 1>&-\n",
                // Keep the linger inside the shell so the observed status is
                // always the fake pdftotext leader's own exit status.
                "i=0; while [ $i -lt 200000 ]; do i=$((i + 1)); done\n",
                "exit 1\n",
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let err = extract_pdf_text_with(
            script.to_str().unwrap(),
            "/nonexistent/no-text.pdf",
            make_ctx(json!({})).abort,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("pdftotext failed") && err.contains("exit status 1"),
            "EOF race must preserve the nonzero exit, got: {err}"
        );
        assert!(
            !err.contains("PDF contains no extractable text"),
            "EOF race must not mask a nonzero failure as empty, got: {err}"
        );
    }

    /// Whether any Office converter (pandoc or a working LibreOffice) exists on
    /// this host, so the end-to-end read path can run at all. Per-converter
    /// guards live in `tools/doc_convert.rs`; this probes the real binaries the
    /// read tool would spawn.
    fn office_converter_available() -> bool {
        let probe = |bin: &str| {
            std::process::Command::new(bin)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        probe("pandoc") || probe("libreoffice")
    }

    fn nbconvert_available() -> bool {
        std::process::Command::new("jupyter")
            .args(["nbconvert", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn read_docx_converts_to_text() {
        if !office_converter_available() {
            eprintln!("skipping: neither pandoc nor libreoffice available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        assert!(fixture.is_file(), "missing fixture {}", fixture.display());
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy() }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        for i in 1..=6 {
            assert!(
                text.contains(&format!("Line {i} of docx fixture")),
                "missing extracted line {i} in: {text}"
            );
        }
    }

    #[tokio::test]
    async fn read_docx_offset_paginates_converted_text() {
        if !office_converter_available() {
            eprintln!("skipping: neither pandoc nor libreoffice available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy(), "offset": 1, "limit": 2 }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(
            text.contains("Line 1 of docx fixture") || text.contains("Line 2 of docx fixture"),
            "offset/limit must select the head of the converted text, got: {text}"
        );
        assert!(text.contains("more lines in file"), "got: {text}");
    }

    #[tokio::test]
    async fn read_ipynb_converts_to_text() {
        if !nbconvert_available() {
            eprintln!("skipping: jupyter nbconvert not available");
            return;
        }
        let fixture = fixture_path("sample.ipynb");
        assert!(fixture.is_file(), "missing fixture {}", fixture.display());
        let tool = read_tool(&fixture.parent().unwrap().to_string_lossy());
        let c = make_ctx(json!({ "path": fixture.to_string_lossy() }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("answer = 40 + 2"), "missing code cell in: {text}");
        assert!(text.contains("print(answer)"), "missing code cell in: {text}");
    }

    #[tokio::test]
    async fn read_with_limit_and_continuation() {
        let d = tmpdir();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        fs::write(d.join("big.txt"), content).unwrap();
        let tool = read_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "path": "big.txt", "limit": 3 }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("line0"));
        assert!(text.contains("line2"));
        assert!(text.contains("more lines in file"));
    }

    #[tokio::test]
    async fn read_image_appends_non_vision_note_from_current_model() {
        let d = tmpdir();
        fs::write(d.join("image.png"), tiny_png()).unwrap();
        let tool = read_tool(&d.to_string_lossy());

        let none = (tool.execute)(make_ctx(json!({ "path": "image.png" }))).await.unwrap();
        assert!(!text_of(&none).contains(NON_VISION_IMAGE_NOTE));

        let text_only = (tool.execute)(make_ctx_with_model(
            json!({ "path": "image.png" }),
            Some(image_model(false)),
        ))
        .await
        .unwrap();
        assert!(text_of(&text_only).contains(NON_VISION_IMAGE_NOTE));

        let vision = (tool.execute)(make_ctx_with_model(
            json!({ "path": "image.png" }),
            Some(image_model(true)),
        ))
        .await
        .unwrap();
        assert!(!text_of(&vision).contains(NON_VISION_IMAGE_NOTE));
        assert!(vision.content.iter().any(|content| matches!(content, ContentBlock::Image { .. })));
    }
    #[tokio::test]
    async fn read_video_returns_bounded_structured_media() {
        let cwd = tmpdir();
        fs::write(cwd.join("capture.webm"), tiny_webm()).unwrap();
        let tool = read_tool(&cwd.to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({ "path": "capture.webm" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("Read video file [video/webm] capture.webm"));
        let media = media_details(&result);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["kind"], "video");
        assert_eq!(media[0]["mimeType"], "video/webm");
        assert_eq!(media[0]["name"], "capture.webm");
        assert!(media[0]["data"].as_str().unwrap().len() <= MAX_INLINE_VIDEO_BASE64_BYTES);
    }

    #[tokio::test]
    async fn read_video_rejects_hostile_content_and_oversize() {
        let cwd = tmpdir();
        fs::write(cwd.join("fake.webm"), b"not webm").unwrap();
        let tool = read_tool(&cwd.to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({ "path": "fake.webm" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("extension does not match"), "{error}");

        let mut audio_ogg = b"OggS".to_vec();
        audio_ogg.extend_from_slice(b"\x01vorbis-audio-only");
        fs::write(cwd.join("audio.ogg"), audio_ogg).unwrap();
        let error = (tool.execute)(make_ctx(json!({ "path": "audio.ogg" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("extension does not match"), "{error}");

        let oversized = cwd.join("large.mp4");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_INLINE_VIDEO_BYTES + 1).unwrap();
        let error = (tool.execute)(make_ctx(json!({ "path": "large.mp4" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Web preview limit"), "{error}");
    }

    #[tokio::test]
    async fn read_media_rejects_outside_workspace_paths() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("image.png"), tiny_png()).unwrap();
        fs::write(outside.path().join("capture.webm"), tiny_webm()).unwrap();
        let tool = read_tool(&cwd.path().to_string_lossy());
        for path in [outside.path().join("image.png"), outside.path().join("capture.webm")] {
            let error = (tool.execute)(make_ctx(json!({ "path": path })))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("escapes working directory"), "{error}");
        }
    }

    #[tokio::test]
    async fn read_trusted_uri_image_preserves_resolver_contract() {
        let cwd = tempfile::tempdir().unwrap();
        let resolved = tempfile::NamedTempFile::new().unwrap();
        fs::write(resolved.path(), tiny_png()).unwrap();
        let resolved_path = resolved.path().to_path_buf();
        let resolver: InternalUriResolverFn = Arc::new(move |_uri| Ok(resolved_path.clone()));
        let tool = read_tool_with_resolver(&cwd.path().to_string_lossy(), None, Some(resolver));
        let result = (tool.execute)(make_ctx(json!({ "path": "artifact://image" })))
            .await
            .unwrap();
        assert!(result.content.iter().any(|block| matches!(block, ContentBlock::Image { .. })));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_media_rejects_symlink_escape() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let image = outside.path().join("image.png");
        let video = outside.path().join("capture.webm");
        fs::write(&image, tiny_png()).unwrap();
        fs::write(&video, tiny_webm()).unwrap();
        std::os::unix::fs::symlink(&image, cwd.path().join("image.png")).unwrap();
        std::os::unix::fs::symlink(&video, cwd.path().join("capture.webm")).unwrap();
        let tool = read_tool(&cwd.path().to_string_lossy());
        for path in ["image.png", "capture.webm"] {
            let error = (tool.execute)(make_ctx(json!({ "path": path })))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("escapes working directory"), "{error}");
        }
    }

    #[tokio::test]
    async fn numeric_schemas_accept_floats_and_execution_rejects_unsafe_ranges() {
        let d = tmpdir();
        fs::write(d.join("lines.txt"), b"a\nb\nc\nd\n").unwrap();
        fs::write(d.join("grep.txt"), b"zero\nmatch\nafter\n").unwrap();

        let read = read_tool(&d.to_string_lossy());
        let ls = ls_tool(&d.to_string_lossy());
        let find = find_tool(&d.to_string_lossy());
        let grep = grep_tool(&d.to_string_lossy());
        for (tool, properties) in [
            (&read, &["offset", "limit"][..]),
            (&ls, &["limit"][..]),
            (&find, &["limit"][..]),
            (&grep, &["context", "limit"][..]),
        ] {
            for property in properties {
                assert_eq!(schema_type(tool, property), Some("number"), "{}.{}", tool.name, property);
            }
        }

        let read_float = (read.execute)(make_ctx(json!({ "path": "lines.txt", "offset": 2.9, "limit": 1.9 })))
            .await
            .unwrap();
        assert!(text_of(&read_float).starts_with('b'));
        assert!(text_of(&read_float).contains("more lines"));

        let grep_clamped = (grep.execute)(make_ctx(json!({
            "pattern": "match",
            "path": "grep.txt",
            "context": -3.5,
            "limit": 1.9
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&grep_clamped).lines().next(), Some("grep.txt:2: match"));

        let unsafe_number = JS_MAX_SAFE_INTEGER + 2.0;
        let error = (read.execute)(make_ctx(json!({ "path": "lines.txt", "offset": unsafe_number })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("finite safe number"), "{error}");
    }

    #[tokio::test]
    async fn edit_details_preserve_normalized_line_endings_and_bom() {
        let d = tmpdir();
        fs::write(d.join("windows.txt"), "\u{FEFF}alpha\r\nbeta\r\ngamma\r\n".as_bytes()).unwrap();
        let tool = edit_tool(&d.to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "path": "windows.txt",
            "edits": [{ "oldText": "beta", "newText": "BETA" }]
        })))
        .await
        .unwrap();

        assert_eq!(result.details["firstChangedLine"], json!(2));
        assert!(result.details["diff"].as_str().unwrap().contains("-2 beta"));
        assert!(result.details["diff"].as_str().unwrap().contains("+2 BETA"));
        let patch = result.details["patch"].as_str().unwrap();
        assert!(patch.starts_with("--- windows.txt\n+++ windows.txt\n@@"), "{patch}");
        assert!(!patch.contains('\r'));
        assert_eq!(
            fs::read(d.join("windows.txt")).unwrap(),
            "\u{FEFF}alpha\r\nBETA\r\ngamma\r\n".as_bytes()
        );
    }
    #[tokio::test]
    async fn todo_tool_preserves_typed_details_for_success_and_domain_errors() {
        let runtime = TodoRuntime::memory();
        let tool = create_todo_tool(runtime.clone());
        let success = (tool.execute)(make_ctx(json!({
            "op": "init",
            "list": [{ "phase": "Build", "items": ["compile"] }]
        })))
        .await
        .expect("todo success");
        assert_eq!(success.details["op"], "init");
        assert_eq!(success.details["storage"], "memory");
        assert_eq!(success.details["phases"][0]["tasks"][0]["content"], "compile");
        assert!(success.details["phases"][0]["tasks"][0]["id"].as_str().is_some_and(|id| id.starts_with("task-")));
        assert_eq!(success.details["phases"][0]["tasks"][0]["dependsOn"], json!([]));
        assert_eq!(success.details["phases"][0]["tasks"][0]["ready"], true);
        assert_eq!(success.details["phases"][0]["tasks"][0]["blockedBy"], json!([]));
        assert!(success.details.get(TODO_ERROR_MARKER).is_none());

        let failure = (tool.execute)(make_ctx(json!({
            "op": "append",
            "phase": "Build",
            "items": ["compile"]
        })))
        .await
        .expect("typed todo domain error");
        assert_eq!(failure.details["op"], "append");
        assert_eq!(failure.details["storage"], "memory");
        assert_eq!(failure.details["phases"][0]["tasks"][0]["content"], "compile");
        assert_eq!(failure.details[TODO_ERROR_MARKER], true);
        assert!(!failure.terminate);
        assert!(runtime.reminder_pending());

    }
    #[test]
    fn todo_schema_is_valid_for_openai_strict_tools() {
        let tool = create_todo_tool(TodoRuntime::memory());
        let schema = serde_json::to_value(&tool.parameters).expect("todo schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"].as_object().expect("todo properties");
        let required = schema["required"].as_array().expect("todo required");
        assert_eq!(required.len(), properties.len());
        for name in properties.keys() {
            assert!(required.iter().any(|item| item == name));
        }
        let prepared = tool.prepare_arguments.as_ref().expect("todo preparation")(json!({"op":"view"})).expect("prepare todo");
        assert!(tool.parameters.validate(&prepared).is_ok());
    }


    #[tokio::test]
    async fn read_ls_find_and_grep_observe_pre_aborted_contexts() {
        let d = tmpdir();
        fs::write(d.join("a.txt"), b"needle\n").unwrap();
        for (tool, args) in [
            (read_tool(&d.to_string_lossy()), json!({ "path": "a.txt" })),
            (ls_tool(&d.to_string_lossy()), json!({})),
            (find_tool(&d.to_string_lossy()), json!({ "pattern": "*.txt" })),
            (grep_tool(&d.to_string_lossy()), json!({ "pattern": "needle" })),
        ] {
            let (controller, abort) = pi_agent::AbortController::new();
            controller.abort();
            let context = ToolCallContext {
                tool_call_id: "cancelled".to_string(),
                arguments: args,
                on_update: noop_update(),
                abort,
                model: None,
            };
            let error = (tool.execute)(context).await.unwrap_err().to_string();
            assert_eq!(error, "Operation aborted", "{}", tool.name);
        }
    }

    #[test]
    fn factories_expose_read_only_and_name_keyed_surfaces() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert_eq!(
            create_read_only_tools(&cwd).into_iter().map(|tool| tool.name).collect::<Vec<_>>(),
            ["read", "grep", "find", "glob", "ls", "web_search", "ast_grep"]
        );
        assert_eq!(create_tool("glob", &cwd).unwrap().name, "glob");
        assert_eq!(create_tool("lsp", &cwd).unwrap().name, "lsp");
        assert!(create_tool("ast", &cwd).is_err());
        assert_eq!(create_read_only_tool_definitions(&cwd).len(), 7);
        let tools = create_all_tools_by_name(&cwd);
        assert_eq!(tools.len(), TOOL_NAMES.len());
        for name in TOOL_NAMES {
            assert_eq!(tools.get(*name).map(|tool| tool.name.as_str()), Some(*name));
        }
        let definitions = create_all_tool_definitions_by_name(&cwd);
        assert_eq!(definitions.len(), TOOL_NAMES.len());
        assert_eq!(create_tool_definition("read", &cwd).unwrap().name, "read");
        assert_eq!(create_coding_tool_definitions(&cwd).len(), 6);
        assert_eq!(create_read_only_tool_definitions(&cwd).len(), 7);
        assert_eq!(create_all_tools_map(&cwd).len(), TOOL_NAMES.len());
        assert_eq!(create_all_tool_definitions(&cwd).len(), TOOL_NAMES.len());
        assert_eq!(create_all_tool_definitions_map(&cwd).len(), TOOL_NAMES.len());
    }

    #[test]
    fn builtin_tools_carry_explicit_capabilities() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        for (name, expected) in [
            ("read", ToolCapability::Read),
            ("inspect_image", ToolCapability::Read),
            ("grep", ToolCapability::Read),
            ("find", ToolCapability::Read),
            ("glob", ToolCapability::Read),
            ("ls", ToolCapability::Read),
            ("web_search", ToolCapability::Read),
            ("ast_grep", ToolCapability::Read),
            ("write", ToolCapability::Write),
            ("edit", ToolCapability::Write),
            ("ast_edit", ToolCapability::Write),
            ("todo", ToolCapability::Write),
            ("lsp", ToolCapability::Write),
            ("github", ToolCapability::Write),
            ("memory", ToolCapability::Write),
            ("bash", ToolCapability::Exec),
            ("browser", ToolCapability::Exec),
        ] {
            let tool = create_tool(name, &cwd).expect("built-in tool");
            assert_eq!(tool.capability, expected, "{name}");
        }
    }

    #[test]
    fn inspect_image_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"inspect_image"));
        assert!(
            tool_snippet("inspect_image").is_some_and(|s| s.contains("metadata") && s.contains("dominant colors"))
        );
        let tool = create_tool("inspect_image", &cwd).expect("inspect_image tool builds");
        assert_eq!(tool.name, "inspect_image");
        assert_eq!(tool.capability, ToolCapability::Read);
        assert_eq!(tool.parameters.required, vec!["path".to_string()]);
        assert_eq!(schema_type(&tool, "path"), Some("string"));
    }

    #[test]
    fn browser_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"browser"));
        assert_eq!(tool_snippet("browser"), Some("Automate a headless Chrome/Chromium browser: navigate, click, fill, screenshot, extract, list_tabs, close"));
        let tool = create_tool("browser", &cwd).expect("browser tool builds");
        assert_eq!(tool.name, "browser");
        assert_eq!(tool.capability, ToolCapability::Exec);
        let action = tool.parameters.properties.get("action").expect("action prop");
        assert_eq!(
            action.schema_type.as_ref().and_then(Value::as_str),
            Some("string")
        );
        assert_eq!(tool.parameters.required, vec!["action"]);
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "browser");
    }

    #[test]
    fn github_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"github"));
        assert!(tool_snippet("github").is_some_and(|s| s.contains("GitHub API")));
        let tool = create_tool("github", &cwd).expect("github tool builds");
        assert_eq!(tool.name, "github");
        assert_eq!(tool.capability, ToolCapability::Write);
        // Every action is enumerated in the schema's action enum.
        let action = tool.parameters.properties.get("action").expect("action prop");
        let actions: Vec<&str> = action
            .enum_values
            .iter()
            .map(|v| v.as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            actions,
            [
                "search_issues",
                "get_issue",
                "list_issues",
                "create_issue",
                "comment_issue",
                "list_prs",
                "get_pr",
                "list_commits",
                "view_file",
                "search_code"
            ]
        );
        assert!(tool.parameters.required.contains(&"action".to_string()));
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "github");
    }

    #[test]
    fn memory_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"memory"));
        assert!(
            tool_snippet("memory")
                .is_some_and(|s| s.contains("learn") && s.contains("recall") && s.contains("forget"))
        );
        let tool = create_tool("memory", &cwd).expect("memory tool builds");
        assert_eq!(tool.name, "memory");
        assert_eq!(tool.capability, ToolCapability::Write);
        // Every action is enumerated in the schema's action enum.
        let op = tool.parameters.properties.get("op").expect("op prop");
        let actions: Vec<&str> = op
            .enum_values
            .iter()
            .map(|v| v.as_str().unwrap_or(""))
            .collect();
        assert_eq!(actions, ["learn", "recall", "list", "forget"]);
        assert!(tool.parameters.required.contains(&"op".to_string()));
        for prop in ["content", "tags", "query", "limit", "tag", "id"] {
            assert!(tool.parameters.properties.contains_key(prop), "missing {prop} prop");
        }
        // The default factory stores under the agent dir without touching disk.
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "memory");
    }

    #[test]
    fn hindsight_tools_are_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        for name in ["recall", "retain", "reflect"] {
            assert!(TOOL_NAMES.contains(&name), "{name} in TOOL_NAMES");
            assert!(tool_snippet(name).is_some(), "{name} snippet");
            let tool = create_tool(name, &cwd).expect("tool builds");
            assert_eq!(tool.name, name);
            // Serializes as a ToolDefinition (used by agent providers).
            assert_eq!(tool.as_tool_definition().name, name);
        }
        let recall = create_tool("recall", &cwd).expect("recall tool builds");
        assert_eq!(recall.capability, ToolCapability::Read);
        assert!(recall.parameters.required.contains(&"query".to_string()));
        assert!(recall.parameters.properties.contains_key("query"));
        let retain = create_tool("retain", &cwd).expect("retain tool builds");
        assert_eq!(retain.capability, ToolCapability::Write);
        assert!(retain.parameters.required.contains(&"content".to_string()));
        let reflect = create_tool("reflect", &cwd).expect("reflect tool builds");
        assert_eq!(reflect.capability, ToolCapability::Read);
        assert!(reflect.parameters.required.contains(&"query".to_string()));
    }
    #[test]
    fn ordinary_and_persona_child_factories_use_selected_nondefault_hindsight_config() {
        let cwd = tmpdir();
        let persona_root = tmpdir();
        let config = MemoryConfig {
            backend: crate::MemoryBackend::Hindsight,
            hindsight_api_url: Some("https://memory.example.test/base".to_owned()),
            hindsight_bank_id: "review-bank".to_owned(),
            hindsight_bank_id_prefix: Some("team".to_owned()),
            ..Default::default()
        };
        let resolver: MemoryConfigFn = Arc::new(move || Some(config.clone()));
        let ordinary = create_coding_tools_with_context_and_resolver(
            &cwd.to_string_lossy(), None, None, None, None, None, Some(resolver.clone()), None,
        );
        let persona = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(), &persona_root, None, None, None, None, None, Some(resolver), None,
        );
        for tools in [ordinary, persona] {
            let names = tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>();
            assert!(!names.iter().any(|name| name == "memory"), "{names:?}");
            for expected in ["recall", "retain", "reflect"] {
                assert!(names.iter().any(|name| name == expected), "{names:?}");
            }
        }
    }

    #[test]
    fn resolver_factories_include_one_browser_without_duplicate_names() {
        let cwd = tmpdir();
        let persona_root = tmpdir();
        let workspace = crate::WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).unwrap();
        let factories = [
            create_coding_tools_with_context_and_resolver(
                &cwd.to_string_lossy(), None, None, None, None, None, None, None,
            ),
            create_coding_tools_with_context_and_resolver_for_persona(
                &cwd.to_string_lossy(), &persona_root, None, None, None, None, None, None, None,
            ),
            create_coding_tools_for_workspace_with_context_and_resolver(
                workspace, None, None, None, None, None, None, None,
            ),
        ];
        for tools in factories {
            let names = tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>();
            assert_eq!(names.iter().filter(|name| name.as_str() == "browser").count(), 1, "{names:?}");
            let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
            assert_eq!(unique.len(), names.len(), "{names:?}");
        }
    }

    #[test]
    fn persona_child_factory_preserves_local_persona_scope_and_off_selection() {
        let cwd = tmpdir();
        let persona_root = tmpdir();
        let local: MemoryConfigFn = Arc::new(|| Some(MemoryConfig::default()));
        let local_tools = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(), &persona_root, None, None, None, None, None, Some(local), None,
        );
        assert!(local_tools.iter().any(|tool| tool.name == "memory"));
        let off: MemoryConfigFn = Arc::new(|| Some(MemoryConfig {
            backend: crate::MemoryBackend::Off,
            ..Default::default()
        }));
        let off_tools = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(), &persona_root, None, None, None, None, None, Some(off), None,
        );
        assert!(!off_tools.iter().any(|tool| matches!(tool.name.as_str(), "memory" | "recall" | "retain" | "reflect")));
    }

    #[test]
    fn mcp_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"mcp"));
        assert!(
            tool_snippet("mcp")
                .is_some_and(|s| s.contains("list_servers") && s.contains("list_tools") && s.contains("call"))
        );
        let tool = create_tool("mcp", &cwd).expect("mcp tool builds");
        assert_eq!(tool.name, "mcp");
        assert_eq!(tool.capability, ToolCapability::Write);
        for prop in ["action", "server", "tool", "args"] {
            assert!(tool.parameters.properties.contains_key(prop), "missing {prop} prop");
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "mcp");
    }

    #[test]
    fn debug_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"debug"));
        assert!(
            tool_snippet("debug").is_some_and(|s| {
                s.contains("launch") && s.contains("set_breakpoint") && s.contains("continue_")
            })
        );
        let tool = create_tool("debug", &cwd).expect("debug tool builds");
        assert_eq!(tool.name, "debug");
        assert_eq!(tool.capability, ToolCapability::Write);
        for prop in [
            "action",
            "adapter",
            "program",
            "file",
            "line",
            "variables_reference",
            "expression",
        ] {
            assert!(tool.parameters.properties.contains_key(prop), "missing {prop} prop");
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "debug");
    }

    #[test]
    fn eval_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"eval"));
        assert!(
            tool_snippet("eval")
                .is_some_and(|s| s.contains("python") && s.contains("QuickJS") && s.contains("syntax"))
        );
        let tool = create_tool("eval", &cwd).expect("eval tool builds");
        assert_eq!(tool.name, "eval");
        assert_eq!(tool.capability, ToolCapability::Exec);
        for prop in ["language", "code", "timeout"] {
            assert!(tool.parameters.properties.contains_key(prop), "missing {prop} prop");
        }
        assert!(tool.parameters.required.contains(&"language".to_string()));
        assert!(tool.parameters.required.contains(&"code".to_string()));
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "eval");
    }

    #[test]
    fn notebook_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let cwd = cwd.to_string_lossy();
        assert!(TOOL_NAMES.contains(&"notebook"));
        assert!(
            tool_snippet("notebook")
                .is_some_and(|s| s.contains("ipynb") && s.contains("execute") && s.contains("edit"))
        );
        let tool = create_tool("notebook", &cwd).expect("notebook tool builds");
        assert_eq!(tool.name, "notebook");
        assert_eq!(tool.capability, ToolCapability::Exec);
        for prop in ["action", "path", "cell", "write", "cell_type", "source", "timeout"] {
            assert!(tool.parameters.properties.contains_key(prop), "missing {prop} prop");
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        assert!(tool.parameters.required.contains(&"path".to_string()));
        // Serializes as a ToolDefinition (used by agent providers).
        let definition = tool.as_tool_definition();
        assert_eq!(definition.name, "notebook");
    }

    #[tokio::test]
    async fn read_allows_absolute_and_parent_relative_external_files() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let external = root.path().join("outside.txt");
        fs::write(&external, "external-bytes").expect("external file");
        let tool = read_tool(&cwd.to_string_lossy());

        let absolute = (tool.execute)(make_ctx(json!({ "path": external })))
            .await
            .expect("absolute external read");
        assert_eq!(text_of(&absolute), "external-bytes");

        let relative = (tool.execute)(make_ctx(json!({ "path": "../outside.txt" })))
            .await
            .expect("parent-relative external read");
        assert_eq!(text_of(&relative), "external-bytes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_allows_symlink_to_external_file() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        let target = external.path().join("secret.txt");
        fs::write(&target, "linked-secret").expect("secret");
        std::os::unix::fs::symlink(&target, cwd.path().join("alias.txt")).expect("symlink");
        let tool = read_tool(&cwd.path().to_string_lossy());

        let result = (tool.execute)(make_ctx(json!({ "path": "alias.txt" })))
            .await
            .expect("symlink external read");
        assert_eq!(text_of(&result), "linked-secret");
    }

    #[tokio::test]
    async fn write_and_edit_allow_external_absolute_and_parent_relative_paths() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        let added = root.path().join("added");
        let external = root.path().join("external");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&added).expect("added root");
        fs::create_dir_all(&external).expect("external directory");
        let workspace = crate::WorkspaceRoots::new(&cwd, [&added]).expect("workspace");
        let tools = create_coding_tools_for_workspace_with_context_and_resolver(
            workspace, None, None, None, None, None, None, None,
        );
        let write = tools
            .iter()
            .find(|tool| tool.name == "write")
            .expect("write tool");
        let edit = tools
            .iter()
            .find(|tool| tool.name == "edit")
            .expect("edit tool");

        let absolute = external.join("absolute.txt");
        (write.execute)(make_ctx(json!({
            "path": absolute,
            "content": "absolute-before"
        })))
        .await
        .expect("write absolute external file");
        (edit.execute)(make_ctx(json!({
            "path": absolute,
            "edits": [{ "oldText": "absolute-before", "newText": "absolute-after" }]
        })))
        .await
        .expect("edit absolute external file");
        assert_eq!(fs::read_to_string(&absolute).expect("read absolute file"), "absolute-after");

        (write.execute)(make_ctx(json!({
            "path": "../parent-relative.txt",
            "content": "relative-before"
        })))
        .await
        .expect("write parent-relative external file");
        (edit.execute)(make_ctx(json!({
            "path": "../parent-relative.txt",
            "edits": [{ "oldText": "relative-before", "newText": "relative-after" }]
        })))
        .await
        .expect("edit parent-relative external file");
        assert_eq!(
            fs::read_to_string(root.path().join("parent-relative.txt"))
                .expect("read parent-relative file"),
            "relative-after"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_follow_symlinks_to_external_regular_files() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        let target = external.path().join("target.txt");
        let alias = cwd.path().join("alias.txt");
        fs::write(&target, "before").expect("target");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");
        let write = write_tool(&cwd.path().to_string_lossy());
        let edit = edit_tool(&cwd.path().to_string_lossy());

        (write.execute)(make_ctx(json!({ "path": "alias.txt", "content": "written" })))
            .await
            .expect("write through symlink");
        assert_eq!(fs::read_to_string(&target).expect("read written target"), "written");

        (edit.execute)(make_ctx(json!({
            "path": "alias.txt",
            "edits": [{ "oldText": "written", "newText": "edited" }]
        })))
        .await
        .expect("edit through symlink");
        assert_eq!(fs::read_to_string(&target).expect("read edited target"), "edited");
        assert!(alias.is_symlink());
    }

    #[tokio::test]
    async fn write_and_edit_reject_non_regular_and_invalid_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let directory = cwd.path().join("directory");
        fs::create_dir(&directory).expect("directory");
        let write = write_tool(&cwd.path().to_string_lossy());
        let edit = edit_tool(&cwd.path().to_string_lossy());

        let write_directory_error = (write.execute)(make_ctx(json!({
            "path": "directory",
            "content": "nope"
        })))
        .await
        .expect_err("write must reject directory")
        .to_string();
        assert!(write_directory_error.contains("not a regular file"), "{write_directory_error}");

        let edit_directory_error = (edit.execute)(make_ctx(json!({
            "path": "directory",
            "edits": [{ "oldText": "x", "newText": "y" }]
        })))
        .await
        .expect_err("edit must reject directory")
        .to_string();
        assert!(edit_directory_error.contains("not a regular file"), "{edit_directory_error}");

        let empty_error = (write.execute)(make_ctx(json!({ "path": "", "content": "nope" })))
            .await
            .expect_err("write must reject empty path")
            .to_string();
        assert!(empty_error.contains("must not be empty"), "{empty_error}");

        let nul_error = (edit.execute)(make_ctx(json!({
            "path": "bad\0path",
            "edits": [{ "oldText": "x", "newText": "y" }]
        })))
        .await
        .expect_err("edit must reject NUL path")
        .to_string();
        assert!(nul_error.contains("NUL byte"), "{nul_error}");
    }

    #[tokio::test]
    async fn write_creates_file_and_reports_utf16_length() {
        let d = tmpdir();
        let tool = write_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "path": "sub/out.txt", "content": "hello" }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "Successfully wrote 5 bytes to sub/out.txt");
        assert_eq!(fs::read_to_string(d.join("sub/out.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn write_reports_utf16_length_for_astral() {
        let d = tmpdir();
        let tool = write_tool(&d.to_string_lossy());
        // U+1F600 → 2 UTF-16 code units (pi reports JS `.length`).
        let c = make_ctx(json!({ "path": "emoji.txt", "content": "\u{1F600}" }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "Successfully wrote 2 bytes to emoji.txt");
    }

    #[tokio::test]
    async fn write_missing_required_args_error_without_creating_files() {
        let d = tmpdir();
        let tool = write_tool(&d.to_string_lossy());

        let missing_content = (tool.execute)(make_ctx(json!({ "path": "m.txt" })))
            .await
            .expect_err("missing content must fail actionably")
            .to_string();
        assert!(missing_content.contains("missing required argument"), "{missing_content}");
        assert!(missing_content.contains("content"), "{missing_content}");
        assert!(
            !d.join("m.txt").exists(),
            "no file may be written when content is missing"
        );

        let missing_path = (tool.execute)(make_ctx(json!({ "content": "x" })))
            .await
            .expect_err("missing path must fail actionably")
            .to_string();
        assert!(missing_path.contains("missing required argument"), "{missing_path}");
        assert!(missing_path.contains("path"), "{missing_path}");
    }

    #[tokio::test]
    async fn write_explicit_empty_content_creates_empty_file() {
        let d = tmpdir();
        let tool = write_tool(&d.to_string_lossy());
        // The schema requires `content` but imposes no minLength: an explicitly
        // empty value stays valid and writes an empty file (missing is the
        // error, not empty).
        let res = (tool.execute)(make_ctx(json!({ "path": "empty.txt", "content": "" })))
            .await
            .expect("explicit empty content is schema-valid");
        assert!(
            text_of(&res).contains("Successfully wrote 0 bytes"),
            "got: {}",
            text_of(&res)
        );
        assert_eq!(fs::read(d.join("empty.txt")).unwrap(), b"");
    }

    #[tokio::test]
    async fn edit_applies_unique_replacement() {
        let d = tmpdir();
        fs::write(d.join("e.txt"), b"fn foo() {\n    return 1;\n}\n").unwrap();
        let tool = edit_tool(&d.to_string_lossy());
        let c = make_ctx(json!({
            "path": "e.txt",
            "edits": [{ "oldText": "return 1;", "newText": "return 2;" }]
        }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 1 block(s) in e.txt.");
        assert_eq!(fs::read_to_string(d.join("e.txt")).unwrap(), "fn foo() {\n    return 2;\n}\n");
    }

    #[tokio::test]
    async fn edit_multiple_disjoint_edits() {
        let d = tmpdir();
        fs::write(d.join("m.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let tool = edit_tool(&d.to_string_lossy());
        let c = make_ctx(json!({
            "path": "m.txt",
            "edits": [
                { "oldText": "alpha", "newText": "ALPHA" },
                { "oldText": "gamma", "newText": "GAMMA" }
            ]
        }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 2 block(s) in m.txt.");
        assert_eq!(fs::read_to_string(d.join("m.txt")).unwrap(), "ALPHA\nbeta\nGAMMA\n");
    }

    #[tokio::test]
    async fn edit_duplicate_oldtext_rejected_unchanged() {
        let d = tmpdir();
        fs::write(d.join("d.txt"), b"x\nx\n").unwrap();
        let tool = edit_tool(&d.to_string_lossy());
        let c = make_ctx(json!({
            "path": "d.txt",
            "edits": [{ "oldText": "x", "newText": "y" }]
        }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("2 occurrences"), "got: {err}");
        assert_eq!(fs::read_to_string(d.join("d.txt")).unwrap(), "x\nx\n");
    }

    #[tokio::test]
    async fn edit_legacy_oldtext_newtext_folded() {
        let d = tmpdir();
        fs::write(d.join("l.txt"), b"alpha\n").unwrap();
        let tool = edit_tool(&d.to_string_lossy());
        // prepare_arguments folds top-level oldText/newText into edits[].
        let prepared = tool
            .prepare_arguments
            .clone()
            .unwrap()(json!({ "path": "l.txt", "oldText": "alpha", "newText": "beta" }))
            .unwrap();
        let c = make_ctx(prepared);
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 1 block(s) in l.txt.");
        assert_eq!(fs::read_to_string(d.join("l.txt")).unwrap(), "beta\n");
    }

    #[tokio::test]
    async fn bash_success() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let c = make_ctx(json!({ "command": "echo hello world" }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "hello world\n");
    }

    #[tokio::test]
    async fn bash_closes_stdin_and_applies_non_interactive_environment() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let result = (tool.execute)(make_ctx(json!({
            "command": "if read line; then printf 'unexpected:%s' \"$line\"; else printf 'stdin-closed:%s:%s:%s' \"$GIT_TERMINAL_PROMPT\" \"$GH_PROMPT_DISABLED\" \"$PAGER\"; fi",
            "timeout": 2
        })))
        .await
        .expect("non-interactive bash");
        assert_eq!(text_of(&result), "stdin-closed:0:1:cat");
    }
    #[tokio::test]
    async fn bash_nonzero_exit() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let c = make_ctx(json!({ "command": "exit 7" }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("Command exited with code 7"), "got: {err}");
    }

    #[tokio::test]
    async fn bash_timeout_kills_command() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let c = make_ctx(json!({ "command": "sleep 30", "timeout": 0.5 }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("Command timed out after"), "got: {err}");
    }

    #[tokio::test]
    async fn bash_invalid_timeout_rejected() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let c = make_ctx(json!({ "command": "echo hi", "timeout": -1 }));
        let err = (tool.execute)(c).await.unwrap_err().to_string();
        assert!(err.contains("Invalid timeout"));
    }

    #[tokio::test]
    async fn bash_missing_command_errors_actionably() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let err = (tool.execute)(make_ctx(json!({})))
            .await
            .expect_err("missing command must fail, never run `bash -c ''`")
            .to_string();
        assert!(err.contains("missing required argument"), "{err}");
        assert!(err.contains("command"), "{err}");
    }

    #[tokio::test]
    async fn bash_explicit_empty_command_follows_schema() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // The bash schema requires `command` but imposes no minLength, so an
        // explicitly empty command is schema-valid: `bash -c ''` runs and
        // exits 0, rendered with the standard empty-output "(no output)"
        // contract (pi parity) instead of a required-argument error.
        let res = (tool.execute)(make_ctx(json!({ "command": "" })))
            .await
            .expect("explicit empty command is schema-valid");
        assert_eq!(text_of(&res), "(no output)");
    }

    #[tokio::test]
    async fn bash_streams_partial_update() {
        let d = tmpdir();
        let updates = Arc::new(Mutex::new(Vec::<String>::new()));
        let u = updates.clone();
        let on_update: ToolUpdateFn = Arc::new(move |r: AgentToolResult| {
            if let Some(ContentBlock::Text { text, .. }) = r.content.first() {
                u.lock().push(text.clone());
            }
        });
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let (_ctrl, abort) = pi_agent::AbortController::new();
        let ctx = ToolCallContext {
            tool_call_id: "t".to_string(),
            arguments: json!({ "command": "echo streaming-output" }),
            on_update,
            abort,
            model: None,
        };
        let _res = (tool.execute)(ctx).await.unwrap();
        let got = updates.lock();
        assert!(got.iter().any(|t| t.contains("streaming-output")), "updates: {:?}", got);
    }

    #[tokio::test]
    async fn bash_preserves_stdout_stderr_write_order() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // Sequential stdout → stderr → stdout; merged stream must keep source order.
        let c = make_ctx(json!({
            "command": "printf 'OUT1\\n'; sleep 0.05; printf 'ERR1\\n' >&2; sleep 0.05; printf 'OUT2\\n'"
        }));
        let res = (tool.execute)(c).await.unwrap();
        assert_eq!(text_of(&res), "OUT1\nERR1\nOUT2\n");
    }

    #[tokio::test]
    async fn execute_bash_success_returns_exit_code() {
        let d = tmpdir();
        let chunks = Arc::new(Mutex::new(String::new()));
        let c = chunks.clone();
        let on_chunk: Arc<dyn Fn(String) + Send + Sync> =
            Arc::new(move |s| c.lock().push_str(&s));
        let (_ctrl, abort) = pi_agent::AbortController::new();

        let res = execute_bash(&d, "echo hello", None, None, on_chunk, abort).await.unwrap();
        assert!(!res.cancelled);
        assert_eq!(res.exit_code, Some(0));
        assert!(res.output.contains("hello"));
        assert!(chunks.lock().contains("hello"), "on_chunk stream missed output");
    }

    #[tokio::test]
    async fn bash_rejects_unsupervised_detach_without_rejecting_literal_ampersands() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        for command in [
            "nohup python3 -m http.server 8765 &",
            "python3 -m http.server 8765 &",
            "setsid python3 -m http.server 8765",
            "sleep 30; disown",
        ] {
            let error = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .expect_err("detach intent must be rejected")
                .to_string();
            assert!(error.contains("background=true"), "{command}: {error}");
            assert!(error.contains("/ps"), "{command}: {error}");
        }

        for substitution in ["printf '%s' \"$(sleep 1 &)\"", "printf '%s' \"`sleep 1 &`\""] {
            let error = (tool.execute)(make_ctx(json!({ "command": substitution })))
                .await
                .expect_err("detach syntax inside shell substitution must fail closed")
                .to_string();
            assert!(error.contains("background=true"), "{substitution}: {error}");
            assert!(error.contains("/ps"), "{substitution}: {error}");
        }

        let result = (tool.execute)(make_ctx(json!({
            "command": "printf '%s\\n' 'quoted & text' escaped\\&value nohup setsid disown '$(sleep 1 &)' '$(nohup true)' '`sleep 1 &`'"
        })))
        .await
        .expect("literal ampersands and argument words remain foreground");
        assert_eq!(
            text_of(&result),
            "quoted & text\nescaped&value\nnohup\nsetsid\ndisown\n$(sleep 1 &)\n$(nohup true)\n`sleep 1 &`\n"
        );
    }

    #[test]
    fn supervised_detach_normalization_is_limited_to_simple_wrappers() {
        let simple = "nohup python3 -m http.server 8765 >/tmp/http.log 2>&1 &";
        let normalized = normalize_supervised_bash(simple, &shell_detach_analysis(simple))
            .expect("simple legacy detach syntax can become supervised");
        assert_eq!(normalized, " python3 -m http.server 8765 >/tmp/http.log 2>&1 ");

        for complex in [
            "python3 -m http.server 8765 & echo hidden",
            "setsid python3 -m http.server 8765",
            "sleep 30; disown",
        ] {
            assert!(
                normalize_supervised_bash(complex, &shell_detach_analysis(complex)).is_err(),
                "complex detach must fail closed: {complex}"
            );
        }
    }

    #[tokio::test]
    async fn bash_background_uses_supervised_manager() {
        let d = tmpdir();
        let manager = crate::ProcessManager::with_config(crate::ProcessManagerConfig {
            idle_timeout: None,
            ..crate::ProcessManagerConfig::default()
        });
        let owner = crate::ProcessOwnerId::new("bash-background-test");
        let tool = bash_tool(
            &d.to_string_lossy(),
            None,
            Some(BashProcessContext {
                manager: manager.clone(),
                owner_id: owner.clone(),
            }),
            None,
        );
        let result = (tool.execute)(make_ctx(json!({
            "command": "printf background-ok",
            "background": true
        })))
        .await
        .expect("background start");
        let id: crate::ProcessId = serde_json::from_value(result.details["id"].clone())
            .expect("opaque process id");
        manager
            .wait(&owner, &id, Some(Duration::from_secs(3)))
            .await
            .expect("background exit");
        let logs = manager
            .logs(&owner, &id, 0, None, false, None)
            .await
            .expect("background logs");
        let output = logs
            .chunks
            .iter()
            .flat_map(crate::ProcessLogChunk::bytes)
            .collect::<Vec<_>>();
        assert_eq!(output, b"background-ok");
    }

    #[tokio::test]
    async fn bash_background_rejects_pre_aborted_call_without_spawning() {
        let d = tmpdir();
        let manager = crate::ProcessManager::with_config(crate::ProcessManagerConfig {
            idle_timeout: None,
            ..crate::ProcessManagerConfig::default()
        });
        let owner = crate::ProcessOwnerId::new("bash-background-abort-test");
        let tool = bash_tool(
            &d.to_string_lossy(),
            None,
            Some(BashProcessContext {
                manager: manager.clone(),
                owner_id: owner.clone(),
            }),
            None,
        );
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let error = (tool.execute)(ToolCallContext {
            tool_call_id: "cancelled-background".to_owned(),
            arguments: json!({ "command": "sleep 30", "background": true }),
            on_update: noop_update(),
            abort,
            model: None,
        })
        .await
        .expect_err("pre-aborted call must fail");
        assert_eq!(error.to_string(), "Operation aborted");
        assert!(manager.list(&owner).is_empty());
    }

    #[tokio::test]
    async fn execute_bash_nonzero_is_ok_with_exit_code() {
        let d = tmpdir();
        let on_chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_s| {});
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        let res = execute_bash(&d, "exit 7", None, None, on_chunk, abort).await.unwrap();
        assert!(!res.cancelled);
        assert_eq!(res.exit_code, Some(7));
    }

    #[tokio::test]
    async fn execute_bash_abort_returns_cancelled() {
        let d = tmpdir();
        let on_chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_s| {});
        let (ctrl, abort) = pi_agent::AbortController::new();
        // Abort a long-running command.
        let h = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            ctrl.abort();
        });
        let res = execute_bash(&d, "sleep 30", None, None, on_chunk, abort).await.unwrap();
        h.await.unwrap();
        assert!(res.cancelled, "expected cancelled=true, got {res:?}");
        assert_eq!(res.exit_code, None);
    }

    // -----------------------------------------------------------------------
    // Embedded brush shell (default unsandboxed path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn bash_default_path_uses_embedded_brush() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // BASH_VERSION is a well-known shell variable: real bash sets it, the
        // embedded brush session skips well-known vars. Its absence proves the
        // default (unsandboxed) path executed through brush, not a /bin/bash
        // subprocess.
        let res = (tool.execute)(make_ctx(json!({
            "command": "printf 'shell=%s' \"${BASH_VERSION:-brush}\""
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "shell=brush");
    }

    #[tokio::test]
    async fn bash_brush_rebuilds_env_explicitly() {
        let d = tmpdir();
        // The session rebuilds the environment explicitly: live session
        // metadata is visible, and $PWD mirrors the working directory (the
        // subprocess path derives it the same way).
        let session_env: SessionEnvFn = Arc::new(|| {
            HashMap::from([("PI_MODEL".to_owned(), "session-value".to_owned())])
        });
        let tool = bash_tool(&d.to_string_lossy(), Some(session_env), None, None);
        let res = (tool.execute)(make_ctx(json!({
            "command": "printf 'model=%s pwd=%s' \"${PI_MODEL:-unset}\" \"$PWD\""
        })))
        .await
        .unwrap();
        assert_eq!(
            text_of(&res),
            format!("model=session-value pwd={}", d.display())
        );
    }

    #[test]
    fn bash_command_env_exports_all_five_session_keys_and_passes_through_host_env() {
        // Upstream compatibility contract: PI_SESSION_ID, PI_SESSION_FILE,
        // PI_PROVIDER, PI_MODEL, and PI_REASONING_LEVEL are exported to bash
        // children with the live session's values.
        let session_env: SessionEnvFn = Arc::new(|| {
            HashMap::from([
                ("PI_SESSION_ID".to_owned(), "sess-123".to_owned()),
                ("PI_SESSION_FILE".to_owned(), "/tmp/sess.jsonl".to_owned()),
                ("PI_PROVIDER".to_owned(), "anthropic".to_owned()),
                ("PI_MODEL".to_owned(), "claude-x".to_owned()),
                ("PI_REASONING_LEVEL".to_owned(), "high".to_owned()),
            ])
        });
        let env: HashMap<String, String> = bash_command_env(Some(&session_env)).into_iter().collect();
        for (key, value) in [
            ("PI_SESSION_ID", "sess-123"),
            ("PI_SESSION_FILE", "/tmp/sess.jsonl"),
            ("PI_PROVIDER", "anthropic"),
            ("PI_MODEL", "claude-x"),
            ("PI_REASONING_LEVEL", "high"),
        ] {
            assert_eq!(
                env.get(key).map(String::as_str),
                Some(value),
                "{key} must be exported to bash children"
            );
        }
        // Host environment passes through for everything else.
        assert!(
            env.contains_key("PATH"),
            "host environment must reach bash children"
        );
    }

    #[test]
    fn bash_command_env_scrubs_stale_host_session_keys_and_skips_empty_values() {
        // The session keys are rebuilt from the session alone: values the host
        // process inherited (e.g. an outer rpi's PI_MODEL) never leak into the
        // child, and keys the session provides as empty are omitted entirely.
        let session_env: SessionEnvFn = Arc::new(|| {
            HashMap::from([
                ("PI_MODEL".to_owned(), "session-model".to_owned()),
                ("PI_SESSION_ID".to_owned(), String::new()),
            ])
        });
        let env: HashMap<String, String> = bash_command_env(Some(&session_env)).into_iter().collect();
        assert_eq!(
            env.get("PI_MODEL").map(String::as_str),
            Some("session-model"),
            "the session value must win over any inherited host value"
        );
        for key in ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_REASONING_LEVEL"] {
            assert!(
                !env.contains_key(key),
                "{key} must be scrubbed when the session provides no non-empty value"
            );
        }
    }

    #[tokio::test]
    async fn bash_brush_rejects_host_dangerous_builtins() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // In-process execution shares the rpi process: builtins that would
        // replace/stop/mutate the host are refused with an actionable message.
        for command in ["exec sleep 0.1", "suspend", "ulimit -n", "umask 077"] {
            let err = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .expect_err("host-dangerous builtins must be refused")
                .to_string();
            assert!(
                err.contains("not supported in the embedded brush shell"),
                "{command}: {err}"
            );
        }
        // `kill` of the host pid ($$ is the rpi process in-process) is refused…
        let err = (tool.execute)(make_ctx(json!({ "command": "kill -9 $$" })))
            .await
            .expect_err("kill of the host pid must be refused")
            .to_string();
        assert!(err.contains("refusing to signal the host process"), "{err}");
        // …but legitimate kill uses pass through the guarded builtin.
        let res = (tool.execute)(make_ctx(json!({ "command": "kill -l" })))
            .await
            .expect("kill -l passes through the guarded builtin");
        assert!(text_of(&res).contains("SIGTERM"), "{}", text_of(&res));
        // `exec` inside a subshell still works (brush spawns a child there).
        let res = (tool.execute)(make_ctx(json!({ "command": "(exec echo subshell-ok)" })))
            .await
            .expect("subshell exec passes through");
        assert_eq!(text_of(&res), "subshell-ok\n");
    }

    #[tokio::test]
    async fn bash_brush_kill_guard_checks_every_numeric_target() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // A multi-target kill that names the host pid anywhere in the list
        // must be refused: brush's kill applies the signal to ALL listed
        // targets, so a first-pid-only scan (`kill 1234 $$`) would delegate
        // and then signal the rpi process itself.
        for command in ["kill 1234 $$", "kill -9 1234 $$", "kill $$ 1234", "kill 1234 5678 $$"] {
            let err = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .expect_err("a multi-target kill naming the host pid must be refused")
                .to_string();
            assert!(
                err.contains("refusing to signal the host process"),
                "{command}: {err}"
            );
        }
        // A multi-target kill that never names the host pid passes through
        // the guarded builtin (the targets themselves do not exist, so kill
        // reports its own error instead of a host-guard refusal).
        let err = (tool.execute)(make_ctx(json!({ "command": "kill 1234 5678" })))
            .await
            .expect_err("multi-target kill without the host pid must not hit the host guard")
            .to_string();
        assert!(
            !err.contains("refusing to signal the host process"),
            "non-host multi-target kill must pass through: {err}"
        );
        // The signal-only form still passes through untouched.
        let res = (tool.execute)(make_ctx(json!({ "command": "kill -l" })))
            .await
            .expect("kill -l passes through the guarded builtin");
        assert!(text_of(&res).contains("SIGTERM"), "{}", text_of(&res));
    }

    #[tokio::test]
    async fn bash_brush_falls_back_to_subprocess_when_parse_fails() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // `echo )` is a syntax error for every shell; brush's parse check
        // fails, so execution falls back to the /bin/bash subprocess path,
        // which reports bash's own syntax error (documented fallback policy).
        let err = (tool.execute)(make_ctx(json!({ "command": "echo )" })))
            .await
            .expect_err("unparseable command must fail")
            .to_string();
        assert!(
            err.contains("syntax error"),
            "expected bash syntax error via the subprocess fallback: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_brush_timeout_reaps_descendants() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        let before = bash::brush::test_descendant_count();
        let err = (tool.execute)(make_ctx(json!({ "command": "sleep 30", "timeout": 0.5 })))
            .await
            .expect_err("sleep must time out")
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        // The external child must be reaped, not orphaned (the descendant
        // enumeration covers children forked by the brush thread).
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if bash::brush::test_descendant_count() <= before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("brush timeout left orphaned descendants behind");
    }

    fn extract_full_output_path(text: &str) -> String {
        text.split("Full output: ")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn bash_full_output_spill_persists_then_cleanup_removes() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // 60000 bytes > 50 KiB display cap → spills to a temp file; exit 0.
        let res = (tool.execute)(make_ctx(json!({ "command": "yes x | head -c 60000", "timeout": 10 })))
            .await
            .unwrap();
        let text = text_of(&res);
        let path = extract_full_output_path(&text);
        assert!(!path.is_empty(), "expected Full output path in: {text}");
        // Path must live inside this process's private spill dir (no
        // path-substitution / cross-process collision hazard).
        let spill_dir = bash_spill_dir();
        let path_buf = std::path::PathBuf::from(&path);
        assert!(
            path_buf.starts_with(&spill_dir),
            "spill path must be under contained dir {spill_dir:?}: {path}"
        );
        assert!(std::path::Path::new(&path).exists(), "success spill file should persist for reads: {path}");
        // Application-owned cleanup removes it.
        cleanup_full_output_path(&path);
        assert!(!std::path::Path::new(&path).exists(), "cleanup_full_output_path must remove the file");
    }

    #[tokio::test]
    async fn bash_full_output_spill_removed_on_nonzero_exit() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // Big output (spill) + nonzero exit: run_bash must clean up the spill
        // and must NOT publish a (dead) Full output path in the error text.
        // (Owned-file cleanup is covered race-free by bash.rs Drop/take unit
        // tests; this test asserts the no-dead-path error contract.)
        let err = (tool.execute)(make_ctx(json!({ "command": "yes x | head -c 60000; exit 7", "timeout": 10 })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited with code 7"), "got: {err}");
        assert!(
            !err.contains("Full output:"),
            "error path must not publish a dead full-output path: {err}"
        );
    }

    #[tokio::test]
    async fn bash_full_output_spill_removed_on_timeout() {
        let d = tmpdir();
        let tool = bash_tool(&d.to_string_lossy(), None, None, None);
        // Produce output continuously, then time out: spill must be cleaned up
        // and the error must not publish a dead Full output path.
        let err = (tool.execute)(make_ctx(json!({ "command": "yes x", "timeout": 0.5 })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(
            !err.contains("Full output:"),
            "timeout must not publish a dead full-output path: {err}"
        );
    }

    #[test]
    fn cleanup_full_output_path_is_idempotent() {
        // Empty/missing paths are no-ops (no panic).
        cleanup_full_output_path("");
        cleanup_full_output_path("/tmp/pi-rs-nonexistent-spill-XXXX.log");
        // A real leftover is removed.
        let d = tmpdir();
        let f = d.join("leftover.log");
        fs::write(&f, b"x").unwrap();
        cleanup_full_output_path(&f.to_string_lossy());
        assert!(!f.exists());
    }


    #[tokio::test]
    async fn ls_lists_entries_with_dir_suffix() {
        let d = tmpdir();
        fs::create_dir_all(d.join("subdir")).unwrap();
        fs::write(d.join("file.txt"), b"x").unwrap();
        let tool = ls_tool(&d.to_string_lossy());
        let c = make_ctx(json!({}));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("subdir/"), "got: {text}");
        assert!(text.contains("file.txt"), "got: {text}");
    }

    #[tokio::test]
    async fn find_matches_glob_respecting_gitignore() {
        let d = tmpdir();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/a.ts"), b"x").unwrap();
        fs::write(d.join("src/b.go"), b"x").unwrap();
        fs::write(d.join(".gitignore"), b"*.go\n").unwrap();
        fs::write(d.join(".git"), b"").unwrap(); // make it a repo so gitignore applies
        let tool = find_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "pattern": "*.ts" }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("src/a.ts"), "got: {text}");
        assert!(!text.contains("b.go"), "gitignored file leaked: {text}");
    }

    #[tokio::test]
    async fn glob_tool_matches_and_respects_gitignore_by_default() {
        let d = tmpdir();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/a.ts"), b"x").unwrap();
        fs::write(d.join("src/b.go"), b"x").unwrap();
        fs::write(d.join(".gitignore"), b"*.go\n").unwrap();
        fs::write(d.join(".git"), b"").unwrap();
        let tool = create_tool("glob", &d.to_string_lossy()).unwrap();
        assert_eq!(tool.name, "glob");
        let res = (tool.execute)(make_ctx(json!({ "pattern": "*.ts" }))).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("src/a.ts"), "got: {text}");
        assert!(!text.contains("b.go"), "gitignored file leaked: {text}");
    }

    #[tokio::test]
    async fn glob_tool_hidden_and_gitignore_flags() {
        let d = tmpdir();
        fs::write(d.join("visible.rs"), b"x").unwrap();
        fs::write(d.join(".secret.rs"), b"x").unwrap();
        fs::write(d.join("ignored.rs"), b"x").unwrap();
        fs::write(d.join(".gitignore"), b"ignored.rs\n").unwrap();
        fs::write(d.join(".git"), b"").unwrap();
        let tool = create_tool("glob", &d.to_string_lossy()).unwrap();

        let def = (tool.execute)(make_ctx(json!({ "pattern": "*.rs" }))).await.unwrap();
        let def_text = text_of(&def);
        assert!(def_text.contains("visible.rs"), "got: {def_text}");
        assert!(!def_text.contains(".secret.rs"), "hidden leaked: {def_text}");
        assert!(!def_text.contains("ignored.rs"), "gitignored leaked: {def_text}");

        let hid = (tool.execute)(make_ctx(json!({ "pattern": "*.rs", "hidden": true }))).await.unwrap();
        let hid_text = text_of(&hid);
        assert!(hid_text.contains(".secret.rs"), "hidden=true should include: {hid_text}");

        let no_gi = (tool.execute)(make_ctx(json!({ "pattern": "*.rs", "gitignore": false }))).await.unwrap();
        let no_gi_text = text_of(&no_gi);
        assert!(no_gi_text.contains("ignored.rs"), "gitignore=false should include: {no_gi_text}");
    }

    #[tokio::test]
    async fn glob_tool_rejects_path_escape_and_clamps_limit() {
        let d = tmpdir();
        for i in 0..5 {
            fs::write(d.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let tool = create_tool("glob", &d.to_string_lossy()).unwrap();
        let err = (tool.execute)(make_ctx(json!({ "pattern": "*", "path": ".." })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("escapes") || err.contains("working directory") || err.contains("workspace"),
            "expected confinement error, got: {err}"
        );

        let res = (tool.execute)(make_ctx(json!({ "pattern": "*.txt", "limit": 2 }))).await.unwrap();
        let text = text_of(&res);
        let count = text.lines().filter(|l| l.ends_with(".txt")).count();
        assert_eq!(count, 2, "limit=2 should return 2 matches: {text}");
        assert!(text.contains("results limit reached"), "expected limit notice: {text}");
        assert_eq!(res.details.get("resultLimitReached"), Some(&json!(2)));

        // Hard max is 200 even if caller asks for more.
        let res_hi = (tool.execute)(make_ctx(json!({ "pattern": "*.txt", "limit": 9999 }))).await.unwrap();
        let n = text_of(&res_hi).lines().filter(|l| l.ends_with(".txt")).count();
        assert_eq!(n, 5, "all five files under hard max: {}", text_of(&res_hi));
    }

    #[tokio::test]
    async fn find_limit_zero_unlimited_with_notice() {
        let d = tmpdir();
        for n in ["a.go", "b.go", "c.go"] {
            fs::write(d.join(n), b"").unwrap();
        }
        // fd treats --max-results 0 as unlimited; pi still reports the
        // "0 results limit reached" notice (len >= 0).
        let tool = find_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "pattern": "*.go", "limit": 0 }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        for n in ["a.go", "b.go", "c.go"] {
            assert!(text.contains(n), "limit=0 should be unlimited; missing {n}: {text}");
        }
        assert!(
            text.contains("0 results limit reached"),
            "expected pi limit-0 notice: {text}"
        );
        assert_eq!(res.details.get("resultLimitReached"), Some(&json!(0)));
    }

    #[tokio::test]
    async fn find_limit_negative_unlimited_no_notice() {
        let d = tmpdir();
        for n in ["a.go", "b.go"] {
            fs::write(d.join(n), b"").unwrap();
        }
        let tool = find_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "pattern": "*.go", "limit": -1 }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("a.go") && text.contains("b.go"), "got: {text}");
        assert!(
            !text.contains("limit reached"),
            "negative limit should not produce a limit notice: {text}"
        );
        assert!(res.details.get("resultLimitReached").is_none());
    }


    #[tokio::test]
    async fn grep_matches_pattern_with_line_numbers() {
        let d = tmpdir();
        fs::write(d.join("g.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let tool = grep_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "pattern": "beta" }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("g.txt:2: beta"), "got: {text}");
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let d = tmpdir();
        fs::write(d.join(".git"), b"").unwrap();
        fs::write(d.join(".gitignore"), b"ignoreme.txt\n").unwrap();
        fs::write(d.join("ignoreme.txt"), b"secret\n").unwrap();
        fs::write(d.join("keep.txt"), b"secret\n").unwrap();
        let tool = grep_tool(&d.to_string_lossy());
        let c = make_ctx(json!({ "pattern": "secret" }));
        let res = (tool.execute)(c).await.unwrap();
        let text = text_of(&res);
        assert!(text.contains("keep.txt"), "got: {text}");
        assert!(!text.contains("ignoreme.txt"), "gitignored file leaked: {text}");
    }
}