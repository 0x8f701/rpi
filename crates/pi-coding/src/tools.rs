//! Built-in coding tools (port of pi's `coding/tools.go`): read, write, edit,
//! bash, ls, find, and grep. Relative filesystem paths resolve from `cwd`.
//!
//! Tool definitions, parameter schemas, prompt guidelines, path resolution,
//! edit unique/non-overlapping replacement semantics, bounded output,
//! binary/image recognition, bash timeout/cancellation with streaming updates,
//! and gitignore-aware find/grep. Reusable helpers live in private sibling
//! modules; this module assembles the tools and exposes the factory API used
//! by the Session facade.

pub(crate) mod ask;
mod ast_edit;
mod ast_grep;
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
mod web_search;

pub(crate) use crate::application::generation::GenerationCaptureFn;
use std::collections::{BTreeMap, HashMap, VecDeque, hash_map::Entry};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn captured_policy_fs(
    generation: &Arc<crate::application::generation::ApplicationGeneration>,
) -> Result<Arc<crate::fs::PolicyFs>> {
    generation
        .sandbox()
        .fs()
        .confined()
        .cloned()
        .map_err(anyhow::Error::from)
}
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use pi_agent::{
    AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability, ToolExecutionMode,
    ToolUpdateFn,
};
use pi_ai::{ConstrainedSampling, ConstrainedSamplingStrictness, ContentBlock, Schema};

use crate::todo::{
    TODO_ERROR_MARKER, TodoRuntime, TodoToolDetails, deserialize_todo_op, tool_failure_result,
};
use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, FIND_DEFAULT_LIMIT, GLOB_DEFAULT_LIMIT, GLOB_MAX_LIMIT,
    GREP_DEFAULT_LIMIT, GREP_MAX_LINE_LENGTH, LS_DEFAULT_LIMIT, TruncationResult, format_size,
    truncate_head, truncate_line, utf16_len,
};

use crate::mcp::{McpRegistry, mcp_tool};
use crate::memory::{
    MemoryConfig, memory_tool, memory_tool_with_session_env, memory_tools_for,
    memory_tools_for_persona, memory_tools_for_with_capabilities,
    memory_tools_for_with_capabilities_and_generation, memory_tools_for_with_generation,
    memory_tools_for_with_live_config_and_generation, recall_tool, reflect_tool, retain_tool,
};
use ast_edit::{ast_edit_tool, ast_edit_tool_with_generation};
use ast_grep::{ast_grep_tool, ast_grep_tool_with_generation};
use bash::{BashSpillAuthority, OutputAccumulator, OutputSnapshot};
use browser::browser_tool_for_workspace;
pub use browser::{BrowserSandboxError, NativeBrowserSandboxFailure};
use debug::{debug_tool, debug_tool_with_generation};
use doc_convert::{extract_doc_text, is_doc};
use editdiff::generate_edit_details;
use editmatch::{
    EditEntry, apply_edits_to_normalized_content, detect_line_ending, normalize_to_lf,
    restore_line_endings, strip_bom,
};
use eval::{eval_tool, eval_tool_with_generation};
use github::github_tool;
use glob::{IgnoreStack, match_fd_glob, match_rg_glob};
use image::{inspect_image_tool, inspect_image_tool_with_generation};
use image_gen::{
    generate_image_tool, generate_image_tool_for_workspace,
    generate_image_tool_for_workspace_with_generation,
};
use imageresize::process_image;
use lsp::{lsp_tool, lsp_tool_with_generation, lsp_tool_with_rules};
use mime::{detect_supported_image_mime_type, is_pdf_bytes};
use mutation_queue::with_policy_mutation_queue;
use notebook::{notebook_tool, notebook_tool_with_generation};
use paths::{resolve_mutation_path, resolve_read_path, resolve_scoped_path};
use web_search::web_search_tool;

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
/// Per-command cap for live merged Bash chunk events. Final output still uses
/// the rolling accumulator and spill file, so bytes beyond this bound remain
/// available through the completed result without flooding UI/RPC consumers.
const BASH_LIVE_STREAM_MAX_BYTES: usize = DEFAULT_MAX_BYTES;

/// Supplies the PI_* session metadata exposed to bash commands. Called per
/// execution so a mid-session change is reflected. `None` for standalone
/// construction (matches pi's `exposeSessionEnvironment && ctx` guard).
pub type SessionEnvFn = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;
pub type SkillSnapshotFn = Arc<dyn Fn() -> Vec<crate::Skill> + Send + Sync>;
/// Resolves internal `agent://`, `history://`, and `artifact://` URIs passed to
/// the `read` tool through an already-open retained file capability. The
/// capability and resolver closure remain private to `pi-coding`.
#[derive(Clone)]
pub struct InternalUriResolver {
    resolve: Arc<dyn Fn(&str) -> anyhow::Result<Arc<crate::fs::CapabilityFile>> + Send + Sync>,
}

#[derive(Clone)]
pub struct InternalUriResolverBinding {
    slot: Arc<Mutex<Option<InternalUriResolver>>>,
}
pub type InternalUriResolverFn = InternalUriResolver;

impl InternalUriResolver {
    pub(crate) fn new(
        resolve: impl Fn(&str) -> anyhow::Result<Arc<crate::fs::CapabilityFile>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            resolve: Arc::new(resolve),
        }
    }

    pub fn deferred() -> (Self, InternalUriResolverBinding) {
        let slot = Arc::new(Mutex::new(None::<Self>));
        let resolve_slot = slot.clone();
        let resolver = Self::new(move |uri| {
            resolve_slot
                .lock()
                .as_ref()
                .ok_or_else(|| anyhow!("internal URI resolver is not initialized"))?
                .resolve_file(uri)
        });
        (resolver, InternalUriResolverBinding { slot })
    }

    pub(crate) fn resolve_file(&self, uri: &str) -> anyhow::Result<Arc<crate::fs::CapabilityFile>> {
        (self.resolve)(uri)
    }
    pub fn resolve_display_path(&self, uri: &str) -> anyhow::Result<PathBuf> {
        Ok(self.resolve_file(uri)?.display_path())
    }
}

impl InternalUriResolverBinding {
    pub fn bind(&self, resolver: InternalUriResolver) -> anyhow::Result<()> {
        let mut slot = self.slot.lock();
        if slot.is_some() {
            return Err(anyhow!("internal URI resolver is already initialized"));
        }
        *slot = Some(resolver);
        Ok(())
    }
}
/// Resolves the live `settings.memory` configuration for memory-tool builds
/// and turn-start injection. Returns `None` before settings are attached
/// (fall back to the built-in `local` backend).
pub type MemoryConfigFn = Arc<dyn Fn() -> Option<MemoryConfig> + Send + Sync>;
#[derive(Clone)]
pub struct BashProcessContext {
    pub manager: crate::ProcessManager,
    pub owner_id: crate::ProcessOwnerId,
    pub(crate) auth_manager: Option<Arc<crate::AuthManager>>,
}

fn factory_workspace(cwd: &str) -> crate::WorkspaceRoots {
    crate::WorkspaceRoots::for_tool_factory(cwd)
}

/// Built-in coding tool identifiers.
pub const TOOL_NAMES: &[&str] = &[
    "read",
    "inspect_image",
    "generate_image",
    "bash",
    "edit",
    "write",
    "grep",
    "find",
    "glob",
    "ls",
    "browser",
    "web_search",
    "ast_grep",
    "ast_edit",
    "ask",
    "lsp",
    "github",
    "memory",
    "recall",
    "retain",
    "reflect",
    "mcp",
    "debug",
    "eval",
    "notebook",
];

/// One-line prompt snippets keyed by tool name.
#[must_use]
pub fn tool_snippet(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => "Read file contents",
        "inspect_image" => {
            "Inspect an image file's metadata and statistics without rendering it (format, dimensions, color type, brightness, dominant colors)"
        }
        "generate_image" => {
            "Generate images through the configured OpenAI-compatible provider (images/generations): saves PNG/JPEG/WebP/GIF/BMP to the workspace (default <cwd>/images), returns paths, dimensions, and a bounded prompt echo; prompts capped at 4096 characters, n 1-4, sizes 256x256/512x512/1024x1024"
        }
        "bash" => {
            "Execute finite foreground shell commands, or supervised long-running commands with background=true"
        }
        "edit" => {
            "Make precise workspace-scoped file edits with exact text replacement, including multiple disjoint edits in one call. PolicyFs rejects external and symlink escapes"
        }
        "write" => {
            "Create or overwrite workspace-scoped files through retained PolicyFs authority. PolicyFs rejects external and symlink escapes"
        }
        "grep" => "Search file contents for patterns (respects .gitignore)",
        "find" => "Find files by glob pattern (respects .gitignore)",
        "glob" => "Match files by glob pattern under workspace roots (sandboxed, bounded)",
        "ls" => "List directory contents",
        "browser" => {
            "Automate a headless Chrome/Chromium browser: navigate, click, fill, screenshot, extract, list_tabs, close"
        }
        "web_search" => {
            "Search the web via the Exa API (disabled while RPI_OFFLINE is set)"
        }
        "ast_grep" => {
            "Search code structurally with ast-grep patterns ($-metavariables, tree-sitter)"
        }
        "ast_edit" => {
            "Rewrite a single file structurally with ast-grep pattern→rewrite (metavariables substituted)"
        }
        "ask" => "Ask the user a question and use their answer (interactive sessions only)",
        "lsp" => {
            "Query a language server: hover, definition, references, diagnostics, symbols, rename, code actions (spawns a per-language server per call)"
        }
        "github" => {
            "Query the GitHub API via governed gh CLI or captured session HTTP: search/get/list issues and PRs, create issues and comments, list commits, view files, search code"
        }
        "mcp" => {
            "Call Model Context Protocol (MCP) servers configured in settings mcpServers: list_servers, list_tools, call (stdio child processes, session-scoped)"
        }
        "debug" => {
            "Drive a DAP debug adapter over stdio (gdb, lldb-dap, debugpy): launch, set_breakpoint, continue_, pause, step_over/step_in/step_out, stack_trace, variables, evaluate, threads, terminate (session-scoped, one adapter at a time)"
        }
        "eval" => {
            "Evaluate code in a persistent session-scoped kernel: python (python3 subprocess, full stdlib) or js (embedded QuickJS); state persists across calls, output bounded at 64 KiB and redacted, errors classified (syntax/runtime/timeout)"
        }
        "notebook" => {
            "Read, execute, and edit Jupyter notebooks (.ipynb): read lists cells, execute runs code cells through a session-scoped Python kernel (write=true persists outputs), edit appends a markdown/code cell"
        }
        "memory" => {
            "Persistent per-project memory: learn, recall, list, and forget note-style entries that survive across sessions in the same repository"
        }
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
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
    for key in ["list", "task", "phase", "items", "dependsOn"] {
        object.entry(key).or_insert(Value::Null);
    }
    if object.get("cascade").is_none_or(Value::is_null) {
        object.insert("cascade".to_owned(), Value::Bool(false));
    }
    Ok(arguments)
}
/// Builds an object schema from ordered `(name, schema)` pairs.
/// Declaration order is preserved in `property_order` for deterministic
/// serialization. Names in `required` are required (the rest are optional,
/// like pi's `Opt`). Duplicate names keep the first declaration (both its
/// position and its schema); later duplicates are dropped so the map and
/// order stay consistent.
pub(crate) fn s_object(props: Vec<(&str, Schema)>, required: Vec<&str>) -> Schema {
    let mut properties = HashMap::with_capacity(props.len());
    let mut property_order = Vec::with_capacity(props.len());
    for (n, s) in props {
        if let Entry::Vacant(e) = properties.entry(n.to_string()) {
            property_order.push(e.key().clone());
            e.insert(s);
        }
    }
    let required = required.into_iter().map(String::from).collect();
    Schema {
        schema_type: Some(Value::String("object".into())),
        properties,
        property_order,
        required,
        ..Default::default()
    }
}
fn s_strict_object(props: Vec<(&str, Schema)>, required: Vec<&str>) -> Schema {
    let mut schema = s_object(props, required);
    schema.additional_properties = Some(Value::Bool(false));
    schema
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
        "bash" => Ok(bash_tool(cwd, session_env, None)),
        "edit" => Ok(edit_tool(cwd)),
        "write" => Ok(write_tool(cwd)),
        "grep" => Ok(grep_tool(cwd)),
        "find" => Ok(find_tool(cwd)),
        "glob" => Ok(glob_tool(cwd)),
        "ls" => Ok(ls_tool(cwd)),
        "browser" => Err(anyhow!("browser tool requires an application generation")),
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
        "mcp" => Err(anyhow!("mcp tool requires an application generation")),
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
/// into the `read` tool for `agent://`, `history://`, and `artifact://` URIs.
pub fn create_tool_with_context_and_resolver(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    resolver: Option<InternalUriResolverFn>,
) -> Result<AgentTool> {
    create_tool_with_context_and_resolver_and_rules(name, cwd, session_env, skills, resolver, None)
}

/// [`create_tool_with_context_and_resolver`] with the session's live path
/// permission rules threaded into the `lsp` tool (see
/// [`create_tool_with_session_env_and_rules`]).
pub fn create_tool_with_context_and_resolver_and_rules(
    name: &str,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    resolver: Option<InternalUriResolverFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Result<AgentTool> {
    match name {
        "read" => Ok(read_tool_with_resolver(cwd, skills, resolver)),
        "bash" => Ok(bash_tool(cwd, session_env, None)),
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
    resolver: Option<InternalUriResolverFn>,
    rules: Option<crate::PermissionRulesSource>,
) -> Result<AgentTool> {
    create_tool_with_context_and_resolver_and_rules(name, cwd, session_env, skills, resolver, rules)
}

/// Default child coding tools with memory selected from the parent's live
/// configuration and local storage redirected to the durable persona root.
pub(crate) fn create_coding_tools_with_context_and_resolver_for_persona(
    cwd: &str,
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let mut tools = vec![
        read_tool_with_resolver(cwd, skills, resolver),
        bash_tool(cwd, session_env.clone(), process),
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
/// `image_gen` supplies the `generate_image` tool's model/endpoint/credential
/// resolution; `None` falls back to the turn's model and env credentials.
pub fn create_coding_tools_for_workspace_with_context_and_resolver(
    workspace: crate::WorkspaceRoots,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let cwd = workspace.cwd().to_string_lossy().into_owned();
    let mut tools = vec![
        read_tool_for_workspace(workspace.clone(), skills, resolver),
        bash_tool(&cwd, session_env.clone(), process),
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
pub(crate) fn create_coding_tools_for_workspace_with_generation(
    workspace: crate::WorkspaceRoots,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
    capture: GenerationCaptureFn,
) -> Result<Vec<AgentTool>> {
    create_coding_tools_for_workspace_with_generation_and_persona_skills(
        workspace,
        session_env,
        skills,
        process,
        resolver,
        memory,
        image_gen,
        capture,
        None,
    )
}

pub(crate) fn create_coding_tools_for_workspace_with_generation_and_persona_skills(
    workspace: crate::WorkspaceRoots,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
    capture: GenerationCaptureFn,
    persona_skills: Option<Arc<Vec<crate::PersonaSkill>>>,
) -> Result<Vec<AgentTool>> {
    let cwd = workspace.cwd().to_string_lossy().into_owned();
    let mut tools = vec![
        read_tool_for_workspace_captured(
            workspace.clone(),
            skills,
            resolver,
            capture.clone(),
            persona_skills,
        ),
        inspect_image_tool_with_generation(workspace.clone(), capture.clone()),
        bash_tool_with_generation(&cwd, session_env.clone(), process, capture.clone()),
        browser_tool_for_workspace(workspace.clone(), capture.clone()),
        edit_tool_for_workspace_captured(workspace.clone(), capture.clone()),
        write_tool_for_workspace_captured(workspace.clone(), capture.clone()),
        ast_edit_tool_with_generation(workspace.clone(), capture.clone()),
        ast_grep_tool_with_generation(workspace.clone(), capture.clone()),
        ls_tool_with_generation(workspace.clone(), capture.clone()),
        find_tool_with_generation(workspace.clone(), capture.clone()),
        grep_tool_with_generation(workspace.clone(), capture.clone()),
        glob_tool_with_generation(workspace.clone(), capture.clone()),
        generate_image_tool_for_workspace_with_generation(workspace, image_gen, capture.clone()),
        web_search::web_search_tool_with_generation(capture.clone()),
        github::github_tool_with_generation(capture.clone()),
        lsp_tool_with_generation(&cwd, crate::empty_permission_rules(), capture.clone()),
        debug_tool_with_generation(&cwd, capture.clone()),
        eval_tool_with_generation(&cwd, capture.clone()),
        notebook_tool_with_generation(&cwd, capture.clone()),
    ];
    tools.extend(memory_tools_for_with_live_config_and_generation(
        &cwd,
        session_env,
        memory,
        capture,
    ));
    Ok(tools)
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
        bash_tool(cwd, session_env.clone(), process),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        memory_tool_with_session_env(cwd, session_env),
    ]
}

/// Like [`create_coding_tools_with_context`] but the `read` tool also resolves
/// internal URIs and memory follows the parent session's live backend selection.
pub fn create_coding_tools_with_context_and_resolver(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    skills: Option<SkillSnapshotFn>,
    process: Option<BashProcessContext>,
    resolver: Option<InternalUriResolverFn>,
    memory: Option<MemoryConfigFn>,
    image_gen: Option<ImageGenConfigFn>,
) -> Vec<AgentTool> {
    let mut tools = vec![
        read_tool_with_resolver(cwd, skills, resolver),
        bash_tool(cwd, session_env.clone(), process),
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
        bash_tool(cwd, None, None),
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
        bash_tool(cwd, session_env.clone(), None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        memory_tool_with_session_env(cwd, session_env),
    ]
}

/// Read-only built-ins bound to a live application generation, in upstream
/// order: `[read, grep, find, glob, ls, web_search, ast_grep]`.
///
/// The generation-less constructors (`create_tool` family, below) fail closed
/// with "<tool> requires an application generation"; detached side-chat
/// agents (code-review hunk agents, `/btw` defaults) must build this variant
/// against the forking application's captured generation so the
/// PolicyFs-backed read tools execute real filesystem authority.
pub(crate) fn create_read_only_tools_for_workspace_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> Vec<AgentTool> {
    vec![
        read_tool_for_workspace_captured(workspace.clone(), None, None, capture.clone(), None),
        grep_tool_with_generation(workspace.clone(), capture.clone()),
        find_tool_with_generation(workspace.clone(), capture.clone()),
        glob_tool_with_generation(workspace.clone(), capture.clone()),
        ls_tool_with_generation(workspace.clone(), capture.clone()),
        web_search::web_search_tool_with_generation(capture.clone()),
        ast_grep::ast_grep_tool_with_generation(workspace, capture),
    ]
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
        bash_tool(cwd, None, None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
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
        bash_tool(cwd, session_env.clone(), None),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
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
        cwd,
        session_env,
        skills,
        process,
        resolver,
        None,
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
        bash_tool(cwd, session_env.clone(), process),
        edit_tool(cwd),
        write_tool(cwd),
        ast_edit_tool(cwd),
        grep_tool(cwd),
        find_tool(cwd),
        glob_tool(cwd),
        ls_tool(cwd),
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
                    ..s_array(
                        s_string("agent name"),
                        "optional explicit agent that must execute each task; parallel to items (agents[i] names the agent for items[i]); length must equal items",
                    )
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
                    enum_values: [
                        "init",
                        "start",
                        "done",
                        "drop",
                        "rm",
                        "append",
                        "add_dependency",
                        "remove_dependency",
                        "update_dependencies",
                        "view",
                    ]
                    .into_iter()
                    .map(|value| json!(value))
                    .collect(),
                    description: Some("operation to apply".to_owned()),
                    ..Schema::default()
                },
            ),
            (
                "list".to_owned(),
                nullable(s_array(init_phase, "phased task list (init)")),
            ),
            (
                "task".to_owned(),
                nullable(s_string(
                    "stable task ID (preferred), or exact task content for start/done/drop/rm compatibility",
                )),
            ),
            ("phase".to_owned(), nullable(s_string("phase name"))),
            (
                "items".to_owned(),
                nullable(s_array(
                    s_string("task content"),
                    "tasks to append or initialize",
                )),
            ),
            (
                "dependsOn".to_owned(),
                nullable(s_array(
                    s_string("stable dependency task ID"),
                    "dependency task IDs for add/remove/update operations",
                )),
            ),
            (
                "cascade".to_owned(),
                nullable(s_boolean(
                    "For rm, explicitly remove dependency edges from surviving tasks before removing dependency targets",
                )),
            ),
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
        required: vec![
            "op",
            "list",
            "task",
            "phase",
            "items",
            "dependsOn",
            "cascade",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    };
    let mut tool = AgentTool::new(
        "todo",
        "Maintain the durable session todo DAG while preserving phased presentation. Every returned task has a stable id, dependsOn, ready, and waitingOn. waitingOn is a dependency-readiness projection, not a task status. Phase order is not a dependency: any ready task may proceed. Completed and dropped dependencies both satisfy dependents. The attached executor runs every ready task concurrently up to its configured limit, so keep independent work in separate tasks without dependsOn edges — reserve dependencies for genuine data or control ordering (a task consumes another's output, or must not start before it); never chain unrelated steps. Use init/append, start/done/drop/rm, add_dependency/remove_dependency/update_dependencies, or read-only view. Dependency operations require stable task IDs. rm rejects dependency targets unless cascade=true explicitly removes those edges. init phase entries may carry an optional agents array parallel to items to bind each task to a specific agent that must execute it. Use concise task titles (≤60 chars): the task content is the title the panel shows — longer titles are truncated.",
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
    tool.constrained_sampling = Some(ConstrainedSampling::json_schema(
        ConstrainedSamplingStrictness::Prefer,
    ));
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
            (
                "path",
                s_string("Path to the file to read (relative or absolute)"),
            ),
            (
                "offset",
                s_number("Line number to start reading from (1-indexed)"),
            ),
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
    .with_prompt_guidelines(vec![
        "Use read to examine files instead of cat or sed.".to_string(),
    ])
}

fn read_tool_for_workspace_captured(
    workspace: crate::WorkspaceRoots,
    skills: Option<SkillSnapshotFn>,
    resolver: Option<InternalUriResolverFn>,
    capture: GenerationCaptureFn,
    persona_skills: Option<Arc<Vec<crate::PersonaSkill>>>,
) -> AgentTool {
    let params = s_object(
        vec![
            (
                "path",
                s_string("Path to the file to read (relative or absolute)"),
            ),
            (
                "offset",
                s_number("Line number to start reading from (1-indexed)"),
            ),
            ("limit", s_number("Maximum number of lines to read")),
        ],
        vec!["path"],
    );
    AgentTool::new(
        "read",
        "Read a policy-authorized file through retained filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let skills = skills.clone();
            let resolver = resolver.clone();
            let capture = capture.clone();
            let persona_skills = persona_skills.clone();
            async move {
                let generation = capture()?;
                run_read_captured(
                    &workspace,
                    ctx.arguments,
                    skills.as_ref(),
                    resolver.as_ref(),
                    &generation,
                    ctx.model.as_ref(),
                    ctx.abort,
                    persona_skills.as_ref().map(|skills| skills.as_slice()),
                )
                .await
            }
        },
    )
    .with_capability(ToolCapability::Read)
    .with_prompt_guidelines(vec![
        "Use read to examine files instead of cat or sed.".to_string(),
    ])
}

fn overlay_persona_skill_content(
    uri: &str,
    persona_skills: Option<&[crate::PersonaSkill]>,
) -> Result<Option<String>> {
    let Some(persona_skills) = persona_skills.filter(|skills| !skills.is_empty()) else {
        return Ok(None);
    };
    let rest = uri
        .strip_prefix("skill://")
        .ok_or_else(|| anyhow!("skill URI must start with skill://"))?;
    if rest.is_empty() || rest.contains(['?', '#', '\\']) {
        bail!("invalid skill URI: {uri}");
    }
    let (name, requested) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(skill) = persona_skills.iter().find(|skill| skill.name == name) else {
        return Ok(None);
    };
    if !requested.is_empty() {
        bail!("persona skills expose only their main SKILL.md");
    }
    Ok(Some(skill.content.clone()))
}

async fn run_read_captured(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    skills: Option<&SkillSnapshotFn>,
    resolver: Option<&InternalUriResolverFn>,
    generation: &Arc<crate::application::generation::ApplicationGeneration>,
    model: Option<&pi_ai::Model>,
    abort: AbortSignal,
    persona_skills: Option<&[crate::PersonaSkill]>,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = arg_str(&args, "path");
    if path.starts_with("skill://") {
        if let Some(text) = overlay_persona_skill_content(&path, persona_skills)? {
            return render_read_result(&path, &text, &args);
        }
        let skills = skills
            .ok_or_else(|| anyhow!("skill:// resolution is unavailable for this read tool"))?(
        );
        let text = crate::selector::read_skill_uri_with_capabilities(
            generation.sandbox().capabilities(),
            &path,
            &skills,
        )?;
        return render_read_result(&path, &text, &args);
    }
    let (resolved, data) = if let Some(scheme) = internal_uri_scheme(&path) {
        let file = resolver
            .ok_or_else(|| anyhow!("{scheme}:// resolution is unavailable for this read tool"))?
            .resolve_file(&path)?;
        let resolved = file.display_path();
        let data = file.read_bounded(64 * 1024 * 1024)?;
        (resolved, data)
    } else if let Some(data) = bash::read_full_output_path(&path, 64 * 1024 * 1024) {
        (PathBuf::from(&path), data?)
    } else {
        let resolved = PathBuf::from(resolve_read_path(&path, workspace)?);
        let data = captured_policy_fs(generation)?.read_bounded(&resolved, 64 * 1024 * 1024)?;
        (resolved, data)
    };
    check_aborted(&abort)?;
    if let Some(result) = inline_video_bytes(&resolved, &data)? {
        return Ok(result);
    }
    if let Some(mime) = detect_supported_image_mime_type(&data) {
        let processed = process_image(&data, &mime, true);
        let non_vision_note = model
            .is_some_and(|model| !model.input.iter().any(|input| input == "image"))
            .then_some(NON_VISION_IMAGE_NOTE);
        if !processed.ok {
            let mut note = format!("Read image file [{mime}]\n{}", processed.message);
            if let Some(note_suffix) = non_vision_note {
                note.push('\n');
                note.push_str(note_suffix);
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
        if let Some(note_suffix) = non_vision_note {
            note.push('\n');
            note.push_str(note_suffix);
        }
        return Ok(AgentToolResult {
            content: vec![
                ContentBlock::text(note),
                ContentBlock::Image {
                    data: base64::engine::general_purpose::STANDARD.encode(&processed.data),
                    mime_type: processed.mime_type,
                },
            ],
            ..Default::default()
        });
    }
    let resolved_text = resolved.to_string_lossy().into_owned();
    if is_pdf_bytes(&data) {
        let text = extract_pdf_text(generation.clone(), &resolved_text, abort).await?;
        let quoted = format!("'{}'", resolved_text.replace('\'', "'\\''"));
        return render_read_result(
            &format!("<(pdftotext -layout -nopgbrk {quoted} -)"),
            &text,
            &args,
        );
    }
    if is_doc(Path::new(&resolved_text)) {
        let extracted = extract_doc_text(generation.clone(), &resolved_text, abort).await?;
        let quoted = format!("'{}'", resolved_text.replace('\'', "'\\''"));
        return render_read_result(
            &extracted.converter.sed_hint(&quoted),
            &extracted.text,
            &args,
        );
    }
    render_read_result(&path, &String::from_utf8_lossy(&data), &args)
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

fn inline_video_bytes(path: &Path, bytes: &[u8]) -> Result<Option<AgentToolResult>> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), Some("mp4" | "webm" | "ogg")) {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_INLINE_VIDEO_BYTES {
        return Err(anyhow!(
            "video exceeds the {} MiB Web preview limit",
            MAX_INLINE_VIDEO_BYTES / 1024 / 1024
        ));
    }
    let Some(mime_type) = video_mime(path, &bytes[..bytes.len().min(64)]) else {
        return Err(anyhow!(
            "video extension does not match supported media content"
        ));
    };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    if data.len() > MAX_INLINE_VIDEO_BASE64_BYTES {
        return Err(anyhow!("video exceeds the inline Web transport limit"));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video")
        .to_owned();
    Ok(Some(AgentToolResult {
        content: vec![ContentBlock::text(format!(
            "Read video file [{mime_type}] {name} ({})",
            format_size(bytes.len())
        ))],
        details: json!({ "media": [{ "kind": "video", "mimeType": mime_type, "data": data, "name": name, "sizeBytes": bytes.len() }] }),
        ..Default::default()
    }))
}

async fn run_read(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _skills: Option<&SkillSnapshotFn>,
    _resolver: Option<&InternalUriResolverFn>,
    _model: Option<&pi_ai::Model>,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("read requires an application generation"))
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
            &format!(
                "[PDF text output capped at {}]\n",
                format_size(PDF_EXTRACT_MAX_BYTES)
            ),
        );
    }
    text
}

/// Runs governed `pdftotext -layout -nopgbrk <abs> -` under the captured
/// application generation and returns the extracted text. The trusted probe
/// fixes the executable while the process context binds the exact
/// `DocumentConverter` family, timeout, abort signal, and output bounds.
async fn extract_pdf_text(
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    abs: &str,
    abort: AbortSignal,
) -> Result<String> {
    let bin = crate::process::run_trusted_tool_probe(
        &generation,
        crate::process::TrustedToolProbe::PdfToText,
    )
    .map_err(|_| {
        anyhow!(
            "PDF extraction requires poppler-utils (pdftotext); install it or use bash: pdftotext -layout -nopgbrk {} -",
            abs
        )
    })?;
    extract_pdf_text_with(generation, bin.as_path(), abs, abort).await
}

/// Governed PDF extraction with an injectable absolute executable path for
/// focused tests. Production callers obtain this path only from the trusted
/// `PdfToText` probe above.
async fn extract_pdf_text_with(
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    bin: &Path,
    abs: &str,
    abort: AbortSignal,
) -> Result<String> {
    let cwd = bin
        .parent()
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| Path::new("/"));
    let output = crate::process::GovernedProcessContext::new(generation, None)
        .run_with_abort(
            pi_sandbox::SpawnFamily::DocumentConverter,
            crate::process::GovernedProcessSpec {
                program: bin.to_path_buf(),
                args: vec![
                    "-layout".to_owned(),
                    "-nopgbrk".to_owned(),
                    abs.to_owned(),
                    "-".to_owned(),
                ],
                cwd: cwd.to_path_buf(),
                environment: BTreeMap::new(),
                timeout: PDF_EXTRACT_TIMEOUT,
                stdout_bytes: PDF_EXTRACT_MAX_BYTES,
                stderr_bytes: PDF_EXTRACT_MAX_BYTES,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
            Some(abort),
        )
        .await
        .map_err(|error| anyhow!("failed to start pdftotext: {error}"))?;

    if output.timed_out {
        return Err(anyhow!(
            "PDF text extraction timed out after {}s (PDF may be corrupted or encrypted). Use bash: pdftotext -layout -nopgbrk {} -",
            PDF_EXTRACT_TIMEOUT.as_secs(),
            abs
        ));
    }
    if output.cancelled {
        return Err(anyhow!("PDF text extraction cancelled"));
    }
    if let Some(status) = output.status {
        if status != 0 {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!(
                "pdftotext failed (exit status {status}): {detail}. Use bash: pdftotext -layout -nopgbrk {} - to inspect (PDF may be encrypted or corrupted).",
                abs
            ));
        }
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.trim().is_empty() {
        return Err(anyhow!(
            "PDF contains no extractable text (scanned or encrypted)"
        ));
    }
    Ok(apply_pdf_cap_notice(text, output.stdout_capped))
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

fn write_tool(cwd: &str) -> AgentTool {
    write_tool_for_workspace(factory_workspace(cwd))
}

fn write_tool_for_workspace(workspace: crate::WorkspaceRoots) -> AgentTool {
    let params = write_tool_parameters();
    AgentTool::new(
        "write",
        "Write or append a bounded chunk to a workspace-scoped file through retained PolicyFs authority. Creates the file if it doesn't exist, overwrites by default, and creates parent directories. Set append=true only for continuation chunks after a successful initial write. External and symlink escapes are rejected.",
        params,
        move |_ctx| async move { Err(anyhow!("write requires an application generation")) },
    )
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Use write only for new files or complete rewrites.".to_string(),
        "If one complete write exceeds the output limit, retry with bounded chunks: overwrite the first chunk, then append each remaining chunk in order.".to_string(),
    ])
}

fn write_tool_parameters() -> Schema {
    s_object(
        vec![
            (
                "path",
                s_string(
                    "Workspace-scoped path to write. Relative paths resolve from cwd. PolicyFs retains mutation authority with no ambient path reopening; external and symlink escapes are rejected.",
                ),
            ),
            ("content", s_string("Content chunk to write to the file")),
            (
                "append",
                s_boolean(
                    "Append this content after the existing file instead of overwriting it. Use only for bounded continuation chunks after a successful initial write.",
                ),
            ),
        ],
        vec!["path", "content"],
    )
}

async fn run_write(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("write requires an application generation"))
}
fn write_tool_for_workspace_captured(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = write_tool_parameters();
    AgentTool::new("write", "Write or append a bounded chunk to a workspace-scoped file through retained PolicyFs authority. Creates the file if it doesn't exist, overwrites by default, and creates parent directories. Set append=true only for continuation chunks after a successful initial write. External and symlink escapes are rejected.", params, move |ctx| {
        let workspace = workspace.clone();
        let capture = capture.clone();
        async move {
            let generation = capture()?;
            run_write_captured(&workspace, &generation, ctx.arguments, ctx.abort).await
        }
    })
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Use write only for new files or complete rewrites.".to_string(),
        "If one complete write exceeds the output limit, retry with bounded chunks: overwrite the first chunk, then append each remaining chunk in order.".to_string(),
    ])
}

async fn run_write_captured(
    workspace: &crate::WorkspaceRoots,
    generation: &Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;
    let path = required_arg_str(&args, "path")?;
    let content = required_arg_str(&args, "content")?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(anyhow!(
            "Write content exceeds the 64 MiB per-call limit; retry with bounded chunks and append=true after the first chunk"
        ));
    }
    let should_append = arg_bool(&args, "append");
    let absolute = PathBuf::from(resolve_mutation_path(&path, workspace)?);
    let fs = captured_policy_fs(generation)?;
    let (root_identity, relative) = fs.mutation_coordinate(&absolute)?;
    with_policy_mutation_queue(fs.revision(), root_identity, &relative, || async {
        check_aborted(&abort)?;
        if let Some(parent) = absolute.parent() {
            fs.create_dir_all(parent, crate::fs::FileMode::Shared)?;
        }
        let existing = match fs.open_file(&absolute) {
            Ok(opened) => Some((
                opened.identity(),
                opened.read_bounded(MAX_WRITE_BYTES as u64)?,
            )),
            Err(crate::fs::FsError::System {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => None,
            Err(error) => return Err(error.into()),
        };
        if should_append && existing.is_none() {
            return Err(anyhow!(
                "Cannot append to missing file {path}; write the first chunk with append=false"
            ));
        }
        let existing_len = existing.as_ref().map_or(0, |(_, bytes)| bytes.len());
        if should_append && existing_len.saturating_add(content.len()) > MAX_WRITE_BYTES {
            return Err(anyhow!(
                "Write would exceed the 64 MiB file limit for {path}"
            ));
        }
        let mut bytes = Vec::with_capacity(if should_append {
            existing_len + content.len()
        } else {
            content.len()
        });
        if should_append {
            bytes.extend_from_slice(&existing.as_ref().expect("append existence checked").1);
        }
        bytes.extend_from_slice(content.as_bytes());
        fs.replace_file(crate::fs::PolicyReplacement {
            path: absolute.clone(),
            expected: existing.map(|(identity, _)| identity),
            bytes,
            mode: crate::fs::FileMode::Shared,
        })?;
        check_aborted(&abort)?;
        let action = if should_append { "appended" } else { "wrote" };
        Ok(text_result(format!(
            "Successfully {action} {} bytes to {}",
            utf16_len(&content),
            path
        )))
    })
    .await
}

fn edit_tool_for_workspace_captured(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let edit_obj = s_object(
        vec![
            ("oldText", s_string("Exact unique text")),
            ("newText", s_string("Replacement text")),
        ],
        vec!["oldText", "newText"],
    );
    let params = s_object(
        vec![
            (
                "path",
                s_string(
                    "Workspace-scoped path to edit. Relative paths resolve from cwd. PolicyFs retains mutation authority with no ambient path reopening; external and symlink escapes are rejected.",
                ),
            ),
            ("edits", s_array(edit_obj, "Targeted replacements")),
        ],
        vec!["path", "edits"],
    );
    AgentTool::new("edit", "Edit a workspace-scoped file through retained PolicyFs authority using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit. External and symlink escapes are rejected.", params, move |ctx| {
        let workspace = workspace.clone();
        let capture = capture.clone();
        async move {
            let generation = capture()?;
            run_edit_captured(&workspace, &generation, ctx.arguments, ctx.abort).await
        }
    })
    .with_capability(ToolCapability::Write)
    .with_prepare_arguments(prepare_edit_arguments)
}

async fn run_edit_captured(
    workspace: &crate::WorkspaceRoots,
    generation: &Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    let path = arg_str(&args, "path");
    let raw_edits = args
        .get("edits")
        .and_then(Value::as_array)
        .filter(|edits| !edits.is_empty())
        .ok_or_else(|| {
            anyhow!("Edit tool input is invalid. edits must contain at least one replacement.")
        })?;
    let edits = raw_edits
        .iter()
        .map(|edit| EditEntry {
            old_text: edit
                .get("oldText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            new_text: edit
                .get("newText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        })
        .collect::<Vec<_>>();
    let absolute = PathBuf::from(resolve_mutation_path(&path, workspace)?);
    let fs = captured_policy_fs(generation)?;
    let opened = fs.open_file(&absolute)?;
    let identity = opened.identity();
    let (root_identity, relative) = fs.mutation_coordinate(&absolute)?;
    with_policy_mutation_queue(fs.revision(), root_identity, &relative, || async {
        check_aborted(&abort)?;
        let data = opened.read_bounded(64 * 1024 * 1024)?;
        let text = String::from_utf8(data).map_err(|_| {
            anyhow!("{path} is not valid UTF-8. Edit refuses to rewrite the file and leaves it unchanged.")
        })?;
        let (bom, raw) = strip_bom(&text);
        let ending = detect_line_ending(&raw);
        let normalized = normalize_to_lf(&raw);
        let (base, new_content) = apply_edits_to_normalized_content(&normalized, &edits, &path)?;
        let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
        fs.replace_file(crate::fs::PolicyReplacement {
            path: absolute.clone(),
            expected: Some(identity),
            bytes: final_content.into_bytes(),
            mode: crate::fs::FileMode::Shared,
        })?;
        check_aborted(&abort)?;
        let details = generate_edit_details(&path, &base, &new_content);
        let mut result = text_result(format!("Successfully replaced {} block(s) in {}.", edits.len(), path));
        result.details = json!({ "diff": details.diff, "patch": details.patch, "firstChangedLine": details.first_changed_line });
        Ok(result)
    }).await
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
            (
                "oldText",
                s_string(
                    "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.",
                ),
            ),
            (
                "newText",
                s_string("Replacement text for this targeted edit."),
            ),
        ],
        vec!["oldText", "newText"],
    );
    let params = s_object(
        vec![
            (
                "path",
                s_string(
                    "Workspace-scoped path to edit. Relative paths resolve from cwd. PolicyFs retains mutation authority with no ambient path reopening; external and symlink escapes are rejected.",
                ),
            ),
            (
                "edits",
                s_array(
                    edit_obj,
                    "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                ),
            ),
        ],
        vec!["path", "edits"],
    );
    AgentTool::new(
        "edit",
        "Edit a workspace-scoped file through retained PolicyFs authority using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. External and symlink escapes are rejected.",
        params,
        move |_ctx| async move { Err(anyhow!("edit requires an application generation")) },
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

async fn run_edit(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("edit requires an application generation"))
}

pub(crate) fn check_aborted(abort: &AbortSignal) -> Result<()> {
    if abort.is_aborted() {
        return Err(anyhow!("Operation aborted"));
    }
    Ok(())
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
    let old = args
        .get("oldText")
        .and_then(|v| v.as_str())
        .map(String::from);
    let new = args
        .get("newText")
        .and_then(|v| v.as_str())
        .map(String::from);
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

/// Closed `input` union: a string (including empty) or JSON `false`.
/// `false` means no stdin. `true` is not a member.
fn bash_input_schema() -> Schema {
    Schema {
        description: Some(
            "Optional stdin. A string is written to the command (followed by a newline) before its output is read; an empty string is preserved and writes no stdin. JSON false means no stdin. true is not accepted. Intended for interactive commands with pty=true (for example a sudo password); ignored otherwise."
                .to_owned(),
        ),
        any_of: vec![
            s_string(
                "Stdin text, including the empty string. Non-empty text is written followed by a newline; an empty string writes no stdin.",
            ),
            Schema {
                const_value: Some(Value::Bool(false)),
                description: Some("false means no stdin. true is not accepted.".to_owned()),
                ..Schema::default()
            },
        ],
        ..Schema::default()
    }
}

/// Bash `input` parser. Does not use [`arg_str`], which treats every
/// non-string (including `true`) as an empty string.
///
/// Missing and JSON `false` mean no stdin. A string, including empty, is
/// preserved: only a non-empty string is written. `true` and every other
/// JSON type are rejected.
fn bash_stdin(args: &Value) -> Result<Option<String>> {
    match args.get("input") {
        None | Some(Value::Bool(false)) => Ok(None),
        Some(Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
        Some(_) => Err(anyhow!(
            "invalid input: must be a string or false (false means no stdin)"
        )),
    }
}

fn bash_tool_with_generation(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    process: Option<BashProcessContext>,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let capture_for_execute = capture.clone();
    bash_tool_captured(cwd, session_env, process, Some(capture_for_execute)).with_prepare_arguments(
        move |arguments| {
            let _generation = capture()?;
            Ok(arguments)
        },
    )
}

fn bash_tool(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    process: Option<BashProcessContext>,
) -> AgentTool {
    bash_tool_captured(cwd, session_env, process, None)
}

fn bash_tool_captured(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    process: Option<BashProcessContext>,
    capture: Option<GenerationCaptureFn>,
) -> AgentTool {
    let cwd = cwd.to_string();
    let params = s_object(
        vec![
            (
                "command",
                s_string(
                    "Bash command to execute. Do not use nohup, setsid, disown, or shell background '&' syntax.",
                ),
            ),
            (
                "timeout",
                s_number("Timeout in seconds (optional, no default timeout)"),
            ),
            (
                "background",
                s_boolean(
                    "Start the command under the supervised process manager, return immediately, and list it in /ps",
                ),
            ),
            (
                "pty",
                s_boolean(
                    "When true, run the command in a pseudo-terminal (PTY) so interactive programs like sudo can prompt for input (e.g. a password). The PTY merges stdout and stderr; provide stdin up front via the input parameter. Not available with background=true.",
                ),
            ),
            (
                "input",
                bash_input_schema(),
            ),
        ],
        vec!["command"],
    );
    let description = format!(
        "Execute a bash command in the current working directory. Finite commands run in the foreground and return stdout and stderr exactly as before. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. For interactive commands that prompt (e.g. sudo), set pty=true and pass stdin via the input parameter (a string, or false for no stdin). For servers, watchers, and other long-running commands, set background=true to return a stable supervised process id visible in /ps. Unsupervised nohup, setsid, disown, and shell '&' detachment are rejected.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("bash", description, params, move |ctx| {
        let cwd = cwd.clone();
        let session_env = session_env.clone();
        let process = process.clone();
        let capture = capture.clone();
        async move {
            run_bash(
                &cwd,
                ctx.arguments,
                ctx.on_update,
                ctx.abort,
                session_env,
                process,
                capture,
            )
            .await
        }
    })
    .with_capability(ToolCapability::Exec)
    .with_prompt_guidelines(vec![
        "Inspect PI_* environment variables for current model and session details.".to_string(),
        "Use foreground bash only for finite commands. For servers, watchers, or commands intended to outlive the tool call, set background=true; never use nohup, setsid, disown, or shell '&'. Supervised commands are visible in /ps with logs, signal, stop, and wait controls.".to_string(),
        "Interactive commands that prompt for input (e.g. sudo) run with pty=true; provide the stdin (e.g. a password) via the input parameter. JSON false means no stdin; do not pass true. PTY is a foreground-only mode and cannot be combined with background=true.".to_string(),
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

/// Resolves bash through the captured generation and returns the canonical
/// executable path that the foreground spawn will retain and revalidate.
fn get_shell_config(
    generation: &crate::application::generation::ApplicationGeneration,
) -> Result<(String, Vec<String>)> {
    let shell =
        crate::process::run_trusted_tool_probe(generation, crate::process::TrustedToolProbe::Bash)?;
    Ok((
        shell.as_path().to_string_lossy().into_owned(),
        vec!["-c".to_owned()],
    ))
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
    live_stream_bytes: usize,
}

impl BashState {
    fn new(
        spill_authority: BashSpillAuthority,
        on_update: Option<ToolUpdateFn>,
        on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    ) -> Self {
        Self {
            acc: OutputAccumulator::new(spill_authority, 0, 0, "pi-bash"),
            on_update,
            on_chunk,
            last_emit: None,
            live_stream_bytes: 0,
        }
    }
    fn append(&mut self, data: &[u8]) {
        self.acc.append(data);
        // Raw merged chunks in arrival order, bounded per command. The
        // accumulator above still drains and accounts for all later output.
        if let Some(on_chunk) = &self.on_chunk {
            let chunk = bounded_bash_live_chunk(self.live_stream_bytes, data);
            if !chunk.is_empty() {
                self.live_stream_bytes = self.live_stream_bytes.saturating_add(chunk.len());
                on_chunk(chunk);
            }
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
        let Some(on_update) = &self.on_update else {
            return;
        };
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

fn bounded_bash_live_chunk(bytes_sent: usize, data: &[u8]) -> &[u8] {
    let remaining = BASH_LIVE_STREAM_MAX_BYTES.saturating_sub(bytes_sent);
    &data[..data.len().min(remaining)]
}
/// How a `run_bash_core` command resolved, separating the start boundary from
/// execution.
enum BashRunEnd {
    /// The command crossed the start boundary (session-tree brush lock
    /// acquired for danger-mode brush; launch authorized otherwise) and ran
    /// to one of the execution outcomes.
    Started(BashExecutionOutcome),
    /// The caller aborted before the command started; nothing ran.
    AbortedBeforeStart,
}

/// The low-level outcome of running a command: bounded output snapshot plus the
/// start boundary end state. Spawn and wait I/O errors propagate as `Err`;
/// nonzero exit and abort are encoded in the outcome so each wrapper can map
/// them to its own result shape.
struct BashRunOutput {
    content: String,
    truncation: TruncationResult,
    full_output_path: String,
    disk_truncated: bool,
    end: BashRunEnd,
    last_line_bytes: usize,
}

struct BashExecutionOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    /// Monotonic measured execution duration from the acquired/authorized
    /// launch boundary; `None` only when the command never started. The
    /// caller's queue wait for the session-tree brush lock is never part of it.
    execution_elapsed: Option<Duration>,
}

/// A `run_bash_core` output for a command that never crossed the start
/// boundary: no output to publish, no spill file created.
fn bash_run_output_without_execution(end: BashRunEnd) -> BashRunOutput {
    BashRunOutput {
        content: String::new(),
        truncation: TruncationResult::default(),
        full_output_path: String::new(),
        disk_truncated: false,
        end,
        last_line_bytes: 0,
    }
}
/// Runs `command` in `cwd`, streams merged output through the accumulator, and
/// enforces timeout and cancellation. PTY mode is opt-in for interactive tools.
/// Returns the bounded output snapshot plus the start-boundary end state; an
/// abort mid-run wins over an execution timeout, the queue wait never fails
/// the run, and neither wait time nor a pre-start abort is execution time.
async fn run_bash_core(
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    auth_manager: Option<Arc<crate::AuthManager>>,
    cwd: &str,
    command: &str,
    session_env: Option<&SessionEnvFn>,
    timeout: Option<f64>,
    abort: AbortSignal,
    on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    on_update: Option<ToolUpdateFn>,
    pty: bool,
    pty_input: Option<String>,
) -> Result<BashRunOutput> {
    let (shell, shell_args) = get_shell_config(&generation)?;
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
    // Confined modes always execute through the governed subprocess boundary
    // and never queue on the brush lock: their children are governed
    // subprocesses, so brush descendant attribution never crosses their runs.
    let is_confined = generation.sandbox().policy_snapshot().policy().mode()
        != pi_sandbox::SandboxMode::DangerFullAccess;
    // Danger-mode brush is scheduled one-at-a-time on this generation's lock.
    // The lock is not descendant ownership: each run already roots reaping at
    // its own `pi-brush-bash` TID. Coordinator and durable-subagent sessions
    // own distinct locks. The queue wait is unbounded and abort-responsive:
    // the holder is bounded by its own timeout or the user's Stop, so a
    // queued command must run rather than fail at an arbitrary bound.
    // Confined subprocess and governed PTY runs bypass the lock.
    let timeout_duration = timeout.map(Duration::from_secs_f64);
    let mut _exec_guard = None;
    if !pty && !is_confined {
        match bash::brush::acquire_bash_exec_lock(generation.brush_exec_lock(), &abort).await? {
            bash::brush::BashExecLockStart::Acquired(guard) => {
                // Hold until this run finishes so same-session bash stays queued.
                _exec_guard = Some(guard);
            }
            bash::brush::BashExecLockStart::Aborted => {
                // Aborted while queued; nothing ran.
                return Ok(bash_run_output_without_execution(BashRunEnd::AbortedBeforeStart));
            }
        }
    }
    let spill = generation.sandbox().scratch().create_bash_output()?;
    let state = Arc::new(Mutex::new(BashState::new(
        BashSpillAuthority::new(spill),
        on_update,
        on_chunk,
    )));
    let stream: Arc<dyn Fn(&[u8]) + Send + Sync> = {
        let state = state.clone();
        Arc::new(move |data: &[u8]| state.lock().append(data))
    };
    // PTY mode (opt-in, `pty: true`) runs the command in a pseudo-terminal so
    // interactive programs like sudo can prompt. The PTY environment keeps the
    // normal command contract except TERM: NON_INTERACTIVE_COMMAND_ENV sets
    // TERM=dumb, which breaks interactive rendering — a PTY needs a real
    // terminal type. Built up front because `run_normal` below takes
    // ownership of `env`; `run_bash` rejects pty with background=true.
    let pty_env: Vec<(String, String)> = if pty
        && generation.sandbox().policy_snapshot().policy().mode()
            == pi_sandbox::SandboxMode::DangerFullAccess
    {
        let mut pty_env = env.clone();
        pty_env.retain(|(key, _)| key != "TERM");
        pty_env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
        pty_env
    } else if pty {
        NON_INTERACTIVE_COMMAND_ENV
            .iter()
            .filter(|(key, _)| bash::foreground::is_confined_safe_pty_requested_name(key))
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    } else {
        Vec::new()
    };
    let pty_process =
        crate::process::GovernedProcessContext::new(generation.clone(), auth_manager.clone());
    let run_normal = async {
        use bash::brush::BrushRunOutcome;
        if is_confined {
            let outcome = bash::foreground::run_confined_command(
                generation.clone(),
                auth_manager.clone(),
                Path::new(cwd),
                &argv,
                &env,
                timeout_duration,
                abort.clone(),
                stream.clone(),
            )
            .await?;
            return Ok::<BashExecutionOutcome, anyhow::Error>(BashExecutionOutcome {
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                cancelled: outcome.cancelled,
                execution_elapsed: outcome.execution_elapsed,
            });
        }
        Ok::<BashExecutionOutcome, anyhow::Error>(
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
                BrushRunOutcome::Executed {
                    exit_code,
                    execution_elapsed,
                } => BashExecutionOutcome {
                    exit_code,
                    timed_out: false,
                    cancelled: false,
                    execution_elapsed: Some(execution_elapsed),
                },
                BrushRunOutcome::TimedOut { execution_elapsed } => BashExecutionOutcome {
                    exit_code: None,
                    timed_out: true,
                    cancelled: false,
                    execution_elapsed: Some(execution_elapsed),
                },
                BrushRunOutcome::Cancelled { execution_elapsed } => BashExecutionOutcome {
                    exit_code: None,
                    timed_out: false,
                    cancelled: true,
                    execution_elapsed: Some(execution_elapsed),
                },
                BrushRunOutcome::Fallback => {
                    let outcome = bash::foreground::run_confined_command(
                        generation.clone(),
                        auth_manager.clone(),
                        Path::new(cwd),
                        &argv,
                        &env,
                        timeout_duration,
                        abort.clone(),
                        stream.clone(),
                    )
                    .await?;
                    BashExecutionOutcome {
                        exit_code: outcome.exit_code,
                        timed_out: outcome.timed_out,
                        cancelled: outcome.cancelled,
                        execution_elapsed: outcome.execution_elapsed,
                    }
                }
            },
        )
    };

    let outcome = if pty {
        let outcome = bash::pty::run_pty_command(
            pty_process,
            Path::new(cwd),
            &argv,
            &pty_env,
            pty_input.as_deref(),
            timeout_duration,
            abort.clone(),
            stream.clone(),
        )
        .await?;
        BashExecutionOutcome {
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
            execution_elapsed: outcome.execution_elapsed,
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

    let end = if outcome.cancelled && outcome.execution_elapsed.is_none() {
        // The abort fired at the start boundary (pre-launch entry checks in
        // the confined/PTY runners); nothing ran.
        BashRunEnd::AbortedBeforeStart
    } else {
        BashRunEnd::Started(outcome)
    };
    Ok(BashRunOutput {
        content: snap.content,
        truncation: snap.truncation,
        full_output_path: snap.full_output_path,
        disk_truncated: snap.disk_truncated,
        end,
        last_line_bytes,
    })
}

/// Analysis bounds for the structural supervision classifier. Parsing is
/// linear, substitution bodies are re-parsed once per scope (so total work
/// stays linear in the command size for bounded nesting), and every bound is
/// enforced before or during the walk with fail-closed behavior.
const MAX_SUPERVISION_COMMAND_BYTES: usize = 512 * 1024;
const MAX_SUBSTITUTION_DEPTH: u32 = 64;
const MAX_AST_DEPTH: u32 = 64;
const MAX_PARSER_DELIMITER_DEPTH: u32 = 128;
const MAX_SUBSTITUTION_SCANS: u32 = 4096;
const MAX_SUPERVISION_AST_NODES: u32 = 200_000;

/// Copy used whenever detach constructs were found inside a command-
/// substitution scope (its shell is foregrounded by the enclosing command,
/// so such children always outlive the substitution). This is the exact
/// historical substitution-supervision error; do not reword.
const SUBSTITUTION_SUPERVISION_ERROR: &str = "Cannot safely determine supervision for shell substitution containing background or control syntax. Move the long-lived command out of the substitution and run it with background=true so it remains visible in /ps.";

/// Copy used when analysis bounds are exceeded and the command cannot be
/// classified within guaranteed space/time. Fail closed.
const BAILOUT_SUPERVISION_ERROR: &str = "Cannot safely determine supervision for the command because it exceeds supervision analysis limits. Simplify the command, then run any long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls.";

/// Structural supervision analysis for one bash command line, based on the
/// shell grammar of the embedded Brush parser (`brush_parser`): async `&`
/// separators, `coproc`, `<(...)`/`>(...)` process substitutions, and
/// `nohup`/`setsid`/`disown` command words are detach constructs at every
/// nested command-substitution scope, while foreground control syntax
/// (`;`, `&&`, finite loops, groups), comments, heredoc bodies (unless the
/// parsed delimiter requires expansion), quoted text, ANSI-C quotes, and
/// arithmetic are shell data or foreground constructs, not detachment.
#[derive(Default)]
struct ShellDetachAnalysis {
    /// Top-level `&` list separators (each runs its preceding list
    /// asynchronously); subshell `&` also lands here because the subshell is
    /// itself foregrounded, so those children are orphaned on exit.
    async_count: u32,
    /// True when a single top-level `&` terminates the last list item so
    /// `background=true` normalization can strip it.
    terminal_async: bool,
    /// Top-level detach command words (`nohup`/`setsid`/`disown`).
    nohup_count: u32,
    setsid_count: u32,
    disown_count: u32,
    /// The first top-level command is `nohup` (normalizable only when
    /// `nohup_count == 1`).
    leading_nohup: bool,
    /// That leading `nohup` is directly followed by a `--` separator word.
    leading_nohup_dashdash: bool,
    /// Detach found inside a command-substitution scope: the substitution
    /// shell is foregrounded, so backgrounded children always outlive it.
    nested_background: bool,
    /// Detach keywords found inside command-substitution scopes.
    nested_keywords: Vec<&'static str>,
    /// A `$()`/backtick appears inside an arithmetic or parameter expansion
    /// whose grammar does not expose the content for classification; fail
    /// closed.
    opaque_expansion: bool,
    /// `coproc` is asynchronous by definition.
    coproc: bool,
    /// `<(...)` / `>(...)` process substitutions run children bash does not
    /// generally wait for.
    process_substitution: u32,
    /// Brush could not parse the command; see the raw-marker fallback in the
    /// bash runner (parse errors settle without execution, so marker-free
    /// inputs may still fall through to the governed runner's parse error).
    parse_failed: bool,
    /// Analysis bounds (command size, substitution depth/count, AST nodes) or
    /// word-piece decomposition failed; classification is unsafe. Fail closed.
    unclassifiable: bool,
}

impl ShellDetachAnalysis {
    fn has_detach_intent(&self) -> bool {
        self.async_count > 0
            || self.nohup_count > 0
            || self.setsid_count > 0
            || self.disown_count > 0
            || self.coproc
            || self.process_substitution > 0
            || self.nested_background
            || !self.nested_keywords.is_empty()
            || self.opaque_expansion
    }

    fn detected_constructs(&self) -> String {
        fn has_keyword(keywords: &[&'static str], name: &str) -> bool {
            keywords.iter().any(|keyword| keyword == &name)
        }
        let mut constructs = Vec::new();
        if self.nohup_count > 0 || has_keyword(&self.nested_keywords, "nohup") {
            constructs.push("nohup");
        }
        if self.setsid_count > 0 || has_keyword(&self.nested_keywords, "setsid") {
            constructs.push("setsid");
        }
        if self.disown_count > 0 || has_keyword(&self.nested_keywords, "disown") {
            constructs.push("disown");
        }
        if self.coproc || has_keyword(&self.nested_keywords, "coproc") {
            constructs.push("coproc");
        }
        if self.process_substitution > 0 {
            constructs.push("process substitution");
        }
        if self.async_count > 0 || self.nested_background {
            constructs.push("shell background '&'");
        }
        constructs.join(", ")
    }

    /// Maps a classified detach intent to its supervision error: detach
    /// constructs inside a foregrounded command-substitution scope (or inside
    /// an expansion whose grammar hides its content) keep the historical
    /// substitution-specific copy; every top-level construct keeps the
    /// historical generic copy byte-for-byte.
    fn classify_rejection(&self) -> Option<anyhow::Error> {
        if self.nested_background
            || !self.nested_keywords.is_empty()
            || self.opaque_expansion
        {
            return Some(anyhow!("{SUBSTITUTION_SUPERVISION_ERROR}"));
        }
        if self.has_detach_intent() {
            return Some(anyhow!(
                "Unsupervised background or detached bash execution is not allowed (detected {}). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls.",
                self.detected_constructs()
            ));
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetachScope {
    /// The program scope the bash runner will execute in the foreground.
    Top,
    /// Inside a command substitution; bash foregrounds its shell, so any
    /// asynchronous child it starts is orphaned when the substitution ends.
    Substitution,
}

#[derive(Default)]
struct WalkBudget {
    nodes: u32,
    substitution_scans: u32,
}

impl WalkBudget {
    /// Charges one walk step. Unlike [`Self::charge_flag`], the
    /// unclassifiable flag does not stop the charge: a dynamic command word
    /// fails open for its own site only and later definite sites must still
    /// classify (`found` wins at the end).
    fn charge_node(&mut self, unclassifiable: &mut bool) -> bool {
        self.nodes += 1;
        if self.nodes > MAX_SUPERVISION_AST_NODES {
            *unclassifiable = true;
            return false;
        }
        true
    }

    fn charge_flag(&mut self, unclassifiable: &mut bool) -> bool {
        if *unclassifiable {
            return false;
        }
        self.charge_node(unclassifiable)
    }

    fn charge(&mut self, analysis: &mut ShellDetachAnalysis) -> bool {
        self.charge_flag(&mut analysis.unclassifiable)
    }
}

fn supervise_parser_options() -> brush_parser::ParserOptions {
    brush_parser::ParserOptions::default()
}

fn parse_shell_supervision_ast(
    script: &str,
) -> Result<brush_parser::ast::Program, brush_parser::ParseError> {
    let mut parser =
        brush_parser::Parser::new(script.as_bytes(), &supervise_parser_options());
    parser.parse_program()
}

fn parser_structural_depth_exceeded(
    script: &str,
) -> Result<bool, brush_parser::TokenizerError> {
    use brush_parser::Token;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum StructuralOpen {
        Paren,
        Brace,
        If,
        Case,
        Loop,
    }

    fn close(stack: &mut Vec<StructuralOpen>, expected: StructuralOpen) {
        if let Some(index) = stack.iter().rposition(|open| *open == expected) {
            stack.truncate(index);
        }
    }

    let tokens = brush_parser::tokenize_str(script)?;
    if tokens.len() > MAX_SUPERVISION_AST_NODES as usize {
        return Ok(true);
    }

    // Grammar-position tracking: shell reserved words are only structural in
    // command position. A list separator or pipe (`\n`, `;`, `&`, `&&`, `||`,
    // `|`) opens a fresh command position; a closer token (`)`, `fi`,
    // `done`, `esac`) re-opens one so separators/case/function continuations
    // after it are recognized; inside a simple command every other word is a
    // plain argument and can never change structural state.
    let mut expect_command_word = true;
    let mut stack = Vec::new();
    for token in tokens {
        match &token {
            Token::Operator(operator, _) => match operator.as_str() {
                "(" => {
                    stack.push(StructuralOpen::Paren);
                    expect_command_word = true;
                }
                ")" => {
                    close(&mut stack, StructuralOpen::Paren);
                    expect_command_word = true;
                }
                ";" | ";;" | ";&" | ";;&" | "\n" | "&" | "&&" | "||" | "|" | "|&" => {
                    expect_command_word = true;
                }
                _ => {}
            },
            Token::Word(word, _) => {
                if !expect_command_word {
                    // Ordinary word inside a simple command: arguments that
                    // merely look like reserved closers (`echo fi`) can never
                    // change the structural nesting state.
                    continue;
                }
                match word.as_str() {
                    "if" => stack.push(StructuralOpen::If),
                    "case" => stack.push(StructuralOpen::Case),
                    "for" | "select" | "while" | "until" => {
                        stack.push(StructuralOpen::Loop);
                    }
                    "{" => stack.push(StructuralOpen::Brace),
                    "}" => {
                        close(&mut stack, StructuralOpen::Brace);
                        // Connector continuation after a brace group.
                        expect_command_word = true;
                    }
                    "fi" => {
                        close(&mut stack, StructuralOpen::If);
                        expect_command_word = true;
                    }
                    "done" => {
                        close(&mut stack, StructuralOpen::Loop);
                        expect_command_word = true;
                    }
                    "esac" => {
                        close(&mut stack, StructuralOpen::Case);
                        expect_command_word = true;
                    }
                    // Pipeline prefixes (`!`, `time`) keep command mode: they
                    // are followed by another command in the same pipeline.
                    "!" | "time" => {}
                    // Compound-open words (`then`/`else`/`elif`/`do`) and the
                    // `case` `in` separator start command lists or patterns,
                    // so stay in command mode.
                    "then" | "else" | "elif" | "do" | "in" => {}
                    _ => expect_command_word = false,
                }
            }
        }
        if stack.len() > MAX_PARSER_DELIMITER_DEPTH as usize {
            return Ok(true);
        }
    }
    Ok(false)
}

fn shell_detach_analysis(command: &str) -> ShellDetachAnalysis {
    let mut analysis = ShellDetachAnalysis::default();
    if command.len() > MAX_SUPERVISION_COMMAND_BYTES {
        analysis.unclassifiable = true;
        return analysis;
    }
    match parser_structural_depth_exceeded(command) {
        Ok(false) => {}
        Ok(true) => {
            analysis.unclassifiable = true;
            return analysis;
        }
        Err(_) => {
            analysis.parse_failed = true;
            return analysis;
        }
    }
    let mut budget = WalkBudget::default();
    let walk = Walk;
    match parse_shell_supervision_ast(command) {
        Err(_) => analysis.parse_failed = true,
        Ok(program) => {
            walk.detach_program(&program, 0, DetachScope::Top, &mut analysis, &mut budget);
            if analysis.nohup_count > 0 {
                let (leading, dashdash) = detect_leading_nohup(&program);
                analysis.leading_nohup = leading;
                analysis.leading_nohup_dashdash = dashdash;
            }
        }
    }
    analysis
}

/// Reports whether the first top-level command is literally `nohup` (and
/// whether a `--` separator word follows it directly). Only such a single,
/// leading wrapper is normalizable for `background=true`.
fn detect_leading_nohup(program: &brush_parser::ast::Program) -> (bool, bool) {
    use brush_parser::ast::CommandPrefixOrSuffixItem;
    let Some(first) = program.complete_commands.first() else {
        return (false, false);
    };
    let Some(item) = first.0.first() else {
        return (false, false);
    };
    let Some(first_pipeline_command) = item.0.first.seq.first() else {
        return (false, false);
    };
    let brush_parser::ast::Command::Simple(simple) = first_pipeline_command else {
        return (false, false);
    };
    // An assignment prefix means `nohup` is not the first token, mirroring
    // the historical leading-wrapper rule; normalization fails closed then.
    if simple.prefix.as_ref().is_some_and(|prefix| !prefix.0.is_empty()) {
        return (false, false);
    }
    let Some(word) = &simple.word_or_name else {
        return (false, false);
    };
    if word.value != "nohup" {
        return (false, false);
    }
    let dashdash = simple
        .suffix
        .as_ref()
        .and_then(|suffix| suffix.0.first())
        .and_then(|item| match item {
            CommandPrefixOrSuffixItem::Word(word) => Some(word.value.as_str()),
            _ => None,
        })
        .is_some_and(|value| value == "--");
    (true, dashdash)
}

fn guard_depth(depth: u32, unclassifiable: &mut bool) -> bool {
    if depth > MAX_AST_DEPTH {
        *unclassifiable = true;
        return false;
    }
    true
}

fn guard_ast_depth(depth: u32, analysis: &mut ShellDetachAnalysis) -> bool {
    guard_depth(depth, &mut analysis.unclassifiable)
}

/// One word piece with its position inside the word text. A concrete type so
/// the recursive piece walker (and its nested quoted sequences) never
/// instantiates a new generic recursion level.
struct WordPieceRef<'a> {
    piece: &'a brush_parser::word::WordPiece,
    start: usize,
    end: usize,
}


/// Walks one program scope (`top` execution scope or a command-substitution
/// body re-parsed with the same authoritative grammar), recording detach
/// constructs in `analysis`.
struct Walk;

impl Walk {
    fn detach_program(
        &self,
        program: &brush_parser::ast::Program,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        if !guard_ast_depth(depth, analysis) {
            return;
        }
        for complete_command in &program.complete_commands {
            self.detach_compound_list(complete_command, depth, scope, analysis, budget);
        }
    }

    fn detach_compound_list(
        &self,
        list: &brush_parser::ast::CompoundList,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        let items = &list.0;
        for (index, item) in items.iter().enumerate() {
            if !budget.charge(analysis) {
                return;
            }
            self.detach_and_or_list(&item.0, depth, scope, analysis, budget);
            // An async separator always launches a child bash does not
            // supervise through the foreground call: at the top scope it is
            // the list separator; inside a substitution shell it orphans the
            // child when the substitution exits.
            if !matches!(item.1, brush_parser::ast::SeparatorOperator::Async) {
                continue;
            }
            match scope {
                DetachScope::Top => {
                    analysis.async_count += 1;
                    if items.len() == 1 && index == items.len() - 1 {
                        analysis.terminal_async = true;
                    }
                }
                DetachScope::Substitution => analysis.nested_background = true,
            }
        }
    }

    fn detach_and_or_list(
        &self,
        and_or: &brush_parser::ast::AndOrList,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        self.detach_pipeline(&and_or.first, depth, scope, analysis, budget);
        for additional in &and_or.additional {
            let pipeline = match additional {
                brush_parser::ast::AndOr::And(pipeline) | brush_parser::ast::AndOr::Or(pipeline) => {
                    pipeline
                }
            };
            self.detach_pipeline(pipeline, depth, scope, analysis, budget);
        }
    }

    fn detach_pipeline(
        &self,
        pipeline: &brush_parser::ast::Pipeline,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        for command in &pipeline.seq {
            self.detach_command(command, depth, scope, analysis, budget);
        }
    }

    fn detach_command(
        &self,
        command: &brush_parser::ast::Command,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        if !guard_ast_depth(depth, analysis) {
            return;
        }
        match command {
            brush_parser::ast::Command::Simple(simple) => {
                self.detach_simple(simple, depth, scope, analysis, budget)
            }
            brush_parser::ast::Command::Compound(compound, redirects) => {
                self.detach_compound(compound, depth + 1, scope, analysis, budget);
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        self.detach_redirect(redirect, depth, scope, analysis, budget);
                    }
                }
            }
            brush_parser::ast::Command::Function(definition) => {
                self.detach_compound(&definition.body.0, depth + 1, scope, analysis, budget);
                if let Some(redirects) = &definition.body.1 {
                    for redirect in &redirects.0 {
                        self.detach_redirect(redirect, depth, scope, analysis, budget);
                    }
                }
            }
            brush_parser::ast::Command::ExtendedTest(extended, redirects) => {
                self.detach_extended_test(&extended.expr, depth + 1, scope, analysis, budget);
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        self.detach_redirect(redirect, depth, scope, analysis, budget);
                    }
                }
            }
        }
    }

    fn detach_extended_test(
        &self,
        expr: &brush_parser::ast::ExtendedTestExpr,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        if !guard_ast_depth(depth, analysis) {
            return;
        }
        match expr {
            brush_parser::ast::ExtendedTestExpr::And(left, right)
            | brush_parser::ast::ExtendedTestExpr::Or(left, right) => {
                self.detach_extended_test(left, depth + 1, scope, analysis, budget);
                self.detach_extended_test(right, depth + 1, scope, analysis, budget);
            }
            brush_parser::ast::ExtendedTestExpr::Not(inner)
            | brush_parser::ast::ExtendedTestExpr::Parenthesized(inner) => {
                self.detach_extended_test(inner, depth + 1, scope, analysis, budget);
            }
            brush_parser::ast::ExtendedTestExpr::UnaryTest(_, word) => {
                self.detach_word(word, depth, scope, analysis, budget);
            }
            brush_parser::ast::ExtendedTestExpr::BinaryTest(_, left, right) => {
                self.detach_word(left, depth, scope, analysis, budget);
                self.detach_word(right, depth, scope, analysis, budget);
            }
        }
    }

    fn detach_compound(
        &self,
        compound: &brush_parser::ast::CompoundCommand,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        if !guard_ast_depth(depth, analysis) {
            return;
        }
        match compound {
            // Arithmetic grammar: `&` is bitwise-AND, never an operator. Its
            // raw text may still embed `$()`/backticks the grammar cannot
            // expand for classification; those fail closed below.
            brush_parser::ast::CompoundCommand::Arithmetic(arithmetic) => {
                self.check_arithmetic_text(&arithmetic.expr.value, analysis)
            }
            brush_parser::ast::CompoundCommand::ArithmeticForClause(clause) => {
                for expr in [&clause.initializer, &clause.condition, &clause.updater]
                    .into_iter()
                    .flatten()
                {
                    self.check_arithmetic_text(&expr.value, analysis)
                }
                self.detach_compound_list(&clause.body.list, depth, scope, analysis, budget);
            }
            brush_parser::ast::CompoundCommand::BraceGroup(group) => {
                self.detach_compound_list(&group.list, depth, scope, analysis, budget)
            }
            brush_parser::ast::CompoundCommand::Subshell(subshell) => {
                // A subshell is foregrounded by its parent scope, so `&`
                // inside it orphans the child on exit: record at this scope.
                self.detach_compound_list(&subshell.list, depth, scope, analysis, budget)
            }
            brush_parser::ast::CompoundCommand::ForClause(clause) => {
                if let Some(values) = &clause.values {
                    for word in values {
                        self.detach_word(word, depth, scope, analysis, budget);
                    }
                }
                self.detach_compound_list(&clause.body.list, depth, scope, analysis, budget)
            }
            brush_parser::ast::CompoundCommand::CaseClause(clause) => {
                self.detach_word(&clause.value, depth, scope, analysis, budget);
                for case in &clause.cases {
                    if !budget.charge(analysis) {
                        return;
                    }
                    for pattern in &case.patterns {
                        self.detach_word(pattern, depth, scope, analysis, budget);
                    }
                    if let Some(list) = &case.cmd {
                        self.detach_compound_list(list, depth, scope, analysis, budget);
                    }
                }
            }
            brush_parser::ast::CompoundCommand::IfClause(clause) => {
                self.detach_compound_list(&clause.condition, depth, scope, analysis, budget);
                self.detach_compound_list(&clause.then, depth, scope, analysis, budget);
                if let Some(elses) = &clause.elses {
                    for else_clause in elses {
                        if let Some(condition) = &else_clause.condition {
                            self.detach_compound_list(condition, depth, scope, analysis, budget);
                        }
                        self.detach_compound_list(&else_clause.body, depth, scope, analysis, budget);
                    }
                }
            }
            brush_parser::ast::CompoundCommand::WhileClause(clause)
            | brush_parser::ast::CompoundCommand::UntilClause(clause) => {
                self.detach_compound_list(&clause.0, depth, scope, analysis, budget);
                self.detach_compound_list(&clause.1.list, depth, scope, analysis, budget);
            }
            brush_parser::ast::CompoundCommand::Coprocess(coprocess) => {
                // `coproc` runs its command asynchronously in a subshell
                // bash does not supervise through the foreground call.
                match scope {
                    DetachScope::Top => analysis.coproc = true,
                    DetachScope::Substitution => {
                        analysis.nested_background = true;
                        analysis.nested_keywords.push("coproc");
                    }
                }
                if let Some(name) = &coprocess.name {
                    self.detach_word(name, depth, scope, analysis, budget);
                }
                self.detach_command(&coprocess.body, depth, scope, analysis, budget);
            }
        }
    }

    fn check_arithmetic_text(&self, text: &str, analysis: &mut ShellDetachAnalysis) {
        if text.contains("$(") || text.contains('`') {
            analysis.opaque_expansion = true;
        }
    }

    fn record_keyword(
        &self,
        scope: DetachScope,
        keyword: &'static str,
        analysis: &mut ShellDetachAnalysis,
    ) {
        match scope {
            DetachScope::Top => match keyword {
                "nohup" => analysis.nohup_count += 1,
                "setsid" => analysis.setsid_count += 1,
                "disown" => analysis.disown_count += 1,
                "coproc" => analysis.coproc = true,
                _ => {}
            },
            DetachScope::Substitution => analysis.nested_keywords.push(keyword),
        }
    }

    /// Command words that may wrap a following detach keyword
    /// (`env nohup x`, `builtin setsid x`, ...). Mirrors the historical
    /// command-prefix handling.
    fn is_keyword_wrapper(word: &str) -> bool {
        matches!(word, "builtin" | "command" | "env" | "exec")
    }

    fn is_detach_keyword(word: &str) -> Option<&'static str> {
        match word {
            "nohup" => Some("nohup"),
            "setsid" => Some("setsid"),
            "disown" => Some("disown"),
            _ => None,
        }
    }

    fn detach_simple(
        &self,
        simple: &brush_parser::ast::SimpleCommand,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                self.detach_prefix_suffix_item(item, depth, scope, analysis, budget);
            }
        }
        if let Some(word) = &simple.word_or_name {
            if let Some(keyword) = Self::is_detach_keyword(word.value.as_str()) {
                self.record_keyword(scope, keyword, analysis);
            }
            self.detach_word(word, depth, scope, analysis, budget);
        }
        if let Some(suffix) = &simple.suffix {
            // Skip options of wrapper commands when matching the effective
            // command name (`env -i nohup x` still detaches through nohup).
            let mut wrapper_mode = simple
                .word_or_name
                .as_ref()
                .is_some_and(|word| Self::is_keyword_wrapper(&word.value));
            for item in &suffix.0 {
                if wrapper_mode
                    && let brush_parser::ast::CommandPrefixOrSuffixItem::Word(word) = item
                {
                    if word.value.starts_with('-') {
                        self.detach_word(word, depth, scope, analysis, budget);
                        continue;
                    }
                    if let Some(keyword) = Self::is_detach_keyword(word.value.as_str()) {
                        self.record_keyword(scope, keyword, analysis);
                        wrapper_mode = false;
                    }
                }
                self.detach_prefix_suffix_item(item, depth, scope, analysis, budget);
            }
        }
    }

    fn detach_prefix_suffix_item(
        &self,
        item: &brush_parser::ast::CommandPrefixOrSuffixItem,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        match item {
            brush_parser::ast::CommandPrefixOrSuffixItem::Word(word) => {
                self.detach_word(word, depth, scope, analysis, budget)
            }
            brush_parser::ast::CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                self.detach_word(word, depth, scope, analysis, budget)
            }
            brush_parser::ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                self.detach_redirect(redirect, depth, scope, analysis, budget)
            }
            brush_parser::ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                // Process substitutions run children bash does not generally
                // wait for at any scope.
                match scope {
                    DetachScope::Top => analysis.process_substitution += 1,
                    DetachScope::Substitution => {
                        analysis.nested_background = true;
                        analysis.nested_keywords.push("process substitution");
                    }
                }
                self.detach_compound_list(&subshell.list, depth, scope, analysis, budget);
            }
        }
    }

    fn detach_redirect(
        &self,
        redirect: &brush_parser::ast::IoRedirect,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        match redirect {
            brush_parser::ast::IoRedirect::File(_, _, target) => match target {
                brush_parser::ast::IoFileRedirectTarget::Filename(word)
                | brush_parser::ast::IoFileRedirectTarget::Duplicate(word) => {
                    self.detach_word(word, depth, scope, analysis, budget);
                }
                brush_parser::ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                    self.record_process_substitution(scope, analysis);
                    self.detach_compound_list(&subshell.list, depth, scope, analysis, budget);
                }
                brush_parser::ast::IoFileRedirectTarget::Fd(_) => {}
            },
            brush_parser::ast::IoRedirect::HereDocument(_, here_document) => {
                // Heredoc bodies are data: expansions are live only when the
                // parsed delimiter says the body requires expansion. Quotes
                // in expanding bodies are literal characters, so the body is
                // parsed with the heredoc word grammar, not the quoting one.
                if here_document.requires_expansion {
                    self.detach_word_parsed(
                        &here_document.doc,
                        brush_parser::word::parse_heredoc,
                        depth,
                        scope,
                        analysis,
                        budget,
                    );
                }
            }
            brush_parser::ast::IoRedirect::HereString(_, word) => {
                self.detach_word(word, depth, scope, analysis, budget)
            }
            brush_parser::ast::IoRedirect::OutputAndError(word, _) => {
                self.detach_word(word, depth, scope, analysis, budget)
            }
        }
    }

    fn record_process_substitution(&self, scope: DetachScope, analysis: &mut ShellDetachAnalysis) {
        match scope {
            DetachScope::Top => analysis.process_substitution += 1,
            DetachScope::Substitution => {
                analysis.nested_background = true;
                analysis.nested_keywords.push("process substitution");
            }
        }
    }

    /// Walks a word through its parsed pieces: quoted text, ANSI-C quotes,
    /// escapes, and tilde forms are safe; command substitutions are
    /// re-parsed as nested program scopes; expansions whose grammar hides
    /// their content (parameter expansions, arithmetic) fail closed if they
    /// could embed `$()`/backticks.
    fn detach_word(
        &self,
        word: &brush_parser::ast::Word,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if !budget.charge(analysis) {
            return;
        }
        self.detach_word_pieces(word, depth, scope, analysis, budget);
    }

    fn detach_word_pieces(
        &self,
        word: &brush_parser::ast::Word,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        self.detach_word_parsed(word, brush_parser::word::parse, depth, scope, analysis, budget);
    }

    fn detach_word_parsed<E>(
        &self,
        word: &brush_parser::ast::Word,
        parse: fn(
            &str,
            &brush_parser::ParserOptions,
        ) -> Result<Vec<brush_parser::word::WordPieceWithSource>, E>,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        if analysis.unclassifiable {
            return;
        }
        if !budget.charge(analysis) {
            return;
        }
        let pieces = match parse(&word.value, &supervise_parser_options()) {
            Ok(pieces) => pieces,
            Err(_) => {
                analysis.unclassifiable = true;
                return;
            }
        };
        let piece_refs: Vec<WordPieceRef<'_>> = pieces
            .iter()
            .map(|piece| WordPieceRef {
                piece: &piece.piece,
                start: piece.start_index,
                end: piece.end_index,
            })
            .collect();
        self.walk_word_pieces(&piece_refs, &word.value, depth, scope, analysis, budget);
    }

    fn walk_word_pieces(
        &self,
        pieces: &[WordPieceRef<'_>],
        word_text: &str,
        depth: u32,
        scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        for WordPieceRef { piece, start, end } in pieces {
            let (start, end) = (*start, *end);
            if !budget.charge(analysis) {
                return;
            }
            match piece {
                brush_parser::word::WordPiece::DoubleQuotedSequence(inner)
                | brush_parser::word::WordPiece::GettextDoubleQuotedSequence(inner) => {
                    let inner_refs: Vec<WordPieceRef<'_>> = inner
                        .iter()
                        .map(|piece| WordPieceRef {
                            piece: &piece.piece,
                            start: piece.start_index,
                            end: piece.end_index,
                        })
                        .collect();
                    self.walk_word_pieces(
                        &inner_refs,
                        word_text,
                        depth,
                        scope,
                        analysis,
                        budget,
                    );
                }
                brush_parser::word::WordPiece::CommandSubstitution(body)
                | brush_parser::word::WordPiece::BackquotedCommandSubstitution(body) => {
                    self.detach_substitution_body(body, depth, scope, analysis, budget);
                }
                brush_parser::word::WordPiece::ArithmeticExpression(expr) => {
                    self.check_arithmetic_text(&expr.value, analysis);
                }
                brush_parser::word::WordPiece::ParameterExpansion(_) => {
                    self.check_opaque_slice(word_text, start, end, analysis);
                }
                _ => {}
            }
        }
    }

    fn check_opaque_slice(
        &self,
        word_text: &str,
        start: usize,
        end: usize,
        analysis: &mut ShellDetachAnalysis,
    ) {
        let end = end.min(word_text.len());
        let start = start.min(end);
        if let Some(slice) = word_text.get(start..end)
            && (slice.contains("$(") || slice.contains('`'))
        {
            analysis.opaque_expansion = true;
        }
    }

    /// Re-parses one command-substitution body with the authoritative
    /// program grammar so foreground control syntax inside it is visible at
    /// its own lifetime scope while every detach construct is still found.
    fn detach_substitution_body(
        &self,
        body: &str,
        depth: u32,
        _scope: DetachScope,
        analysis: &mut ShellDetachAnalysis,
        budget: &mut WalkBudget,
    ) {
        budget.substitution_scans += 1;
        if depth + 1 > MAX_SUBSTITUTION_DEPTH || budget.substitution_scans > MAX_SUBSTITUTION_SCANS {
            analysis.unclassifiable = true;
            return;
        }
        if body.len() > MAX_SUPERVISION_COMMAND_BYTES {
            analysis.unclassifiable = true;
            return;
        }
        match parse_shell_supervision_ast(body) {
            Err(_) => analysis.unclassifiable = true,
            Ok(program) => {
                self.detach_program(&program, depth + 1, DetachScope::Substitution, analysis, budget)
            }
        }
    }
}

/// Conservative raw-text marker check for commands the grammar could not
/// classify (parse failure or exceeded analysis bounds). Bash never executes
/// anything from an input that fails its own parse, so marker-free inputs may
/// still fall through to the governed runner's parse error; every form that
/// could hide a detach construct marks these categories.
fn unclassified_supervision_error(command: &str) -> Option<anyhow::Error> {
    let has_substitution_marker = command.contains("$(") || command.contains('`');
    let has_control_marker = command.contains('&') || command.contains(';');
    if has_substitution_marker && has_control_marker {
        return Some(anyhow!("{SUBSTITUTION_SUPERVISION_ERROR}"));
    }
    if command.contains('&') {
        return Some(anyhow!(
            "Unsupervised background or detached bash execution is not allowed (detected shell background '&'). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        ));
    }
    let has_keyword_marker = ["nohup", "setsid", "disown", "coproc", "<(", ">("]
        .iter()
        .any(|marker| command.contains(marker));
    if has_keyword_marker {
        return Some(anyhow!(
            "Unsupervised background or detached bash execution is not allowed (detected detach command). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        ));
    }
    None
}

/// Normalizes a `background=true` command that uses the historical
/// synchronous-detach syntax into a plain supervised command. Only a single
/// leading `nohup` wrapper (with optional `--`) plus one terminal `&`
/// qualifies; everything else must be replaced by explicit `background=true`
/// so ProcessManager owns the process and /ps reports it.
fn normalize_supervised_bash(command: &str, analysis: &ShellDetachAnalysis) -> Result<String> {
    if analysis.process_substitution > 0
        || analysis.coproc
        || analysis.setsid_count > 0
        || analysis.disown_count > 0
    {
        return Err(anyhow!(
            "Cannot safely supervise shell detachment using {}. Remove the detach command and run the long-lived command with background=true so it remains visible in /ps.",
            analysis.detected_constructs()
        ));
    }
    if analysis.nohup_count > 0
        && (analysis.nohup_count != 1 || !analysis.leading_nohup)
    {
        return Err(anyhow!(
            "Cannot safely supervise nested or compound nohup execution. Remove nohup and run the long-lived command with background=true so it remains visible in /ps."
        ));
    }
    if analysis.async_count > 0
        && (analysis.async_count != 1 || !analysis.terminal_async)
    {
        return Err(anyhow!(
            "Cannot safely supervise a compound shell background job. Remove shell '&' syntax and run the long-lived command with background=true so it remains visible in /ps."
        ));
    }

    let mut normalized = command.to_owned();

    // Strip the leading `nohup` word (and its direct `--` separator word),
    // leaving the surrounding whitespace exactly as before.
    if analysis.leading_nohup && analysis.nohup_count == 1 {
        let Some(first_word_start) = first_non_whitespace_index(&normalized) else {
            return Err(anyhow!("background bash requires a command to supervise"));
        };
        if !normalized[first_word_start..].starts_with("nohup") {
            return Err(anyhow!(
                "Cannot safely supervise nested or compound nohup execution. Remove nohup and run the long-lived command with background=true so it remains visible in /ps."
            ));
        }
        normalized.replace_range(first_word_start..first_word_start + "nohup".len(), "");
        if analysis.leading_nohup_dashdash {
            let separator_start = first_non_whitespace_index(&normalized)
                .ok_or_else(|| anyhow!("background bash requires a command to supervise"))?;
            let separator_followed_by_whitespace = normalized[separator_start..]
                .get(2..3)
                .is_none_or(|rest| rest.chars().next().is_some_and(char::is_whitespace));
            if normalized[separator_start..].starts_with("--") && separator_followed_by_whitespace
            {
                normalized.replace_range(separator_start..separator_start + 2, "");
            } else {
                return Err(anyhow!(
                    "Cannot safely supervise nested or compound nohup execution. Remove nohup and run the long-lived command with background=true so it remains visible in /ps."
                ));
            }
        }
    }

    // Strip the single terminal `&`. Everything after it must be whitespace.
    if analysis.async_count == 1 {
        let Some(ampersand) = normalized.rfind('&') else {
            return Err(anyhow!(
                "Cannot safely supervise a compound shell background job. Remove shell '&' syntax and run the long-lived command with background=true so it remains visible in /ps."
            ));
        };
        if !normalized[ampersand + 1..].chars().all(char::is_whitespace) {
            return Err(anyhow!(
                "Cannot safely supervise a compound shell background job. Remove shell '&' syntax and run the long-lived command with background=true so it remains visible in /ps."
            ));
        }
        normalized.replace_range(ampersand..ampersand + 1, "");
    }

    if normalized.trim().is_empty() {
        return Err(anyhow!("background bash requires a command to supervise"));
    }
    Ok(normalized)
}

fn first_non_whitespace_index(command: &str) -> Option<usize> {
    command
        .char_indices()
        .find_map(|(index, character)| (!character.is_whitespace()).then_some(index))
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
    capture: Option<GenerationCaptureFn>,
) -> Result<AgentToolResult> {
    let command = required_arg_str(&args, "command")?;
    let timeout = arg_float(&args, "timeout");
    if let Some(t) = timeout {
        if !t.is_finite() || t <= 0.0 {
            return Err(anyhow!(
                "Invalid timeout: must be a finite number of seconds"
            ));
        }
        if t * 1000.0 > MAX_BASH_TIMEOUT_MS {
            return Err(anyhow!(
                "Invalid timeout: maximum is {MAX_BASH_TIMEOUT_SECONDS} seconds"
            ));
        }
    }
    check_aborted(&abort)?;

    let background = arg_bool(&args, "background");
    let pty = arg_bool(&args, "pty");
    let pty_input = bash_stdin(&args)?;
    let detach = shell_detach_analysis(&command);
    if pty && background {
        return Err(anyhow!(
            "pty bash is not supported with background=true: PTY input and output are only available in the foreground. Run the interactive command without background=true."
        ));
    }
    // Analysis-limit bailouts are categorically unsafe: reject before either
    // foreground execution or ProcessManager spawn, independent of raw text.
    if detach.unclassifiable {
        return Err(anyhow!("{BAILOUT_SUPERVISION_ERROR}"));
    }
    // Preserve parse-failure compatibility for ordinary malformed commands:
    // marker-bearing forms fail closed by their historical category; a
    // marker-free syntax error falls through to bash's parse-before-execute
    // gate (which runs nothing).
    if detach.parse_failed
        && let Some(error) = unclassified_supervision_error(&command)
    {
        return Err(error);
    }
    if background {
        // Legacy top-level detach syntax (`nohup ... &`) must reach
        // normalize_supervised_bash; only nested substitution lifetimes are
        // inherently non-normalizable and keep the historical copy here.
        if detach.nested_background
            || !detach.nested_keywords.is_empty()
            || detach.opaque_expansion
        {
            return Err(anyhow!("{SUBSTITUTION_SUPERVISION_ERROR}"));
        }
    } else if let Some(error) = detach.classify_rejection() {
        // Foreground execution rejects every classified detach intent.
        return Err(error);
    }
    if background {
        let process =
            process.ok_or_else(|| anyhow!("background bash is unavailable in this context"))?;
        let command = normalize_supervised_bash(&command, &detach)?;
        let generation = capture
            .as_ref()
            .ok_or_else(|| anyhow!("bash requires an application generation"))?(
        )?;
        let (shell, shell_args) = get_shell_config(&generation)?;
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
        let info = process
            .manager
            .spawn(
                process.owner_id,
                crate::ProcessSpawnSpec {
                    argv,
                    cwd: Path::new(cwd).to_path_buf(),
                    env,
                    tty: false,
                    terminal_size: None,
                    label: None,
                    timeout_ms: timeout.map(|seconds| (seconds * 1000.0) as u64),
                    output_bytes: None,
                },
            )
            .await?;
        return Ok(AgentToolResult {
            content: vec![ContentBlock::text(format!(
                "Process started: {} (visible in /ps)",
                info.id
            ))],
            details: serde_json::to_value(info)?,
            ..Default::default()
        });
    }
    // pi emits an initial empty update before spawning.
    (on_update)(AgentToolResult::default());
    let generation = capture
        .as_ref()
        .ok_or_else(|| anyhow!("bash requires an application generation"))?()?;
    let auth_manager = process
        .as_ref()
        .and_then(|context| context.auth_manager.clone());
    let out = run_bash_core(
        generation,
        auth_manager,
        cwd,
        &command,
        session_env.as_ref(),
        timeout,
        abort,
        None,
        Some(on_update),
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

    // Start-boundary failures run nothing and never reach the output
    // formatting below: the command never executed, so a pre-start abort is
    // distinguishable from an abort mid-run.
    let execution = match out.end {
        BashRunEnd::Started(execution) => execution,
        BashRunEnd::AbortedBeforeStart => {
            return Err(anyhow!("Command aborted before start"));
        }
    };

    // Abort wins over timeout when both fired.
    if execution.cancelled {
        let (text, _) = format_output("", false);
        cleanup_full_output_path(&full_output_path);
        return Err(anyhow!("{}", append_status(&text, "Command aborted")));
    }
    if execution.timed_out {
        let (text, _) = format_output("", false);
        cleanup_full_output_path(&full_output_path);
        // The message reports the measured execution window (bounded to two
        // decimals) next to the configured limit, so queued wait time is
        // never mistaken for run time.
        return Err(anyhow!(
            "{}",
            append_status(
                &text,
                &format!(
                    "Command execution timed out after {} seconds (limit {} seconds)",
                    format_elapsed_seconds(execution.execution_elapsed.unwrap_or_default()),
                    format_float(timeout.unwrap_or(0.0))
                )
            )
        ));
    }
    // A signal-killed child has no exit code (pi: exitCode === null) and is
    // treated as success with whatever output was produced.
    if let Some(code) = execution.exit_code {
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
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    auth_manager: Option<Arc<crate::AuthManager>>,
    cwd: &Path,
    command: &str,
    session_env: Option<SessionEnvFn>,
    on_chunk: Arc<dyn Fn(String) + Send + Sync>,
    abort: AbortSignal,
) -> Result<BashResult> {
    let cwd_str = cwd.to_string_lossy().into_owned();
    let raw: Arc<dyn Fn(&[u8]) + Send + Sync> =
        Arc::new(move |b: &[u8]| on_chunk(String::from_utf8_lossy(b).into_owned()));
    let out = run_bash_core(
        generation,
        auth_manager,
        &cwd_str,
        command,
        session_env.as_ref(),
        None,
        abort,
        Some(raw),
        None,
        false,
        None,
    )
    .await?;
    let (exit_code, cancelled) = match out.end {
        BashRunEnd::Started(execution) => (execution.exit_code, execution.cancelled),
        // The only pre-start exit is the caller's abort while still queued
        // behind the brush lock.
        BashRunEnd::AbortedBeforeStart => (None, true),
    };
    Ok(BashResult {
        output: out.content,
        exit_code,
        cancelled,
        truncated: out.truncation.truncated,
        full_output_path: if out.full_output_path.is_empty() {
            None
        } else {
            Some(out.full_output_path)
        },
    })
}

pub use bash::{cleanup_all_bash_spills, cleanup_full_output_path};

/// Formats an f64 the way JS `String(number)` does for the common bash timeout
/// values (shortest round-trippable decimal, no trailing `.0` for integers).
fn format_float(t: f64) -> String {
    if t.fract() == 0.0 {
        format!("{t:.0}")
    } else {
        format!("{t}")
    }
}

/// Renders a measured execution duration as seconds bounded to two decimals.
/// The execution-timeout message reports this measured value — never the raw
/// configured timeout — so queueing time is never reported as run time.
fn format_elapsed_seconds(elapsed: Duration) -> String {
    format_float((elapsed.as_secs_f64() * 100.0).round() / 100.0)
}

/// Renders a collected list (ls entries, find/glob results, grep rows) with
/// pi's truncation/limit notice contract (ls.ts/find.ts/grep.ts):
///
/// - the joined output is head-truncated at [`DEFAULT_MAX_BYTES`] with no line
///   limit (the collection limit already capped rows), so a hit appends a
///   `<size> limit reached` notice and a `truncation` detail;
/// - when `counted` reached the effective collection limit, a
///   `<limit> <kind> limit reached<limit_suffix>` notice and a
///   `<kind>LimitReached` detail are emitted (the details key is passed in
///   because pi names it per tool: `entryLimitReached` / `resultLimitReached`
///   / `matchLimitReached`);
/// - extra notices (e.g. grep's line-truncation warning) and details join
///   before the limit/byte notices.
fn bounded_output_result(
    rows: &[String],
    counted: usize,
    effective_limit: usize,
    kind: &str,
    limit_suffix: &str,
    limit_details_key: &str,
    extra_notices: &[String],
    extra_details: &[(&str, Value)],
) -> Result<AgentToolResult> {
    let joined = rows.join("\n");
    let truncation = truncate_head(&joined, usize::MAX, DEFAULT_MAX_BYTES);
    // Keep `truncation` whole for the details payload; clone the content so
    // serializing `&truncation` below does not observe a partially moved field.
    let mut output = truncation.content.clone();
    let mut notices = extra_notices.to_vec();
    let mut details = serde_json::Map::new();
    if counted >= effective_limit {
        notices.push(format!(
            "{effective_limit} {kind} limit reached{limit_suffix}"
        ));
        details.insert(limit_details_key.to_owned(), json!(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".to_owned(), serde_json::to_value(&truncation)?);
    }
    for (key, value) in extra_details {
        details.insert((*key).to_owned(), value.clone());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let mut result = text_result(output);
    if !details.is_empty() {
        result.details = Value::Object(details);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

fn ls_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            (
                "path",
                s_string("Directory to list (default: current directory)"),
            ),
            (
                "limit",
                s_number("Maximum number of entries to return (default: 5000)"),
            ),
        ],
        vec![],
    );
    let description = format!(
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {} entries or {}KB (whichever is hit first).",
        LS_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("ls", description, params, move |_ctx| async move {
        Err(anyhow!("ls requires an application generation"))
    })
    .with_capability(ToolCapability::Read)
}

fn run_ls(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("ls requires an application generation"))
}
fn ls_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("path", s_string("Policy-authorized directory to list")),
            ("limit", s_number("Maximum number of entries")),
        ],
        vec![],
    );
    AgentTool::new(
        "ls",
        "List a directory through retained policy filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move {
                let generation = capture()?;
                run_ls_captured(&workspace, &generation, ctx.arguments, ctx.abort)
            }
        },
    )
    .with_capability(ToolCapability::Read)
}

fn run_ls_captured(
    workspace: &crate::WorkspaceRoots,
    generation: &Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let directory = match args
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(value) => PathBuf::from(resolve_scoped_path(value, workspace)?),
        None => workspace.cwd().to_path_buf(),
    };
    let limit = arg_int(&args, "limit")?
        .map(|value| value.max(0) as usize)
        .unwrap_or(LS_DEFAULT_LIMIT);
    let fs = captured_policy_fs(generation)?;
    let mut entries = fs.read_dir(&directory)?;
    // Collect and sort ALL entries first, then apply the entry limit while
    // formatting (pi: sort with toLowerCase().localeCompare, then cap) so a
    // limited listing still returns the alphabetically-first entries.
    let mut names = Vec::new();
    while let Some(entry) = entries.next()? {
        check_aborted(&abort)?;
        names.push((
            entry.name().to_string_lossy().into_owned(),
            entry.opened().kind(),
        ));
    }
    names.sort_by(|left, right| left.0.to_lowercase().cmp(&right.0.to_lowercase()));
    let mut results = Vec::new();
    let mut entry_limit_reached = false;
    for (name, kind) in &names {
        check_aborted(&abort)?;
        if results.len() >= limit {
            entry_limit_reached = true;
            break;
        }
        let suffix = if *kind == crate::fs::EntryKind::Directory {
            "/"
        } else {
            ""
        };
        results.push(format!("{name}{suffix}"));
    }
    if results.is_empty() {
        if entry_limit_reached {
            // pi: limit=0 (or a limit below the first entry) still reports the
            // limit alongside the empty listing instead of plain emptiness.
            let mut result = text_result(format!(
                "(empty directory)\n\n[{limit} entries limit reached. Use limit={} for more]",
                limit.saturating_mul(2)
            ));
            result.details = json!({ "entryLimitReached": limit });
            return Ok(result);
        }
        return Ok(text_result("(empty directory)"));
    }
    bounded_output_result(
        &results,
        results.len(),
        limit,
        "entries",
        &format!(". Use limit={} for more", limit * 2),
        "entryLimitReached",
        &[],
        &[],
    )
}

// ---------------------------------------------------------------------------
// find (glob)
// ---------------------------------------------------------------------------

fn find_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            (
                "pattern",
                s_string(
                    "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'",
                ),
            ),
            (
                "path",
                s_string("Directory to search in (default: current directory)"),
            ),
            (
                "limit",
                s_number("Maximum number of results (default: 5000)"),
            ),
        ],
        vec!["pattern"],
    );
    let description = format!(
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {} results or {}KB (whichever is hit first).",
        FIND_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("find", description, params, move |_ctx| async move {
        Err(anyhow!("find requires an application generation"))
    })
    .with_capability(ToolCapability::Read)
}

fn run_find(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("find requires an application generation"))
}

fn find_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("pattern", s_string("Glob pattern to match files")),
            ("path", s_string("Directory to search")),
            ("limit", s_number("Maximum results")),
        ],
        vec!["pattern"],
    );
    AgentTool::new(
        "find",
        "Search files through retained policy filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move {
                let generation = capture()?;
                let mut args = ctx.arguments;
                if let Some(object) = args.as_object_mut() {
                    object.insert("hidden".into(), Value::Bool(true));
                    // find's documented default is FIND_DEFAULT_LIMIT results; inject
                    // it so the shared walker's cap and the limit notice agree.
                    if !object.contains_key("limit") {
                        object.insert("limit".into(), Value::from(FIND_DEFAULT_LIMIT as i64));
                    }
                }
                run_glob_captured(&workspace, generation, args, ctx.abort).await
            }
        },
    )
    .with_capability(ToolCapability::Read)
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
pub(crate) fn create_glob_tool_for_workspace(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    glob_tool_with_generation(workspace, capture)
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
                    "Workspace-authorized directory, file, or semicolon-separated targets to search. Relative paths resolve from cwd (default: workspace cwd).",
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
        "Match files and directories by glob pattern. Hidden entries are skipped unless hidden=true. Respects .gitignore unless gitignore=false. Output is truncated to at most {GLOB_MAX_LIMIT} results or {}KB.",
        DEFAULT_MAX_BYTES / 1024
    );
    AgentTool::new("glob", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_glob(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

fn run_glob(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("glob requires an application generation"))
}
/// Relative path of `abs` under `base` when possible; otherwise the absolute path.
fn pathdiff_rel(base: &Path, abs: &str) -> String {
    let abs_path = Path::new(abs);
    abs_path
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.replace('\\', "/"))
}

fn glob_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("pattern", s_string("Glob pattern to match")),
            (
                "path",
                s_string(
                    "Workspace-authorized directory, file, or semicolon-separated targets. Relative paths resolve from cwd.",
                ),
            ),
            ("hidden", s_boolean("Include hidden entries")),
            (
                "gitignore",
                s_boolean("Respect .gitignore rules (default: true)"),
            ),
            ("limit", s_number("Maximum results")),
        ],
        vec!["pattern"],
    );
    AgentTool::new(
        "glob",
        "Match files and directories through retained policy filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move {
                let generation = capture()?;
                let mut args = ctx.arguments;
                // glob's contract is a hard `[1, GLOB_MAX_LIMIT]` bound (schema:
                // default GLOB_DEFAULT_LIMIT, max GLOB_MAX_LIMIT); normalize it here
                // so the shared walker and the limit notice see the effective cap.
                let limit = arg_int(&args, "limit")?
                    .map(|value| (value.max(1) as usize).min(GLOB_MAX_LIMIT))
                    .unwrap_or(GLOB_DEFAULT_LIMIT);
                if let Some(object) = args.as_object_mut() {
                    object.insert("limit".into(), Value::from(limit as i64));
                }
                run_glob_captured(&workspace, generation, args, ctx.abort).await
            }
        },
    )
    .with_capability(ToolCapability::Read)
}

async fn run_glob_captured(
    workspace: &crate::WorkspaceRoots,
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    if pattern.is_empty() {
        return Err(anyhow!("pattern is required"));
    }
    let include_hidden = arg_bool(&args, "hidden");
    // gitignore defaults to true (fd semantics); gitignore=false disables
    // .gitignore/.git/info/exclude/global-excludes matching so gitignored
    // files become visible (the engine still always skips `.git` itself).
    let use_gitignore = args
        .get("gitignore")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let limit = arg_int(&args, "limit")?
        .map(|value| value.max(1) as usize)
        .unwrap_or(GLOB_DEFAULT_LIMIT);
    let target = match args
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(value) => PathBuf::from(resolve_scoped_path(value, workspace)?),
        None => workspace.cwd().to_path_buf(),
    };
    let fs = captured_policy_fs(&generation)?;
    let metadata = fs.metadata(&target)?;
    let mut ignores = if use_gitignore {
        IgnoreStack::new(generation, &target.to_string_lossy(), false, true).await?
    } else {
        IgnoreStack::without_gitignore(&target.to_string_lossy())
    };
    let mut results = Vec::new();
    if metadata.identity.kind() == crate::fs::EntryKind::RegularFile {
        let name = target
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if (include_hidden || !name.starts_with('.'))
            && match_fd_glob(&pattern, name, &target.to_string_lossy())
        {
            results.push(pathdiff_rel(workspace.cwd(), &target.to_string_lossy()));
        }
    } else {
        walk_policy_fs(
            &fs,
            &mut ignores,
            &target,
            Path::new(""),
            &pattern,
            include_hidden,
            limit,
            &abort,
            &mut results,
        )?;
    }
    results.sort();
    // A mid-walk abort yields partial results from the walkers; surface it as
    // an abort (pi stages `signal.aborted` checks around the walk).
    check_aborted(&abort)?;
    if results.is_empty() {
        Ok(text_result("No files found matching pattern"))
    } else {
        bounded_output_result(
            &results,
            results.len(),
            limit,
            "results",
            &format!(". Use limit={} for more, or refine pattern", limit * 2),
            "resultLimitReached",
            &[],
            &[],
        )
    }
}

fn walk_policy_fs(
    fs: &Arc<crate::fs::PolicyFs>,
    ignores: &mut IgnoreStack,
    root: &Path,
    relative: &Path,
    pattern: &str,
    include_hidden: bool,
    limit: usize,
    abort: &AbortSignal,
    results: &mut Vec<String>,
) -> Result<()> {
    if results.len() >= limit || abort.is_aborted() {
        return Ok(());
    }
    let directory = root.join(relative);
    let mut entries = fs.read_dir(&directory)?;
    while let Some(entry) = entries.next()? {
        if results.len() >= limit || abort.is_aborted() {
            break;
        }
        let name = entry.name().to_string_lossy();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let child_relative = relative.join(entry.name());
        if ignores.ignored(
            &entry.display_path().to_string_lossy(),
            &child_relative.to_string_lossy(),
            matches!(entry.opened(), crate::fs::PolicyOpenedEntry::Directory(_)),
        ) {
            continue;
        }
        let rel = child_relative.to_string_lossy().replace('\\', "/");
        if match_fd_glob(pattern, &rel, &entry.display_path().to_string_lossy()) {
            results.push(rel.clone());
        }
        if let crate::fs::PolicyOpenedEntry::Directory(directory) = entry.into_opened() {
            walk_policy_directory(
                fs,
                ignores,
                directory,
                root,
                &child_relative,
                pattern,
                include_hidden,
                limit,
                abort,
                results,
            )?;
        }
    }
    Ok(())
}

fn walk_policy_directory(
    fs: &Arc<crate::fs::PolicyFs>,
    ignores: &mut IgnoreStack,
    directory: crate::fs::PolicyDirectory,
    root: &Path,
    relative: &Path,
    pattern: &str,
    include_hidden: bool,
    limit: usize,
    abort: &AbortSignal,
    results: &mut Vec<String>,
) -> Result<()> {
    let mut entries = directory.read_dir(fs)?;
    while let Some(entry) = entries.next()? {
        if results.len() >= limit || abort.is_aborted() {
            break;
        }
        let name = entry.name().to_string_lossy();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let child_relative = relative.join(entry.name());
        if ignores.ignored(
            &entry.display_path().to_string_lossy(),
            &child_relative.to_string_lossy(),
            matches!(entry.opened(), crate::fs::PolicyOpenedEntry::Directory(_)),
        ) {
            continue;
        }
        let rel = child_relative.to_string_lossy().replace('\\', "/");
        if match_fd_glob(pattern, &rel, &entry.display_path().to_string_lossy()) {
            results.push(rel);
        }
        if let crate::fs::PolicyOpenedEntry::Directory(child) = entry.into_opened() {
            walk_policy_directory(
                fs,
                ignores,
                child,
                root,
                &child_relative,
                pattern,
                include_hidden,
                limit,
                abort,
                results,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

fn grep_tool_parameters() -> Schema {
    s_strict_object(
        vec![
            (
                "pattern",
                s_string("Search pattern (regex or literal string)"),
            ),
            (
                "path",
                s_string(
                    "Workspace-authorized directory or file to search. Relative paths resolve from cwd (default: current directory).",
                ),
            ),
            (
                "glob",
                s_string("Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"),
            ),
            (
                "ignoreCase",
                s_boolean("Case-insensitive search (default: false)"),
            ),
            (
                "literal",
                s_boolean("Treat pattern as literal string instead of regex (default: false)"),
            ),
            (
                "context",
                s_number("Number of lines before and after each match (default: 0)"),
            ),
            (
                "limit",
                s_number("Maximum number of matches to return (default: 1000)"),
            ),
        ],
        vec!["pattern"],
    )
}

fn grep_tool(_cwd: &str) -> AgentTool {
    let description = format!(
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {} matches or {}KB (whichever is hit first). Long lines are truncated to {} chars.",
        GREP_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024,
        GREP_MAX_LINE_LENGTH
    );
    AgentTool::new(
        "grep",
        description,
        grep_tool_parameters(),
        move |_ctx| async move { Err(anyhow!("grep requires an application generation")) },
    )
    .with_capability(ToolCapability::Read)
}

fn run_grep(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("grep requires an application generation"))
}

fn grep_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    AgentTool::new(
        "grep",
        "Search file contents through retained policy filesystem authority. Paths require workspace authorization; relative paths resolve from cwd.",
        grep_tool_parameters(),
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move { run_grep_captured(&workspace, capture()?, ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Read)
}

async fn run_grep_captured(
    workspace: &crate::WorkspaceRoots,
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    let flags = if arg_bool(&args, "ignoreCase") {
        "(?i)"
    } else {
        ""
    };
    let expression = if arg_bool(&args, "literal") {
        regex::escape(&pattern)
    } else {
        pattern
    };
    let regex = regex::Regex::new(&format!("{flags}{expression}"))
        .map_err(|error| anyhow!("invalid regex: {error}"))?;
    let root = match args
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(value) => PathBuf::from(resolve_scoped_path(value, workspace)?),
        None => workspace.cwd().to_path_buf(),
    };
    let fs = captured_policy_fs(&generation)?;
    // pi: Math.max(1, limit ?? 100) — non-positive limits clamp to 1.
    let limit = arg_int(&args, "limit")?
        .map(|value| value.max(1) as usize)
        .unwrap_or(GREP_DEFAULT_LIMIT);
    let context = arg_int(&args, "context")?
        .map(|value| value.max(0) as usize)
        .unwrap_or(0);
    let glob = arg_str(&args, "glob");
    let mut ignores = IgnoreStack::new(generation, &root.to_string_lossy(), true, false).await?;
    let mut output = GrepOutput::default();
    // `limit` counts MATCHES only; context rows never consume the budget
    // (pi tracks matchCount separately from the output rows).
    let mut matched = 0usize;
    let mut lines_truncated = false;
    let mut output_truncated = false;
    grep_policy_entry(
        &fs,
        &mut ignores,
        &root,
        Path::new(""),
        &regex,
        &glob,
        context,
        limit,
        &abort,
        &mut output,
        &mut matched,
        &mut lines_truncated,
        &mut output_truncated,
    )?;
    // A mid-scan abort yields partial rows from the walker; surface it as an
    // abort instead of a truncated-but-successful result.
    check_aborted(&abort)?;
    if output.is_empty() {
        return Ok(text_result("No matches found"));
    }
    let mut extra_notices = Vec::new();
    let mut extra_details = Vec::new();
    if lines_truncated {
        extra_notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        extra_details.push(("linesTruncated", json!(true)));
    }
    if output_truncated {
        extra_notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        extra_details.push((
            "truncation",
            json!(TruncationResult {
                truncated: true,
                truncated_by: "bytes".to_owned(),
                total_lines: matched,
                total_bytes: DEFAULT_MAX_BYTES.saturating_add(1),
                output_lines: output.len(),
                output_bytes: output.retained_bytes,
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
                ..TruncationResult::default()
            }),
        ));
        extra_details.push(("outputTruncated", json!(true)));
    }
    bounded_output_result(
        &output,
        matched,
        limit,
        "matches",
        &format!(". Use limit={} for more, or refine pattern", limit * 2),
        "matchLimitReached",
        &extra_notices,
        &extra_details,
    )
}

fn grep_policy_entry(
    fs: &Arc<crate::fs::PolicyFs>,
    ignores: &mut IgnoreStack,
    root: &Path,
    relative: &Path,
    regex: &regex::Regex,
    glob: &str,
    context: usize,
    limit: usize,
    abort: &AbortSignal,
    output: &mut GrepOutput,
    matched: &mut usize,
    lines_truncated: &mut bool,
    output_truncated: &mut bool,
) -> Result<()> {
    if *matched >= limit || abort.is_aborted() {
        return Ok(());
    }
    let target = root.join(relative);
    let metadata = fs.metadata(&target)?;
    if metadata.identity.kind() == crate::fs::EntryKind::RegularFile {
        let rel = if relative.as_os_str().is_empty() {
            target
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            relative.to_string_lossy().replace('\\', "/")
        };
        if !glob.is_empty() && !match_rg_glob(glob, &rel) {
            return Ok(());
        }
        grep_policy_file(
            fs,
            &target,
            &rel,
            regex,
            context,
            limit,
            abort,
            output,
            matched,
            lines_truncated,
            output_truncated,
        )?;
        return Ok(());
    }
    let directory = fs.open_directory(&target)?;
    let mut entries = directory.read_dir(fs)?;
    while let Some(entry) = entries.next()? {
        let child = relative.join(entry.name());
        if ignores.ignored(
            &entry.display_path().to_string_lossy(),
            &child.to_string_lossy(),
            matches!(entry.opened(), crate::fs::PolicyOpenedEntry::Directory(_)),
        ) {
            continue;
        }
        grep_policy_entry(
            fs,
            ignores,
            root,
            &child,
            regex,
            glob,
            context,
            limit,
            abort,
            output,
            matched,
            lines_truncated,
            output_truncated,
        )?;
        if *matched >= limit {
            break;
        }
    }
    Ok(())
}

fn grep_policy_file(
    fs: &Arc<crate::fs::PolicyFs>,
    target: &Path,
    rel: &str,
    regex: &regex::Regex,
    context: usize,
    limit: usize,
    abort: &AbortSignal,
    output: &mut GrepOutput,
    matched: &mut usize,
    lines_truncated: &mut bool,
    output_truncated: &mut bool,
) -> Result<()> {
    let opened = fs.open_file(target)?;
    let mut file = opened.try_clone_std()?;
    let mut binary_probe = [0u8; 8192];
    let probe_len = file.read(&mut binary_probe)?;
    if binary_probe[..probe_len].contains(&0) {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    scan_grep_lines(
        &mut reader,
        rel,
        regex,
        context,
        limit,
        abort,
        output,
        matched,
        lines_truncated,
        output_truncated,
    )
}

fn scan_grep_lines<R: BufRead + ?Sized>(
    reader: &mut R,
    rel: &str,
    regex: &regex::Regex,
    context: usize,
    limit: usize,
    abort: &AbortSignal,
    output: &mut GrepOutput,
    matched: &mut usize,
    lines_truncated: &mut bool,
    output_truncated: &mut bool,
) -> Result<()> {
    let mut raw = Vec::new();
    let mut preceding = VecDeque::<GrepBufferedRow>::new();
    let mut preceding_bytes = 0usize;
    let mut pending = VecDeque::<PendingGrepBlock>::new();
    let mut pending_bytes = 0usize;
    let mut line_number = 0usize;
    loop {
        if abort.is_aborted() {
            return Ok(());
        }
        let line = match read_grep_line(reader, &mut raw, abort)? {
            GrepLine::Eof => {
                for block in &mut pending {
                    block.remaining = 0;
                }
                flush_grep_blocks(
                    &mut pending,
                    &mut pending_bytes,
                    output,
                    rel,
                    lines_truncated,
                    output_truncated,
                );
                return Ok(());
            }
            GrepLine::Skipped => None,
            GrepLine::Text(line) => Some(line),
        };
        line_number += 1;
        let buffered = line
            .as_deref()
            .map(|text| GrepBufferedRow::new(rel, line_number, text));

        for block in &mut pending {
            if block.remaining == 0 {
                continue;
            }
            block.remaining -= 1;
            let Some(row) = buffered.as_ref() else {
                continue;
            };
            if pending_bytes.saturating_add(row.retained_bytes) <= DEFAULT_MAX_BYTES {
                block.rows.push(row.clone());
                pending_bytes += row.retained_bytes;
            } else {
                *output_truncated = true;
            }
        }

        if let Some(line) = line.as_deref()
            && *matched < limit
            && regex.is_match(line)
        {
            let match_row = buffered.clone().expect("text line has buffered row");
            let mut rows = Vec::new();
            let mut block_bytes = match_row.retained_bytes;
            if pending_bytes.saturating_add(block_bytes) <= DEFAULT_MAX_BYTES {
                for row in preceding.iter().rev() {
                    if row.line_number.saturating_add(context) < line_number {
                        break;
                    }
                    if pending_bytes
                        .saturating_add(block_bytes)
                        .saturating_add(row.retained_bytes)
                        > DEFAULT_MAX_BYTES
                    {
                        *output_truncated = true;
                        break;
                    }
                    rows.push(row.clone());
                    block_bytes += row.retained_bytes;
                }
                rows.reverse();
                rows.push(match_row);
                pending_bytes += block_bytes;
            } else {
                *output_truncated = true;
            }
            pending.push_back(PendingGrepBlock {
                rows,
                match_line: line_number,
                remaining: context,
            });
            *matched += 1;
        }

        flush_grep_blocks(
            &mut pending,
            &mut pending_bytes,
            output,
            rel,
            lines_truncated,
            output_truncated,
        );

        let min_context_line = line_number.saturating_add(1).saturating_sub(context);
        while preceding
            .front()
            .is_some_and(|row| row.line_number < min_context_line)
        {
            let removed = preceding
                .pop_front()
                .expect("preceding context is non-empty");
            preceding_bytes = preceding_bytes.saturating_sub(removed.retained_bytes);
        }
        if let Some(row) = buffered {
            preceding_bytes += row.retained_bytes;
            preceding.push_back(row);
            while preceding_bytes > DEFAULT_MAX_BYTES {
                let removed = preceding
                    .pop_front()
                    .expect("preceding context is non-empty");
                preceding_bytes = preceding_bytes.saturating_sub(removed.retained_bytes);
                *output_truncated = true;
            }
        }

        if *matched >= limit && pending.is_empty() {
            return Ok(());
        }
    }
}

#[derive(Clone)]
struct GrepBufferedRow {
    line_number: usize,
    text: String,
    was_truncated: bool,
    retained_bytes: usize,
}

impl GrepBufferedRow {
    fn new(rel: &str, line_number: usize, text: &str) -> Self {
        let (text, was_truncated) = truncate_line(text, 0);
        let retained_bytes = rel
            .len()
            .saturating_add(line_number.to_string().len())
            .saturating_add(text.len())
            .saturating_add(4);
        Self {
            line_number,
            text,
            was_truncated,
            retained_bytes,
        }
    }
}

struct PendingGrepBlock {
    rows: Vec<GrepBufferedRow>,
    match_line: usize,
    remaining: usize,
}

impl PendingGrepBlock {
    fn retained_bytes(&self) -> usize {
        self.rows.iter().map(|row| row.retained_bytes).sum()
    }
}

fn flush_grep_blocks(
    pending: &mut VecDeque<PendingGrepBlock>,
    pending_bytes: &mut usize,
    output: &mut GrepOutput,
    rel: &str,
    lines_truncated: &mut bool,
    output_truncated: &mut bool,
) {
    while pending.front().is_some_and(|block| block.remaining == 0) {
        let block = pending.pop_front().expect("completed pending block exists");
        *pending_bytes = pending_bytes.saturating_sub(block.retained_bytes());
        for row in block.rows {
            if row.was_truncated {
                *lines_truncated = true;
            }
            let rendered = if row.line_number == block.match_line {
                format!("{rel}:{}: {}", row.line_number, row.text)
            } else {
                format!("{rel}-{}- {}", row.line_number, row.text)
            };
            if !output.push(rendered) {
                *output_truncated = true;
            }
        }
    }
}

#[derive(Debug, Default)]
struct GrepOutput {
    rows: Vec<String>,
    retained_bytes: usize,
}

impl GrepOutput {
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    fn push(&mut self, row: String) -> bool {
        let separator = usize::from(!self.rows.is_empty());
        if self
            .retained_bytes
            .saturating_add(separator)
            .saturating_add(row.len())
            > DEFAULT_MAX_BYTES
        {
            return false;
        }
        self.retained_bytes += separator + row.len();
        self.rows.push(row);
        true
    }
}

impl std::ops::Deref for GrepOutput {
    type Target = [String];

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

/// Per-line bound mirrors the old whole-file read budget: a line may hold
/// up to 64 MiB before it is drained and skipped (abort-aware).
const GREP_MAX_RAW_LINE: usize = 64 * 1024 * 1024;

enum GrepLine {
    Eof,
    Skipped,
    Text(String),
}

fn read_grep_line<R: BufRead + ?Sized>(
    reader: &mut R,
    raw: &mut Vec<u8>,
    abort: &AbortSignal,
) -> Result<GrepLine> {
    raw.clear();
    let mut total = 0usize;
    let mut pending_cr = false;
    loop {
        if abort.is_aborted() {
            return Ok(GrepLine::Skipped);
        }
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if total == 0 {
                return Ok(GrepLine::Eof);
            }
            break;
        }
        if pending_cr {
            if buffer[0] == b'\n' {
                raw.push(b'\n');
                reader.consume(1);
            }
            break;
        }
        let newline_at = buffer.iter().position(|byte| *byte == b'\n');
        let cr_at = buffer.iter().position(|byte| *byte == b'\r');
        let term_at = match (cr_at, newline_at) {
            (Some(cr), Some(nl)) => Some(cr.min(nl)),
            (Some(cr), None) => Some(cr),
            (None, Some(nl)) => Some(nl),
            (None, None) => None,
        };
        let clean_term = term_at;
        if let Some(term) = clean_term {
            let is_cr = buffer[term] == b'\r';
            let is_crlf = is_cr && term + 1 < buffer.len() && buffer[term + 1] == b'\n';
            let mut take = term + 1;
            if is_crlf {
                take += 1;
            }
            if total + take > GREP_MAX_RAW_LINE {
                reader.consume(take);
                return Ok(GrepLine::Skipped);
            }
            raw.extend_from_slice(&buffer[..take]);
            let ends_at_buffer_edge = is_cr && !is_crlf && term + 1 == buffer.len();
            reader.consume(take);
            if ends_at_buffer_edge {
                pending_cr = true;
                continue;
            }
            break;
        }
        if total + buffer.len() > GREP_MAX_RAW_LINE {
            let keep = GREP_MAX_RAW_LINE - total;
            raw.extend_from_slice(&buffer[..keep]);
            reader.consume(keep);
            drain_grep_line(reader, abort)?;
            return Ok(GrepLine::Skipped);
        }
        let take = buffer.len();
        raw.extend_from_slice(buffer);
        reader.consume(take);
        total += take;
    }
    if raw.ends_with(b"\n") {
        raw.pop();
    }
    if raw.ends_with(b"\r") {
        raw.pop();
    }
    Ok(GrepLine::Text(String::from_utf8_lossy(raw).into_owned()))
}

/// Consume the remainder of an over-long line through its CR/LF/CRLF
/// terminator. Abort-aware: the scan loop's post-drain check surfaces
/// `Operation aborted` instead of blocking until newline/EOF.
fn drain_grep_line<R: BufRead + ?Sized>(reader: &mut R, abort: &AbortSignal) -> Result<()> {
    let mut pending_cr = false;
    loop {
        if abort.is_aborted() {
            return Ok(());
        }
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        if pending_cr {
            if buffer[0] == b'\n' {
                reader.consume(1);
            }
            return Ok(());
        }
        let term = buffer
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r');
        match term {
            Some(at) => {
                let mut take = at + 1;
                if buffer[at] == b'\r' && at + 1 == buffer.len() {
                    reader.consume(take);
                    pending_cr = true;
                    continue;
                }
                if buffer[at] == b'\r' && buffer[at + 1] == b'\n' {
                    take += 1;
                }
                reader.consume(take);
                return Ok(());
            }
            None => {
                let skip = buffer.len();
                reader.consume(skip);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recursive directory walker (gitignore-aware)
// ---------------------------------------------------------------------------

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

    struct ToolGenerationFixture {
        _agent_dir: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
        _faux: pi_ai::providers::FauxProviderRegistration,
        application: crate::Application,
        workspace: crate::WorkspaceRoots,
        skills: Option<SkillSnapshotFn>,
        resolver: Option<InternalUriResolverFn>,
    }

    impl ToolGenerationFixture {
        fn generation(&self) -> Arc<crate::application::generation::ApplicationGeneration> {
            self.application.capture_generation()
        }

        fn capture(&self) -> GenerationCaptureFn {
            self.application.generation_capture_fn()
        }

        async fn hold_brush_exec_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
            bash::brush::test_hold_bash_exec_lock(self.generation().brush_exec_lock()).await
        }

        fn abort_aware(&self, mut tool: AgentTool) -> AgentTool {
            let execute = tool.execute.clone();
            tool.execute = Arc::new(move |context| {
                if context.abort.is_aborted() {
                    return Box::pin(async { Err(anyhow!("Operation aborted")) });
                }
                execute(context)
            });
            tool
        }

        fn tool(&self, name: &str) -> AgentTool {
            let capture = self.capture();
            let cwd = self.workspace.cwd().to_string_lossy();
            let tool = match name {
                "read" => read_tool_for_workspace_captured(
                    self.workspace.clone(),
                    self.skills.clone(),
                    self.resolver.clone(),
                    capture,
                    None,
                ),
                "inspect_image" => {
                    inspect_image_tool_with_generation(self.workspace.clone(), capture)
                }
                "bash" => bash_tool_with_generation(&cwd, None, None, capture),
                "edit" => edit_tool_for_workspace_captured(self.workspace.clone(), capture),
                "write" => write_tool_for_workspace_captured(self.workspace.clone(), capture),
                "grep" => grep_tool_with_generation(self.workspace.clone(), capture),
                "find" => find_tool_with_generation(self.workspace.clone(), capture),
                "glob" => glob_tool_with_generation(self.workspace.clone(), capture),
                "ls" => ls_tool_with_generation(self.workspace.clone(), capture),
                "browser" => browser_tool_for_workspace(self.workspace.clone(), capture),
                "web_search" => web_search::web_search_tool_with_generation(capture),
                "ast_grep" => ast_grep_tool_with_generation(self.workspace.clone(), capture),
                "ast_edit" => ast_edit_tool_with_generation(self.workspace.clone(), capture),
                "ask" => ask::standalone_ask_tool(),
                "lsp" => lsp_tool_with_generation(&cwd, crate::empty_permission_rules(), capture),
                "github" => github::github_tool_with_generation(capture),
                "debug" => debug_tool_with_generation(&cwd, capture),
                "eval" => eval_tool_with_generation(&cwd, capture),
                "notebook" => notebook_tool_with_generation(&cwd, capture),
                "mcp" => mcp_tool(McpRegistry::new(capture, None)),
                "generate_image" => generate_image_tool_for_workspace_with_generation(
                    self.workspace.clone(),
                    None,
                    capture,
                ),
                "todo" => todo_tool(TodoRuntime::memory()),
                "memory" => memory_tools_for_with_capabilities_and_generation(
                    self.generation().sandbox().capabilities(),
                    &cwd,
                    None,
                    Some(MemoryConfig::default()),
                    capture,
                )
                .into_iter()
                .next()
                .expect("generation-bound memory tool"),
                "recall" | "retain" | "reflect" => {
                    let config = MemoryConfig {
                        backend: crate::MemoryBackend::Hindsight,
                        hindsight_api_url: Some("https://memory.example.test".to_owned()),
                        ..Default::default()
                    };
                    memory_tools_for_with_capabilities_and_generation(
                        self.generation().sandbox().capabilities(),
                        &cwd,
                        None,
                        Some(config),
                        capture,
                    )
                    .into_iter()
                    .find(|tool| tool.name == name)
                    .unwrap_or_else(|| panic!("missing generation-bound hindsight tool: {name}"))
                }
                _ => panic!("unsupported generation-bound test tool: {name}"),
            };
            self.abort_aware(tool)
        }

        fn coding_tools(&self, memory: Option<MemoryConfigFn>) -> Vec<AgentTool> {
            create_coding_tools_for_workspace_with_generation(
                self.workspace.clone(),
                None,
                None,
                None,
                None,
                memory,
                None,
                self.capture(),
            )
            .expect("generation-bound coding tools")
        }
    }

    fn named_tool(tools: &[AgentTool], name: &str) -> AgentTool {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing generation-bound test tool: {name}"))
            .clone()
    }

    async fn tool_generation_fixture(cwd: &Path) -> ToolGenerationFixture {
        tool_generation_fixture_with_settings(
            crate::WorkspaceRoots::new(cwd, Vec::<PathBuf>::new()).expect("workspace"),
            None,
            None,
            true,
        )
        .await
    }

    async fn tool_generation_fixture_for_workspace(
        workspace: crate::WorkspaceRoots,
    ) -> ToolGenerationFixture {
        tool_generation_fixture_with_settings(workspace, None, None, true).await
    }
    async fn tool_generation_fixture_with_context(
        workspace: crate::WorkspaceRoots,
        skills: Option<SkillSnapshotFn>,
        resolver: Option<InternalUriResolverFn>,
    ) -> ToolGenerationFixture {
        tool_generation_fixture_with_settings(workspace, skills, resolver, true).await
    }

    async fn tool_generation_fixture_with_settings(
        workspace: crate::WorkspaceRoots,
        skills: Option<SkillSnapshotFn>,
        resolver: Option<InternalUriResolverFn>,
        danger_full_access: bool,
    ) -> ToolGenerationFixture {
        tool_generation_fixture_result(workspace, skills, resolver, danger_full_access)
            .await
            .expect("publish policy filesystem application")
    }

    /// Confined-mode fixture behind the trusted-launcher gate (mirror of
    /// `sandbox_smoke::confined_bootstrap`): a confined generation authenticates
    /// the bubblewrap launcher during bootstrap, so without a trusted launcher
    /// (bwrap >= 0.10) the build fails with a `launcher`-named error and the
    /// test must skip rather than silently degrade. Any other bootstrap failure
    /// is a real defect and panics.
    async fn confined_generation_fixture(
        workspace: crate::WorkspaceRoots,
    ) -> Option<ToolGenerationFixture> {
        match tool_generation_fixture_result(workspace, None, None, false).await {
            Ok(fixture) => Some(fixture),
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("launcher") {
                    eprintln!(
                        "skipping live confinement test: trusted sandbox launcher unavailable ({message})"
                    );
                    None
                } else {
                    panic!("confined fixture failed beyond an unavailable launcher: {message}");
                }
            }
        }
    }

    async fn tool_generation_fixture_result(
        workspace: crate::WorkspaceRoots,
        skills: Option<SkillSnapshotFn>,
        resolver: Option<InternalUriResolverFn>,
        danger_full_access: bool,
    ) -> anyhow::Result<ToolGenerationFixture> {
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let session_dir = tempfile::tempdir().expect("session dir");
        let sandbox_settings = if danger_full_access {
            json!({ "sandbox": { "mode": "danger-full-access" } })
        } else {
            json!({ "sandbox": { "mode": "workspace-write" } })
        };
        fs::write(
            agent_dir.path().join("settings.json"),
            serde_json::to_vec(&sandbox_settings).expect("serialize isolated sandbox settings"),
        )
        .expect("isolated sandbox settings");
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let api = format!("tools-test-api-{suffix}");
        let provider = format!("tools-test-provider-{suffix}");
        let model = pi_ai::Model {
            id: format!("tools-test-model-{suffix}"),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".into(),
            ..pi_ai::Model::default()
        };
        let faux =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![model.clone()],
                chunk_size: 8,
            });
        let mut resource_options = crate::ResourceManagerOptions::new(workspace.cwd());
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("isolated resources");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
                    model,
                    cwd: workspace.cwd().to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: String::new(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace.clone(),
                None,
            )
            .expect("session");
        session.set_session_dir(session_dir.path().to_path_buf());
        session
            .attach_resources(resources)
            .await
            .expect("attach isolated resources");
        let application = crate::Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session),
        )
        .await?;
        Ok(ToolGenerationFixture {
            _agent_dir: agent_dir,
            _faux: faux,
            _session_dir: session_dir,
            application,
            workspace,
            skills,
            resolver,
        })
    }

    async fn pdf_test_generation(
        cwd: &Path,
    ) -> (
        ToolGenerationFixture,
        Arc<crate::application::generation::ApplicationGeneration>,
    ) {
        let fixture = tool_generation_fixture(cwd).await;
        let generation = fixture.generation();
        (fixture, generation)
    }

    fn captured_read_tool(
        cwd: &Path,
        generation: Arc<crate::application::generation::ApplicationGeneration>,
    ) -> AgentTool {
        let capture: GenerationCaptureFn = Arc::new(move || Ok(generation.clone()));
        read_tool_for_workspace_captured(
            factory_workspace(&cwd.to_string_lossy()),
            None,
            None,
            capture,
            None,
        )
    }

    fn captured_bash_tool(
        cwd: &Path,
        generation: Arc<crate::application::generation::ApplicationGeneration>,
    ) -> AgentTool {
        let capture: GenerationCaptureFn = Arc::new(move || Ok(generation.clone()));
        bash_tool_with_generation(&cwd.to_string_lossy(), None, None, capture)
    }

    fn media_details(result: &AgentToolResult) -> &[Value] {
        result.details["media"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
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
            approval_token: None,
        }
    }

    fn text_of(res: &AgentToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    /// Polls subsecond-gated cross-command synchronization files (deterministic
    /// ordering between two shell commands without long sleeps).
    async fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for gate file {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_pid_alive(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(close) = stat.rfind(')') else {
            return false;
        };
        let state = stat[close + 1..].split_whitespace().next().unwrap_or("");
        state != "Z" && !state.is_empty()
    }

    #[cfg(target_os = "linux")]
    fn read_stat_start_time(stat: &str) -> Option<u64> {
        let close = stat.rfind(')')?;
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        fields.get(19).and_then(|field| field.parse().ok())
    }

    #[cfg(target_os = "linux")]
    fn linux_process_start_time(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        read_stat_start_time(&stat)
    }

    #[cfg(target_os = "linux")]
    fn linux_pid_identity_live(pid: i32, start_time: u64) -> bool {
        linux_pid_alive(pid) && linux_process_start_time(pid) == Some(start_time)
    }

    #[cfg(target_os = "linux")]
    fn read_recorded_pid(path: &Path) -> i32 {
        fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .trim()
            .parse()
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    #[cfg(target_os = "linux")]
    fn read_recorded_start_time(path: &Path) -> u64 {
        let stat = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        read_stat_start_time(&stat).unwrap_or_else(|| panic!("starttime in {}", path.display()))
    }

    #[cfg(target_os = "linux")]
    fn tracked_sleep_command(stem: &str) -> String {
        format!(
            "sh -c 'echo $$ > {stem}.pid; IFS= read -r stat < /proc/self/stat; printf \"%s\\n\" \"$stat\" > {stem}.start; printf started > {stem}.ready; exec sleep 30'"
        )
    }

    #[cfg(target_os = "linux")]
    async fn wait_until_identity_gone(pid: i32, start_time: u64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while linux_pid_identity_live(pid, start_time) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !linux_pid_identity_live(pid, start_time),
            "descendant {pid} start {start_time} must be gone"
        );
    }

    #[cfg(target_os = "linux")]
    struct TrackedSleepRun {
        controller: pi_agent::AbortController,
        run: tokio::task::JoinHandle<anyhow::Result<pi_agent::AgentToolResult>>,
        pid: i32,
        start_time: u64,
    }

    #[cfg(target_os = "linux")]
    async fn spawn_tracked_sleep(
        fixture: &ToolGenerationFixture,
        dir: &Path,
        stem: &str,
        timeout: f64,
    ) -> TrackedSleepRun {
        let (controller, abort) = pi_agent::AbortController::new();
        let tool = fixture.tool("bash");
        let command = tracked_sleep_command(stem);
        let run = tokio::spawn(async move {
            let mut context = make_ctx(json!({
                "command": command,
                "timeout": timeout
            }));
            context.abort = abort;
            (tool.execute)(context).await
        });
        wait_for_file(&dir.join(format!("{stem}.ready"))).await;
        let pid = read_recorded_pid(&dir.join(format!("{stem}.pid")));
        let start_time = read_recorded_start_time(&dir.join(format!("{stem}.start")));
        assert!(
            linux_pid_identity_live(pid, start_time),
            "{stem} recorded pid {pid} start {start_time} must match live /proc"
        );
        TrackedSleepRun {
            controller,
            run,
            pid,
            start_time,
        }
    }

    #[cfg(target_os = "linux")]
    async fn join_aborted_tracked_sleep(held: TrackedSleepRun) {
        let error = held
            .run
            .await
            .expect("tracked sleep task joins")
            .expect_err("tracked sleep must abort")
            .to_string();
        assert!(error.contains("Command aborted"), "{error}");
        wait_until_identity_gone(held.pid, held.start_time).await;
    }

    #[cfg(target_os = "linux")]
    async fn abort_tracked_sleep(held: TrackedSleepRun) {
        held.controller.abort();
        join_aborted_tracked_sleep(held).await;
    }

    #[test]
    fn bash_live_chunk_delivery_stops_at_byte_cap() {
        let first = bounded_bash_live_chunk(BASH_LIVE_STREAM_MAX_BYTES - 2, b"abcd");
        assert_eq!(first, b"ab");
        assert!(bounded_bash_live_chunk(BASH_LIVE_STREAM_MAX_BYTES, b"later").is_empty());
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
            input: if images {
                vec!["text".into(), "image".into()]
            } else {
                vec!["text".into()]
            },
            ..pi_ai::Model::default()
        }
    }

    fn tiny_png() -> Vec<u8> {
        // `::image` — the local `image` module (inspect_image) shadows the
        // extern crate name inside tools.rs.
        let image = ::image::DynamicImage::new_rgba8(2, 2);
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                ::image::ImageFormat::Png,
            )
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
        fs::write(
            d.join("hello.txt"),
            b"first line\nsecond line\nthird line\n",
        )
        .unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("read");
        let res = (tool.execute)(make_ctx(json!({ "path": "hello.txt" })))
            .await
            .unwrap();
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
            managed_project_boundary: None,
            managed_root: None,
            trusted: true,
        };
        let provider: SkillSnapshotFn = Arc::new(move || vec![skill.clone()]);
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(&d, Vec::<PathBuf>::new()).expect("workspace"),
            Some(provider),
            None,
        )
        .await;
        let tool = fixture.tool("read");
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

    fn trusted_test_skill(base: &Path, name: &str, body: &str) -> crate::Skill {
        fs::create_dir_all(base).unwrap();
        fs::write(base.join("SKILL.md"), body).unwrap();
        crate::Skill {
            name: name.to_owned(),
            description: format!("{name} shared"),
            file_path: base.join("SKILL.md").to_string_lossy().into_owned(),
            base_dir: base.to_string_lossy().into_owned(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: crate::SkillSource::User,
            managed_project_boundary: None,
            managed_root: None,
            trusted: true,
        }
    }

    #[tokio::test]
    async fn read_skill_uri_overlays_persona_snapshot_over_shared_catalog() {
        let d = tmpdir();
        let overlap = trusted_test_skill(&d.join("overlap"), "overlap", "shared body");
        let unique_shared = trusted_test_skill(&d.join("shared-asset"), "shared-asset", "shared unique");
        fs::write(d.join("shared-asset/asset.txt"), "shared asset body").unwrap();
        let shared = vec![overlap, unique_shared];
        let provider: SkillSnapshotFn = Arc::new({
            let shared = shared.clone();
            move || shared.clone()
        });
        let persona = Arc::new(vec![
            crate::PersonaSkill {
                name: "overlap".to_owned(),
                description: "persona description".to_owned(),
                content: "persona body".to_owned(),
            },
            crate::PersonaSkill {
                name: "persona-only".to_owned(),
                description: "persona only".to_owned(),
                content: "persona only body".to_owned(),
            },
        ]);
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(&d, Vec::<PathBuf>::new()).expect("workspace"),
            Some(provider.clone()),
            None,
        )
        .await;
        let persona_read = named_tool(
            &create_coding_tools_for_workspace_with_generation_and_persona_skills(
                fixture.workspace.clone(),
                None,
                Some(provider.clone()),
                None,
                None,
                None,
                None,
                fixture.capture(),
                Some(persona),
            )
            .expect("persona tools"),
            "read",
        );
        let overlap_text = text_of(
            &(persona_read.execute)(make_ctx(json!({ "path": "skill://overlap" })))
                .await
                .unwrap(),
        );
        assert!(overlap_text.contains("persona body"), "{overlap_text}");
        assert!(!overlap_text.contains("shared body"), "{overlap_text}");
        let only = text_of(
            &(persona_read.execute)(make_ctx(json!({ "path": "skill://persona-only" })))
                .await
                .unwrap(),
        );
        assert!(only.contains("persona only body"), "{only}");
        let traversal = (persona_read.execute)(make_ctx(json!({ "path": "skill://overlap/../escape" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            traversal.contains("persona skills expose only their main SKILL.md"),
            "{traversal}"
        );
        let asset = text_of(
            &(persona_read.execute)(make_ctx(json!({ "path": "skill://shared-asset/asset.txt" })))
                .await
                .unwrap(),
        );
        assert!(asset.contains("shared asset body"), "{asset}");

        let ordinary_read = named_tool(
            &create_coding_tools_for_workspace_with_generation_and_persona_skills(
                fixture.workspace.clone(),
                None,
                Some(provider),
                None,
                None,
                None,
                None,
                fixture.capture(),
                None,
            )
            .expect("ordinary tools"),
            "read",
        );
        let shared_text = text_of(
            &(ordinary_read.execute)(make_ctx(json!({ "path": "skill://overlap" })))
                .await
                .unwrap(),
        );
        assert!(shared_text.contains("shared body"), "{shared_text}");
        assert!(!shared_text.contains("persona body"), "{shared_text}");
        let missing = (ordinary_read.execute)(make_ctx(json!({ "path": "skill://persona-only" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("unknown or untrusted skill"), "{missing}");
    }

    #[tokio::test]
    async fn read_internal_uri_resolver_returns_resolved_file_content() {
        let d = tmpdir();
        let target = d.join("agent-out.md");
        fs::write(&target, b"agent transcript body\n").unwrap();
        let capabilities = crate::fs::CapabilityFs::new().unwrap();
        let file = capabilities
            .open_exact_file(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(target).unwrap(),
            )
            .unwrap();
        let file = Arc::new(file);
        let resolver = InternalUriResolver::new(move |_uri| Ok(file.clone()));
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(&d, Vec::<PathBuf>::new()).expect("workspace"),
            None,
            Some(resolver),
        )
        .await;
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": "agent://abc" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("agent transcript body"));
    }

    #[tokio::test]
    async fn read_internal_uri_absent_resolver_errors_for_recognized_scheme() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("read");
        for (uri, scheme) in [
            ("agent://abc", "agent"),
            ("history://sess", "history"),
            ("artifact://42", "artifact"),
        ] {
            let err = (tool.execute)(make_ctx(json!({ "path": uri })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("{scheme}:// resolution is unavailable")),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn read_internal_uri_resolver_errors_propagate_without_fallthrough() {
        let d = tmpdir();
        let resolver =
            InternalUriResolver::new(|_uri| Err(anyhow::anyhow!("uri resolution failed: boom")));
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(&d, Vec::<PathBuf>::new()).expect("workspace"),
            None,
            Some(resolver),
        )
        .await;
        let err = (fixture.tool("read").execute)(make_ctx(json!({ "path": "agent://abc" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("uri resolution failed: boom"), "{err}");
    }

    #[tokio::test]
    async fn read_internal_uri_unknown_scheme_falls_through_to_file_path() {
        let d = tmpdir();
        fs::write(d.join("hello.txt"), b"plain file").unwrap();
        let resolver = InternalUriResolver::new(|_uri| {
            Err(anyhow::anyhow!("unexpected internal URI resolution"))
        });
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(&d, Vec::<PathBuf>::new()).expect("workspace"),
            None,
            Some(resolver),
        )
        .await;
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": "hello.txt" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("plain file"));
    }

    #[tokio::test]
    async fn read_directory_returns_eisdir() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("read").execute)(make_ctx(json!({ "path": "." })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("IsADirectory")
                || error.contains("entry type is unsupported")
                || error.contains("invalid filesystem path"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn read_offset_beyond_end_errors() {
        let d = tmpdir();
        fs::write(d.join("small.txt"), b"a\nb\nc\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("read").execute)(make_ctx(json!({
            "path": "small.txt",
            "offset": 99
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("beyond end of file"), "{error}");
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn generation_has_pdftotext(fixture: &ToolGenerationFixture) -> bool {
        crate::process::run_trusted_tool_probe(
            &fixture.generation(),
            crate::process::TrustedToolProbe::PdfToText,
        )
        .is_ok()
    }

    #[tokio::test]
    async fn read_pdf_extracts_text() {
        let path = fixture_path("sample.pdf");
        assert!(path.is_file(), "missing fixture {}", path.display());
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": path })))
            .await
            .unwrap();
        let text = text_of(&result);
        for line in 1..=6 {
            assert!(
                text.contains(&format!("Line {line} of PDF fixture")),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn read_pdf_offset_limit_paginates() {
        let path = fixture_path("sample.pdf");
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let result = (fixture.tool("read").execute)(make_ctx(json!({
            "path": path,
            "offset": 3,
            "limit": 2
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(text.contains("Line 3 of PDF fixture"), "{text}");
        assert!(text.contains("Line 4 of PDF fixture"), "{text}");
        assert!(!text.contains("Line 2 of PDF fixture"), "{text}");
        assert!(!text.contains("Line 5 of PDF fixture"), "{text}");
        assert!(
            text.contains("[3 more lines in file. Use offset=5 to continue.]"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn read_pdf_offset_beyond_end_errors() {
        let path = fixture_path("sample.pdf");
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let error = (fixture.tool("read").execute)(make_ctx(json!({
            "path": path,
            "offset": 99
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Offset 99 is beyond end of file"), "{error}");
    }

    #[tokio::test]
    async fn read_pdf_without_text_layer_notices() {
        let path = fixture_path("no-text.pdf");
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let error = (fixture.tool("read").execute)(make_ctx(json!({ "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("PDF contains no extractable text (scanned or encrypted)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn extract_pdf_missing_binary_is_actionable() {
        let (_application, generation) = pdf_test_generation(Path::new(".")).await;
        let err = extract_pdf_text_with(
            generation,
            Path::new("pdftotext-definitely-missing-xyz"),
            "/nonexistent/sample.pdf",
            make_ctx(json!({})).abort,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            (err.contains("pdftotext failed") && err.contains("exit status"))
                || (err.contains("failed to start pdftotext") && err.contains("not found")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn extract_pdf_abort_cancels_extraction() {
        let path = fixture_path("sample.pdf");
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let error = extract_pdf_text(fixture.generation(), path.to_str().unwrap(), abort)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("PDF text extraction cancelled"), "{error}");
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
        assert_eq!(
            out.len(),
            PDF_EXTRACT_MAX_BYTES,
            "output must be exactly the cap"
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_pdf_eof_before_exit_keeps_nonzero_actionable() {
        let dir = tmpdir();
        let fixture = tool_generation_fixture(&dir).await;
        if !generation_has_pdftotext(&fixture) {
            eprintln!("skipping: pdftotext not available to published generation");
            return;
        }
        let input = dir.join("malformed.pdf");
        fs::write(&input, b"%PDF-1.4\nmalformed\n").unwrap();
        let generation = fixture.generation();
        let executable = crate::process::run_trusted_tool_probe(
            &generation,
            crate::process::TrustedToolProbe::PdfToText,
        )
        .expect("published pdftotext executable");
        let error = extract_pdf_text_with(
            generation,
            executable.as_path(),
            &input.to_string_lossy(),
            make_ctx(json!({})).abort,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("pdftotext failed") && error.contains("exit status"),
            "{error}"
        );
        assert!(
            !error.contains("PDF contains no extractable text"),
            "{error}"
        );
    }

    /// Returns whether the published generation can execute either Office
    /// converter, not merely construct a policy request for a configured path.
    async fn generation_has_office_converter(fixture: &ToolGenerationFixture) -> bool {
        for probe in [
            crate::process::TrustedToolProbe::Pandoc,
            crate::process::TrustedToolProbe::LibreOffice,
        ] {
            let Ok(executable) =
                crate::process::run_trusted_tool_probe(&fixture.generation(), probe)
            else {
                continue;
            };
            let output = crate::process::GovernedProcessContext::new(fixture.generation(), None)
                .run(
                    pi_sandbox::SpawnFamily::DocumentConverter,
                    crate::process::GovernedProcessSpec {
                        program: executable.as_path().to_path_buf(),
                        args: vec!["--version".to_owned()],
                        cwd: executable
                            .as_path()
                            .parent()
                            .unwrap_or_else(|| Path::new("/"))
                            .to_path_buf(),
                        environment: BTreeMap::new(),
                        timeout: Duration::from_secs(5),
                        stdout_bytes: 64 * 1024,
                        stderr_bytes: 64 * 1024,
                        #[cfg(unix)]
                        mount_sources: Vec::new(),
                        #[cfg(unix)]
                        retained_executable: None,
                        #[cfg(unix)]
                        retained_working_directory: None,
                    },
                )
                .await;
            if output.is_ok_and(|output| output.status == Some(0)) {
                return true;
            }
        }
        false
    }

    async fn generation_has_nbconvert(fixture: &ToolGenerationFixture) -> bool {
        let Ok(executable) = crate::process::run_trusted_tool_probe(
            &fixture.generation(),
            crate::process::TrustedToolProbe::Jupyter,
        ) else {
            return false;
        };
        crate::process::GovernedProcessContext::new(fixture.generation(), None)
            .run(
                pi_sandbox::SpawnFamily::DocumentConverter,
                crate::process::GovernedProcessSpec {
                    program: executable.as_path().to_path_buf(),
                    args: vec!["--version".to_owned()],
                    cwd: executable
                        .as_path()
                        .parent()
                        .unwrap_or_else(|| Path::new("/"))
                        .to_path_buf(),
                    environment: BTreeMap::new(),
                    timeout: Duration::from_secs(5),
                    stdout_bytes: 64 * 1024,
                    stderr_bytes: 64 * 1024,
                    #[cfg(unix)]
                    mount_sources: Vec::new(),
                    #[cfg(unix)]
                    retained_executable: None,
                    #[cfg(unix)]
                    retained_working_directory: None,
                },
            )
            .await
            .is_ok_and(|output| output.status == Some(0))
    }
    #[tokio::test]
    async fn read_docx_converts_to_text() {
        let path = fixture_path("sample.docx");
        assert!(path.is_file(), "missing fixture {}", path.display());
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_office_converter(&fixture).await {
            eprintln!("skipping: no office converter available to published generation");
            return;
        }
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": path })))
            .await
            .unwrap();
        let text = text_of(&result);
        for line in 1..=6 {
            assert!(
                text.contains(&format!("Line {line} of docx fixture")),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn read_docx_offset_paginates_converted_text() {
        let path = fixture_path("sample.docx");
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_office_converter(&fixture).await {
            eprintln!("skipping: no office converter available to published generation");
            return;
        }
        let result = (fixture.tool("read").execute)(make_ctx(json!({
            "path": path,
            "offset": 1,
            "limit": 2
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Line 1 of docx fixture") || text.contains("Line 2 of docx fixture"),
            "{text}"
        );
        assert!(text.contains("more lines in file"), "{text}");
    }

    #[tokio::test]
    async fn read_ipynb_converts_to_text() {
        let path = fixture_path("sample.ipynb");
        assert!(path.is_file(), "missing fixture {}", path.display());
        let fixture = tool_generation_fixture(path.parent().unwrap()).await;
        if !generation_has_nbconvert(&fixture).await {
            eprintln!("skipping: jupyter unavailable to published generation");
            return;
        }
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": path })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("answer = 40 + 2"),
            "missing code cell in: {text}"
        );
        assert!(
            text.contains("print(answer)"),
            "missing code cell in: {text}"
        );
    }

    #[tokio::test]
    async fn read_with_limit_and_continuation() {
        let d = tmpdir();
        let content = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(d.join("big.txt"), content).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("read").execute)(make_ctx(json!({
            "path": "big.txt",
            "limit": 3
        })))
        .await
        .unwrap();
        let text = text_of(&res);
        assert!(text.contains("line0"));
        assert!(text.contains("line2"));
        assert!(text.contains("more lines in file"));
    }

    #[tokio::test]
    async fn read_image_appends_non_vision_note_from_current_model() {
        let d = tmpdir();
        fs::write(d.join("image.png"), tiny_png()).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("read");

        let none = (tool.execute)(make_ctx(json!({ "path": "image.png" })))
            .await
            .unwrap();
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
        assert!(
            vision
                .content
                .iter()
                .any(|content| matches!(content, ContentBlock::Image { .. }))
        );
    }

    #[tokio::test]
    async fn read_video_returns_bounded_structured_media() {
        let cwd = tmpdir();
        fs::write(cwd.join("capture.webm"), tiny_webm()).unwrap();
        let fixture = tool_generation_fixture(&cwd).await;
        let result = (fixture.tool("read").execute)(make_ctx(json!({ "path": "capture.webm" })))
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
        let fixture = tool_generation_fixture(&cwd).await;
        let tool = fixture.tool("read");
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
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_INLINE_VIDEO_BYTES + 1)
            .unwrap();
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
        let Some(fixture) = confined_generation_fixture(
            crate::WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace"),
        )
        .await
        else {
            return;
        };
        let tool = fixture.tool("read");
        for path in [
            outside.path().join("image.png"),
            outside.path().join("capture.webm"),
        ] {
            let error = (tool.execute)(make_ctx(json!({ "path": path })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("filesystem capability does not cover"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn read_trusted_uri_image_preserves_resolver_contract() {
        let cwd = tempfile::tempdir().unwrap();
        let resolved = tempfile::NamedTempFile::new().unwrap();
        fs::write(resolved.path(), tiny_png()).unwrap();
        let capabilities = crate::fs::CapabilityFs::new().unwrap();
        let file = capabilities
            .open_exact_file(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(resolved.path().to_path_buf()).unwrap(),
            )
            .unwrap();
        let file = Arc::new(file);
        let resolver = InternalUriResolver::new(move |_uri| Ok(file.clone()));
        let fixture = tool_generation_fixture_with_context(
            crate::WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace"),
            None,
            Some(resolver),
        )
        .await;
        let result =
            (fixture.tool("read").execute)(make_ctx(json!({ "path": "artifact://image" })))
                .await
                .unwrap();
        assert!(
            result
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. }))
        );
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
        let fixture = tool_generation_fixture(cwd.path()).await;
        let tool = fixture.tool("read");
        for path in ["image.png", "capture.webm"] {
            let error = (tool.execute)(make_ctx(json!({ "path": path })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("filesystem operation OpenEntry failed"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn numeric_schemas_accept_floats_and_execution_rejects_unsafe_ranges() {
        let d = tmpdir();
        fs::write(d.join("lines.txt"), b"a\nb\nc\nd\n").unwrap();
        fs::write(d.join("grep.txt"), b"zero\nmatch\nafter\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let read = fixture.tool("read");
        let ls = fixture.tool("ls");
        let find = fixture.tool("find");
        let grep = fixture.tool("grep");
        for (tool, properties) in [
            (&read, &["offset", "limit"][..]),
            (&ls, &["limit"][..]),
            (&find, &["limit"][..]),
            (&grep, &["context", "limit"][..]),
        ] {
            for property in properties {
                assert_eq!(
                    schema_type(tool, property),
                    Some("number"),
                    "{}.{}",
                    tool.name,
                    property
                );
            }
        }

        let read_float = (read.execute)(make_ctx(json!({
            "path": "lines.txt",
            "offset": 2.9,
            "limit": 1.9
        })))
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
        assert_eq!(
            text_of(&grep_clamped).lines().next(),
            Some("grep.txt:2: match")
        );

        let unsafe_number = JS_MAX_SAFE_INTEGER + 2.0;
        let error = (read.execute)(make_ctx(json!({
            "path": "lines.txt",
            "offset": unsafe_number
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("finite safe number"), "{error}");
    }

    #[tokio::test]
    async fn edit_details_preserve_normalized_line_endings_and_bom() {
        let d = tmpdir();
        fs::write(
            d.join("windows.txt"),
            "\u{FEFF}alpha\r\nbeta\r\ngamma\r\n".as_bytes(),
        )
        .unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("edit");
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
        assert!(
            patch.starts_with("--- windows.txt\n+++ windows.txt\n@@"),
            "{patch}"
        );
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
        assert_eq!(
            success.details["phases"][0]["tasks"][0]["content"],
            "compile"
        );
        let first_id = success.details["phases"][0]["tasks"][0]["id"]
            .as_str()
            .expect("todo success id");
        assert!(first_id.starts_with("task-"));
        let repeated = (create_todo_tool(TodoRuntime::memory()).execute)(make_ctx(json!({
            "op": "init",
            "list": [{ "phase": "Build", "items": ["compile"] }]
        })))
        .await
        .expect("independent todo success");
        assert_eq!(
            repeated.details["phases"][0]["tasks"][0]["id"].as_str(),
            Some(first_id)
        );
        assert_eq!(
            success.details["phases"][0]["tasks"][0]["dependsOn"],
            json!([])
        );
        assert_eq!(success.details["phases"][0]["tasks"][0]["ready"], true);
        assert_eq!(
            success.details["phases"][0]["tasks"][0]["waitingOn"],
            json!([])
        );
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
        assert_eq!(
            failure.details["phases"][0]["tasks"][0]["content"],
            "compile"
        );
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
        let prepared =
            tool.prepare_arguments.as_ref().expect("todo preparation")(json!({"op":"view"}))
                .expect("prepare todo");
        assert!(tool.parameters.validate(&prepared).is_ok());

        // OpenAI strict dialect (the cliproxy/gpt-5.6-sol 400 regression):
        // every nullable field must survive the provider-boundary rewrite
        // into `anyOf: [{type: T}, {type: "null"}]` — no type unions may
        // remain, and array items must move into the array branch.
        let mut wire = serde_json::to_value(&tool.parameters).expect("todo wire schema");
        pi_ai::providers::openai_strict_schema_dialect(&mut wire);
        let wire_properties = &wire["properties"];
        assert!(
            wire_properties.get("type").is_none(),
            "root must not carry a type union: {wire_properties:?}"
        );
        for name in ["list", "items", "dependsOn"] {
            let field = &wire_properties[name];
            let branches = field["anyOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} must convert to anyOf: {field}"));
            assert_eq!(branches[0]["type"], json!("array"), "{name}");
            assert_eq!(branches[1]["type"], json!("null"), "{name}");
            assert!(field.get("type").is_none(), "{name} union must be gone");
        }
        // list.items (the phase object) moved into the array branch; nested
        // source-optional agents is required+nullable with items/minItems kept.
        let phase = &wire_properties["list"]["anyOf"][0]["items"];
        assert_eq!(phase["type"], json!("object"));
        let phase_required = phase["required"].as_array().expect("phase required");
        assert!(phase_required.iter().any(|name| name == "phase"));
        assert!(phase_required.iter().any(|name| name == "items"));
        assert!(phase_required.iter().any(|name| name == "agents"));
        let agents = &phase["properties"]["agents"];
        assert_eq!(agents["anyOf"][0]["type"], json!("array"));
        assert_eq!(agents["anyOf"][0]["minItems"], json!(1));
        assert_eq!(agents["anyOf"][0]["items"]["type"], json!("string"));
        assert_eq!(agents["anyOf"][1]["type"], json!("null"));
        // Non-nullable scalar fields stay untouched.
        assert_eq!(
            wire_properties["op"]["enum"]
                .as_array()
                .map_or(0, |e| e.len()),
            10
        );
        assert_eq!(
            wire_properties["cascade"]["anyOf"][0]["type"],
            json!("boolean")
        );
        // The rewritten schema still validates real arguments (dialect is
        // semantics-preserving for data, not just shape).
        assert!(serde_json::from_value::<serde_json::Value>(wire).is_ok());
    }

    #[tokio::test]
    async fn read_ls_find_and_grep_observe_pre_aborted_contexts() {
        let d = tmpdir();
        fs::write(d.join("a.txt"), b"needle\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        for (tool, args) in [
            (fixture.tool("read"), json!({ "path": "a.txt" })),
            (fixture.tool("ls"), json!({})),
            (fixture.tool("find"), json!({ "pattern": "*.txt" })),
            (fixture.tool("grep"), json!({ "pattern": "needle" })),
        ] {
            let (controller, abort) = pi_agent::AbortController::new();
            controller.abort();
            let context = ToolCallContext {
                tool_call_id: "cancelled".to_owned(),
                arguments: args,
                on_update: noop_update(),
                abort,
                model: None,
                approval_token: None,
            };
            let error = (tool.execute)(context).await.unwrap_err().to_string();
            assert_eq!(error, "Operation aborted", "{}", tool.name);
        }
    }
    #[tokio::test]
    async fn factories_expose_read_only_and_name_keyed_surfaces() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        let read_only_names = [
            "read",
            "grep",
            "find",
            "glob",
            "ls",
            "web_search",
            "ast_grep",
        ];
        let read_only = read_only_names
            .iter()
            .map(|name| fixture.tool(name))
            .collect::<Vec<_>>();
        assert_eq!(
            read_only
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            read_only_names
        );
        assert_eq!(fixture.tool("glob").name, "glob");
        assert_eq!(fixture.tool("lsp").name, "lsp");
        assert!(create_tool("ast", &cwd.to_string_lossy()).is_err());
        assert_eq!(
            read_only.iter().map(AgentTool::as_tool_definition).count(),
            7
        );

        let tools = TOOL_NAMES
            .iter()
            .map(|name| fixture.tool(name))
            .collect::<Vec<_>>();
        let by_name = tools
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect::<HashMap<_, _>>();
        assert_eq!(by_name.len(), TOOL_NAMES.len());
        for name in TOOL_NAMES {
            assert_eq!(
                by_name.get(*name).map(|tool| tool.name.as_str()),
                Some(*name)
            );
        }
        let definitions = by_name
            .iter()
            .map(|(name, tool)| (name.clone(), tool.as_tool_definition()))
            .collect::<HashMap<_, _>>();
        assert_eq!(definitions.len(), TOOL_NAMES.len());
        assert_eq!(
            definitions
                .get("read")
                .map(|definition| definition.name.as_str()),
            Some("read")
        );
    }
    #[tokio::test]
    async fn builtin_tools_carry_explicit_capabilities() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        let tools = fixture.coding_tools(None);
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
            ("lsp", ToolCapability::Write),
            ("github", ToolCapability::Write),
            ("memory", ToolCapability::Write),
            ("bash", ToolCapability::Exec),
            ("browser", ToolCapability::Exec),
        ] {
            assert_eq!(named_tool(&tools, name).capability, expected, "{name}");
        }
        assert_eq!(fixture.tool("todo").capability, ToolCapability::Write);
    }

    #[tokio::test]
    async fn inspect_image_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"inspect_image"));
        assert!(tool_snippet("inspect_image").is_some_and(|snippet| {
            snippet.contains("metadata") && snippet.contains("dominant colors")
        }));
        let tool = fixture.tool("inspect_image");
        assert_eq!(tool.name, "inspect_image");
        assert_eq!(tool.capability, ToolCapability::Read);
        assert_eq!(tool.parameters.required, vec!["path".to_string()]);
        assert_eq!(schema_type(&tool, "path"), Some("string"));
    }

    #[tokio::test]
    async fn browser_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"browser"));
        assert_eq!(
            tool_snippet("browser"),
            Some(
                "Automate a headless Chrome/Chromium browser: navigate, click, fill, screenshot, extract, list_tabs, close"
            )
        );
        let tool = fixture.tool("browser");
        assert_eq!(tool.name, "browser");
        assert_eq!(tool.capability, ToolCapability::Exec);
        let action = tool
            .parameters
            .properties
            .get("action")
            .expect("action prop");
        assert_eq!(
            action.schema_type.as_ref().and_then(Value::as_str),
            Some("string")
        );
        assert_eq!(tool.parameters.required, vec!["action"]);
        assert_eq!(tool.as_tool_definition().name, "browser");
    }

    #[tokio::test]
    async fn github_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"github"));
        assert!(tool_snippet("github").is_some_and(|snippet| snippet.contains("GitHub API")));
        let tool = fixture.tool("github");
        assert_eq!(tool.name, "github");
        assert_eq!(tool.capability, ToolCapability::Write);
        let action = tool
            .parameters
            .properties
            .get("action")
            .expect("action prop");
        let actions = action
            .enum_values
            .iter()
            .map(|value| value.as_str().unwrap_or(""))
            .collect::<Vec<_>>();
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
                "search_code",
            ]
        );
        assert!(tool.parameters.required.contains(&"action".to_string()));
        assert_eq!(tool.as_tool_definition().name, "github");
    }

    #[tokio::test]
    async fn memory_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"memory"));
        assert!(tool_snippet("memory").is_some_and(|snippet| {
            snippet.contains("learn") && snippet.contains("recall") && snippet.contains("forget")
        }));
        let tools = fixture.coding_tools(None);
        let tool = named_tool(&tools, "memory");
        assert_eq!(tool.name, "memory");
        assert_eq!(tool.capability, ToolCapability::Write);
        let operation = tool.parameters.properties.get("op").expect("op prop");
        let actions = operation
            .enum_values
            .iter()
            .map(|value| value.as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(actions, ["learn", "recall", "list", "forget"]);
        assert!(tool.parameters.required.contains(&"op".to_string()));
        for property in ["content", "tags", "query", "limit", "tag", "id"] {
            assert!(
                tool.parameters.properties.contains_key(property),
                "missing {property} prop"
            );
        }
        assert_eq!(tool.as_tool_definition().name, "memory");
    }

    #[tokio::test]
    async fn hindsight_tools_are_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        let config = MemoryConfig {
            backend: crate::MemoryBackend::Hindsight,
            hindsight_api_url: Some("https://memory.example.test".to_owned()),
            ..Default::default()
        };
        let resolver: MemoryConfigFn = Arc::new(move || Some(config.clone()));
        let tools = fixture.coding_tools(Some(resolver));
        for name in ["recall", "retain", "reflect"] {
            assert!(TOOL_NAMES.contains(&name), "{name} in TOOL_NAMES");
            assert!(tool_snippet(name).is_some(), "{name} snippet");
            let tool = named_tool(&tools, name);
            assert_eq!(tool.as_tool_definition().name, name);
        }
        let recall = named_tool(&tools, "recall");
        assert_eq!(recall.capability, ToolCapability::Read);
        assert!(recall.parameters.required.contains(&"query".to_string()));
        let retain = named_tool(&tools, "retain");
        assert_eq!(retain.capability, ToolCapability::Write);
        assert!(retain.parameters.required.contains(&"content".to_string()));
        let reflect = named_tool(&tools, "reflect");
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
            &cwd.to_string_lossy(),
            None,
            None,
            None,
            None,
            Some(resolver.clone()),
            None,
        );
        let persona = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(),
            &persona_root,
            None,
            None,
            None,
            None,
            Some(resolver),
            None,
        );
        for tools in [ordinary, persona] {
            let names = tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>();
            assert!(!names.iter().any(|name| name == "memory"), "{names:?}");
            for expected in ["recall", "retain", "reflect"] {
                assert!(names.iter().any(|name| name == expected), "{names:?}");
            }
        }
    }

    #[tokio::test]
    async fn resolver_factories_include_one_browser_without_duplicate_names() {
        let cwd = tmpdir();
        let workspace = crate::WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).unwrap();
        let fixture = tool_generation_fixture_for_workspace(workspace).await;
        let tools = fixture.coding_tools(None);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names.iter().filter(|name| **name == "browser").count(),
            1,
            "{names:?}"
        );
        let unique = names
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), names.len(), "{names:?}");
    }

    #[test]
    fn persona_child_factory_preserves_local_persona_scope_and_off_selection() {
        let cwd = tmpdir();
        let persona_root = tmpdir();
        let local: MemoryConfigFn = Arc::new(|| Some(MemoryConfig::default()));
        let local_tools = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(),
            &persona_root,
            None,
            None,
            None,
            None,
            Some(local),
            None,
        );
        assert!(local_tools.iter().any(|tool| tool.name == "memory"));
        let off: MemoryConfigFn = Arc::new(|| {
            Some(MemoryConfig {
                backend: crate::MemoryBackend::Off,
                ..Default::default()
            })
        });
        let off_tools = create_coding_tools_with_context_and_resolver_for_persona(
            &cwd.to_string_lossy(),
            &persona_root,
            None,
            None,
            None,
            None,
            Some(off),
            None,
        );
        assert!(!off_tools.iter().any(|tool| matches!(
            tool.name.as_str(),
            "memory" | "recall" | "retain" | "reflect"
        )));
    }

    #[tokio::test]
    async fn mcp_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"mcp"));
        assert!(tool_snippet("mcp").is_some_and(|snippet| {
            snippet.contains("list_servers")
                && snippet.contains("list_tools")
                && snippet.contains("call")
        }));
        let tool = fixture.tool("mcp");
        assert_eq!(tool.name, "mcp");
        assert_eq!(tool.capability, ToolCapability::Write);
        for property in ["action", "server", "tool", "args"] {
            assert!(
                tool.parameters.properties.contains_key(property),
                "missing {property} prop"
            );
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        assert_eq!(tool.as_tool_definition().name, "mcp");
    }

    #[tokio::test]
    async fn debug_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"debug"));
        assert!(tool_snippet("debug").is_some_and(|snippet| {
            snippet.contains("launch")
                && snippet.contains("set_breakpoint")
                && snippet.contains("continue_")
        }));
        let tool = fixture.tool("debug");
        assert_eq!(tool.name, "debug");
        assert_eq!(tool.capability, ToolCapability::Write);
        for property in [
            "action",
            "adapter",
            "program",
            "file",
            "line",
            "variables_reference",
            "expression",
        ] {
            assert!(
                tool.parameters.properties.contains_key(property),
                "missing {property} prop"
            );
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        assert_eq!(tool.as_tool_definition().name, "debug");
    }

    #[tokio::test]
    async fn eval_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"eval"));
        assert!(tool_snippet("eval").is_some_and(|snippet| {
            snippet.contains("python") && snippet.contains("QuickJS") && snippet.contains("syntax")
        }));
        let tool = fixture.tool("eval");
        assert_eq!(tool.name, "eval");
        assert_eq!(tool.capability, ToolCapability::Exec);
        for property in ["language", "code", "timeout"] {
            assert!(
                tool.parameters.properties.contains_key(property),
                "missing {property} prop"
            );
        }
        assert!(tool.parameters.required.contains(&"language".to_string()));
        assert!(tool.parameters.required.contains(&"code".to_string()));
        assert_eq!(tool.as_tool_definition().name, "eval");
    }

    #[tokio::test]
    async fn notebook_tool_is_registered_with_schema_and_snippet() {
        let cwd = tmpdir();
        let fixture = tool_generation_fixture(&cwd).await;
        assert!(TOOL_NAMES.contains(&"notebook"));
        assert!(tool_snippet("notebook").is_some_and(|snippet| {
            snippet.contains("ipynb") && snippet.contains("execute") && snippet.contains("edit")
        }));
        let tool = fixture.tool("notebook");
        assert_eq!(tool.name, "notebook");
        assert_eq!(tool.capability, ToolCapability::Exec);
        for property in [
            "action",
            "path",
            "cell",
            "write",
            "cell_type",
            "source",
            "timeout",
        ] {
            assert!(
                tool.parameters.properties.contains_key(property),
                "missing {property} prop"
            );
        }
        assert!(tool.parameters.required.contains(&"action".to_string()));
        assert!(tool.parameters.required.contains(&"path".to_string()));
        assert_eq!(tool.as_tool_definition().name, "notebook");
    }

    #[tokio::test]
    async fn read_rejects_absolute_and_parent_relative_external_files() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let external = root.path().join("outside.txt");
        fs::write(&external, "external-bytes").expect("external file");
        let Some(fixture) = confined_generation_fixture(
            crate::WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).expect("workspace"),
        )
        .await
        else {
            return;
        };
        let tool = fixture.tool("read");
        for path in [
            external.to_string_lossy().into_owned(),
            "../outside.txt".to_owned(),
        ] {
            let error = (tool.execute)(make_ctx(json!({ "path": path })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("filesystem capability does not cover"),
                "{error}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_symlink_to_external_file() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        let target = external.path().join("secret.txt");
        fs::write(&target, "linked-secret").expect("secret");
        std::os::unix::fs::symlink(&target, cwd.path().join("alias.txt")).expect("symlink");
        let fixture = tool_generation_fixture(cwd.path()).await;
        let error = (fixture.tool("read").execute)(make_ctx(json!({ "path": "alias.txt" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("filesystem operation OpenEntry failed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn write_and_edit_stay_within_published_workspace_roots() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        let added = root.path().join("added");
        let external = root.path().join("external");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&added).expect("added root");
        fs::create_dir_all(&external).expect("external directory");
        let workspace = crate::WorkspaceRoots::new(&cwd, [&added]).expect("workspace");
        let Some(fixture) = confined_generation_fixture(workspace).await else {
            return;
        };
        let tools = fixture.coding_tools(None);
        let write = named_tool(&tools, "write");
        let edit = named_tool(&tools, "edit");

        let published = added.join("published.txt");
        (write.execute)(make_ctx(json!({
            "path": published,
            "content": "before"
        })))
        .await
        .expect("write published root");
        (edit.execute)(make_ctx(json!({
            "path": published,
            "edits": [{ "oldText": "before", "newText": "after" }]
        })))
        .await
        .expect("edit published root");
        assert_eq!(fs::read_to_string(&published).unwrap(), "after");

        for path in [
            external.join("outside.txt"),
            root.path().join("parent-relative.txt"),
        ] {
            let write_error = (write.execute)(make_ctx(json!({
                "path": path,
                "content": "outside"
            })))
            .await
            .unwrap_err()
            .to_string();
            assert!(
                write_error.contains("filesystem capability does not cover"),
                "{write_error}"
            );

            fs::write(&path, "before").expect("external edit target");
            let edit_error = (edit.execute)(make_ctx(json!({
                "path": path,
                "edits": [{ "oldText": "before", "newText": "after" }]
            })))
            .await
            .unwrap_err()
            .to_string();
            assert!(
                edit_error.contains("filesystem capability does not cover"),
                "{edit_error}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "before");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_reject_symlinks_to_external_regular_files() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        let target = external.path().join("target.txt");
        let alias = cwd.path().join("alias.txt");
        fs::write(&target, "before").expect("target");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");
        let fixture = tool_generation_fixture(cwd.path()).await;
        let write = fixture.tool("write");
        let edit = fixture.tool("edit");

        let write_error = (write.execute)(make_ctx(json!({
            "path": "alias.txt",
            "content": "written"
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            write_error.contains("filesystem operation OpenEntry failed")
                || write_error.contains("entry type is unsupported"),
            "{write_error}"
        );

        let edit_error = (edit.execute)(make_ctx(json!({
            "path": "alias.txt",
            "edits": [{ "oldText": "before", "newText": "edited" }]
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            edit_error.contains("filesystem operation OpenEntry failed")
                || edit_error.contains("entry type is unsupported"),
            "{edit_error}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert!(alias.is_symlink());
    }

    #[tokio::test]
    async fn write_and_edit_reject_non_regular_and_invalid_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let directory = cwd.path().join("directory");
        fs::create_dir(&directory).expect("directory");
        let fixture = tool_generation_fixture(cwd.path()).await;
        let write = fixture.tool("write");
        let edit = fixture.tool("edit");

        let write_directory_error = (write.execute)(make_ctx(json!({
            "path": "directory",
            "content": "nope"
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            write_directory_error.contains("entry type is unsupported"),
            "{write_directory_error}"
        );

        let edit_directory_error = (edit.execute)(make_ctx(json!({
            "path": "directory",
            "edits": [{ "oldText": "x", "newText": "y" }]
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            edit_directory_error.contains("entry type is unsupported")
                || edit_directory_error.contains("Could not find the exact text in directory"),
            "{edit_directory_error}"
        );

        let empty_error = (write.execute)(make_ctx(json!({ "path": "", "content": "nope" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(empty_error.contains("must not be empty"), "{empty_error}");

        let nul_error = (edit.execute)(make_ctx(json!({
            "path": "bad\0path",
            "edits": [{ "oldText": "x", "newText": "y" }]
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(nul_error.contains("NUL byte"), "{nul_error}");
    }

    #[tokio::test]
    async fn write_creates_file_and_reports_utf16_length() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("write").execute)(make_ctx(json!({
            "path": "sub/out.txt",
            "content": "hello"
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "Successfully wrote 5 bytes to sub/out.txt");
        assert_eq!(fs::read_to_string(d.join("sub/out.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn write_reports_utf16_length_for_astral() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("write").execute)(make_ctx(json!({
            "path": "emoji.txt",
            "content": "\u{1F600}"
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "Successfully wrote 2 bytes to emoji.txt");
    }

    #[tokio::test]
    async fn write_missing_required_args_error_without_creating_files() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("write");

        let missing_content = (tool.execute)(make_ctx(json!({ "path": "m.txt" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            missing_content.contains("missing required argument"),
            "{missing_content}"
        );
        assert!(missing_content.contains("content"), "{missing_content}");
        assert!(!d.join("m.txt").exists());

        let missing_path = (tool.execute)(make_ctx(json!({ "content": "x" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            missing_path.contains("missing required argument"),
            "{missing_path}"
        );
        assert!(missing_path.contains("path"), "{missing_path}");
    }

    #[tokio::test]
    async fn write_appends_bounded_continuation_chunks() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let write = fixture.tool("write");
        let first = (write.execute)(make_ctx(json!({
            "path": "chunked.txt",
            "content": "first-"
        })))
        .await
        .expect("initial chunk");
        let second = (write.execute)(make_ctx(json!({
            "path": "chunked.txt",
            "content": "second",
            "append": true
        })))
        .await
        .expect("continuation chunk");

        assert_eq!(text_of(&first), "Successfully wrote 6 bytes to chunked.txt");
        assert_eq!(
            text_of(&second),
            "Successfully appended 6 bytes to chunked.txt"
        );
        assert_eq!(fs::read(d.join("chunked.txt")).unwrap(), b"first-second");
    }

    #[tokio::test]
    async fn write_rejects_append_without_initial_chunk() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("write").execute)(make_ctx(json!({
            "path": "missing.txt",
            "content": "second",
            "append": true
        })))
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("Cannot append to missing file"), "{error}");
        assert!(!d.join("missing.txt").exists());
    }

    #[tokio::test]
    async fn write_explicit_empty_content_creates_empty_file() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("write").execute)(make_ctx(json!({
            "path": "empty.txt",
            "content": ""
        })))
        .await
        .expect("explicit empty content is schema-valid");
        assert!(text_of(&res).contains("Successfully wrote 0 bytes"));
        assert_eq!(fs::read(d.join("empty.txt")).unwrap(), b"");
    }

    #[tokio::test]
    async fn edit_applies_unique_replacement() {
        let d = tmpdir();
        fs::write(d.join("e.txt"), b"fn foo() {\n    return 1;\n}\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("edit").execute)(make_ctx(json!({
            "path": "e.txt",
            "edits": [{ "oldText": "return 1;", "newText": "return 2;" }]
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 1 block(s) in e.txt.");
        assert_eq!(
            fs::read_to_string(d.join("e.txt")).unwrap(),
            "fn foo() {\n    return 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn edit_multiple_disjoint_edits() {
        let d = tmpdir();
        fs::write(d.join("m.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("edit").execute)(make_ctx(json!({
            "path": "m.txt",
            "edits": [
                { "oldText": "alpha", "newText": "ALPHA" },
                { "oldText": "gamma", "newText": "GAMMA" }
            ]
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 2 block(s) in m.txt.");
        assert_eq!(
            fs::read_to_string(d.join("m.txt")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
    }

    #[tokio::test]
    async fn edit_duplicate_oldtext_rejected_unchanged() {
        let d = tmpdir();
        fs::write(d.join("d.txt"), b"x\nx\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("edit").execute)(make_ctx(json!({
            "path": "d.txt",
            "edits": [{ "oldText": "x", "newText": "y" }]
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 occurrences"), "{error}");
        assert_eq!(fs::read_to_string(d.join("d.txt")).unwrap(), "x\nx\n");
    }

    #[tokio::test]
    async fn edit_legacy_oldtext_newtext_folded() {
        let d = tmpdir();
        fs::write(d.join("l.txt"), b"alpha\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("edit");
        let prepared = tool.prepare_arguments.clone().unwrap()(
            json!({ "path": "l.txt", "oldText": "alpha", "newText": "beta" }),
        )
        .unwrap();
        let res = (tool.execute)(make_ctx(prepared)).await.unwrap();
        assert_eq!(text_of(&res), "Successfully replaced 1 block(s) in l.txt.");
        assert_eq!(fs::read_to_string(d.join("l.txt")).unwrap(), "beta\n");
    }

    #[tokio::test]
    async fn edit_rejects_non_utf8_without_rewrite() {
        let d = tmpdir();
        let bytes = b"ok\xffmore";
        fs::write(d.join("bin.txt"), bytes).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("edit").execute)(make_ctx(json!({
            "path": "bin.txt",
            "edits": [{ "oldText": "ok", "newText": "OK" }]
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
        assert_eq!(fs::read(d.join("bin.txt")).unwrap(), bytes);
    }

    #[tokio::test]
    async fn bash_success() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "echo hello world"
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "hello world\n");
    }

    #[tokio::test]
    async fn bash_closes_stdin_and_applies_non_interactive_environment() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "if read line; then printf 'unexpected:%s' \"$line\"; else printf 'stdin-closed:%s:%s:%s' \"$GIT_TERMINAL_PROMPT\" \"$GH_PROMPT_DISABLED\" \"$PAGER\"; fi",
            "timeout": 2
        })))
        .await
        .expect("non-interactive bash");
        assert_eq!(text_of(&result), "stdin-closed:0:1:cat");
    }

    /// `input` is a closed union of string | false on the real bash schema.
    /// JSON false means no stdin (the shape models actually send). true,
    /// objects, arrays, and numbers are rejected. Strings, including empty,
    /// stay strings. `command` stays a required string.
    #[tokio::test]
    async fn bash_input_false_means_no_stdin() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        let definition = tool.as_tool_definition();
        let input_schema = tool.parameters.properties.get("input").expect("input schema");
        let description = input_schema
            .description
            .as_deref()
            .expect("input description");
        assert!(
            description.contains("false") && description.to_ascii_lowercase().contains("no stdin"),
            "input description must document that false means no stdin: {description}"
        );
        assert!(
            input_schema
                .schema_type
                .as_ref()
                .is_none_or(|ty| ty.as_str() != Some("boolean")),
            "input must not be an unconstrained boolean"
        );
        assert!(
            input_schema.any_of.iter().any(|branch| {
                branch.schema_type.as_ref().and_then(Value::as_str) == Some("string")
            }),
            "input union is missing its string branch"
        );
        assert!(
            input_schema
                .any_of
                .iter()
                .any(|branch| branch.const_value == Some(json!(false))),
            "input union is missing const false"
        );
        assert!(
            input_schema.any_of.iter().all(|branch| {
                branch.const_value != Some(json!(true))
                    && !branch.enum_values.iter().any(|value| value == &json!(true))
            }),
            "true must not be a member of the input union"
        );

        let prepare = tool
            .prepare_arguments
            .clone()
            .expect("bash prepare");
        let validate = move |arguments: Value| {
            let prepared = prepare(arguments).expect("bash prepare echoes arguments");
            pi_ai::validate_tool_arguments(&definition, &prepared)
        };

        let no_stdin = json!({
            "command": "printf ok",
            "timeout": 5,
            "pty": false,
            "input": false
        });
        let validated = validate(no_stdin.clone()).expect("input false is schema-valid");
        assert_eq!(validated["command"], json!("printf ok"));
        assert_eq!(validated["timeout"], json!(5));
        assert_eq!(validated["pty"], json!(false));
        assert_eq!(validated["input"], json!(false));

        for input in [json!(""), json!("secret-line"), json!("line\nwith newline")] {
            let validated = validate(json!({"command": "printf ok", "input": input}))
                .expect("string stdin is schema-valid");
            assert_eq!(validated["input"], input, "string stdin must be preserved");
        }

        for input in [json!(true), json!({"a": 1}), json!(["x"]), json!(1)] {
            let error = validate(json!({"command": "printf ok", "input": input}))
                .expect_err("non-string, non-false input must fail schema")
                .to_string();
            assert!(error.contains("input"), "{error}");
        }
        for command in [json!(1), json!(true), json!(["printf"]), json!({"c": "printf"})] {
            let error = validate(json!({"command": command, "input": false}))
                .expect_err("non-string command must fail schema")
                .to_string();
            assert!(error.contains("command"), "{error}");
        }

        let printed = (tool.execute)(make_ctx(no_stdin))
            .await
            .expect("printf with input false");
        assert_eq!(text_of(&printed), "ok");

        let false_stdin = (tool.execute)(make_ctx(json!({
            "command": "cat",
            "pty": true,
            "input": false,
            "timeout": 10
        })))
        .await
        .expect("false means no stdin bytes");
        assert_eq!(text_of(&false_stdin), "(no output)");

        let empty_stdin = (tool.execute)(make_ctx(json!({
            "command": "cat",
            "pty": true,
            "input": "",
            "timeout": 10
        })))
        .await
        .expect("empty string stdin writes no bytes");
        assert_eq!(text_of(&empty_stdin), "(no output)");

        let echoed = (tool.execute)(make_ctx(json!({
            "command": "read line; printf 'got:%s' \"$line\"",
            "pty": true,
            "input": "secret-line",
            "timeout": 10
        })))
        .await
        .expect("non-empty string stdin is written");
        assert!(
            text_of(&echoed).contains("got:secret-line"),
            "string stdin was not preserved: {}",
            text_of(&echoed)
        );

        for input in [json!(true), json!({"a": 1}), json!(["x"]), json!(1)] {
            let error = (tool.execute)(make_ctx(json!({
                "command": "printf ok",
                "input": input
            })))
            .await
            .expect_err("runtime must reject input that is not a string or false")
            .to_string();
            assert!(error.contains("invalid input"), "{error}");
            assert!(error.contains("false means no stdin"), "{error}");
        }
        for command in [json!(1), json!(true), json!(["printf"]), json!({"c": "printf"})] {
            let error = (tool.execute)(make_ctx(json!({
                "command": command,
                "input": false
            })))
            .await
            .expect_err("runtime must reject a non-string command")
            .to_string();
            assert!(error.contains("command"), "{error}");
            assert!(error.contains("must be a string"), "{error}");
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({ "command": "exit 7" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Command exited with code 7"), "{error}");
    }

    #[tokio::test]
    async fn bash_unmatched_quote_settles_with_exit_two() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "printf \"unterminated"
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Command exited with code 2"), "{error}");
        assert!(error.contains("unexpected EOF"), "{error}");
    }

    #[tokio::test]
    async fn bash_nested_remote_quoting_remains_valid() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "bash -c 'printf \"%s\\n\" \"nested value\"'"
        })))
        .await
        .expect("valid nested quoting");
        assert_eq!(text_of(&result), "nested value\n");
    }

    /// Deterministic queue-vs-execution proof through the tool surface:
    /// while the brush lock is held via the test seam, the queued run waits
    /// (nothing executes, no queue bound fails it); once the seam guard
    /// releases, the run crosses the start boundary and hits the execution
    /// timeout exactly once.
    #[tokio::test]
    async fn bash_timeout_kills_command() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let held = fixture.hold_brush_exec_lock().await;
        let run = tokio::spawn({
            let tool = fixture.tool("bash");
            async move {
                (tool.execute)(make_ctx(json!({
                    "command": "sleep 30",
                    "timeout": 0.5
                })))
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !run.is_finished(),
            "a queued run must wait for the held lock instead of failing at a bound"
        );
        drop(held);
        let error = run
            .await
            .expect("queued run joins")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Command execution timed out after"),
            "a freed brush lock must let the run cross the start boundary: {error}"
        );
        assert!(
            !error.contains("could not start within"),
            "the queue wait must never fail the start boundary: {error}"
        );
    }

    /// The execution timeout reports the measured monotonic run window plus
    /// the configured limit — never the raw configured value as elapsed time.
    #[tokio::test]
    async fn bash_execution_timeout_reports_measured_elapsed_with_limit() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        let started = Instant::now();
        let error = (tool.execute)(make_ctx(json!({
            "command": "sleep 30",
            "timeout": 0.5
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("(limit 0.5 seconds)"), "{error}");
        let measured: f64 = error
            .split("Command execution timed out after ")
            .nth(1)
            .and_then(|rest| rest.split(" seconds").next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("measured elapsed missing in message: {error}"));
        assert!(
            (0.4..=1.0).contains(&measured),
            "measured execution window must sit near the configured limit: {measured} in {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "execution timeout must bound the run: {:?}",
            started.elapsed()
        );
    }

    /// While another shell command holds the brush lock, a queued second
    /// command waits without running; once the holder releases, the executor
    /// runs it — the queue wait is unbounded and never measured as run time.
    #[tokio::test]
    async fn bash_queued_command_waits_then_runs_after_holder_releases() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let gate = d.join("gate");
        let marker = d.join("queued-marker");
        let holder = tokio::spawn({
            let tool = fixture.tool("bash");
            let gate = gate.clone();
            async move {
                let command = format!(
                    "printf started > holder-marker; while [ ! -s {} ]; do sleep 0.05; done",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command }))).await
            }
        });
        wait_for_file(&d.join("holder-marker")).await;
        let queued = tokio::spawn({
            let tool = fixture.tool("bash");
            let marker_path = marker.display().to_string();
            async move {
                (tool.execute)(make_ctx(json!({
                    "command": format!("echo queued > {marker_path}"),
                    "timeout": 0.2
                })))
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !queued.is_finished(),
            "the queued command must wait for the holder, not fail at a bound"
        );
        assert!(
            !marker.exists(),
            "the brush executor must not have run the queued command while queued"
        );
        fs::write(&gate, b"release").expect("release the holder gate");
        holder
            .await
            .expect("holder task joins")
            .expect("holder completes after the gate opens");
        let result = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("queued command resolves after release")
            .expect("queued task joins")
            .expect("queued command succeeds");
        assert!(
            marker.exists(),
            "the executor must run the queued command after the holder releases"
        );
    }

    /// A caller aborting while queued behind the brush lock resolves with the
    /// before-start abort error, and the command never runs.
    #[tokio::test]
    async fn bash_abort_while_queued_reports_aborted_before_start() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let gate = d.join("gate");
        let holder = tokio::spawn({
            let tool = fixture.tool("bash");
            let gate = gate.clone();
            async move {
                let command = format!(
                    "printf started > holder-marker; while [ ! -s {} ]; do sleep 0.05; done",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command }))).await
            }
        });
        wait_for_file(&d.join("holder-marker")).await;
        let (controller, abort) = pi_agent::AbortController::new();
        let queued = tokio::spawn({
            let tool = fixture.tool("bash");
            async move {
                let mut context = make_ctx(json!({ "command": "echo queued > queued-marker" }));
                context.abort = abort;
                (tool.execute)(context).await
            }
        });
        // Give the queued call a moment to reach the lock wait; the holder
        // still owns the brush lock (the gate opens only below), so the abort
        // deterministically fires while queued.
        tokio::time::sleep(Duration::from_millis(150)).await;
        controller.abort();
        let error = queued
            .await
            .expect("queued task joins")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Command aborted before start"),
            "abort while queued must report a before-start abort: {error}"
        );
        assert!(
            !error.contains("Command aborted\n"),
            "pre-start abort must be distinguishable from a running abort: {error}"
        );
        assert!(
            !d.join("queued-marker").exists(),
            "the queued command must not run after a pre-start abort"
        );
        fs::write(&gate, b"release").expect("release the holder gate");
        holder
            .await
            .expect("holder task joins")
            .expect("holder completes after the gate opens");
    }

    /// A caller aborting a running command reports the running abort error:
    /// distinguishable from the before-start abort even though both start
    /// with "Command aborted".
    #[tokio::test]
    async fn bash_abort_while_running_reports_running_abort() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let (controller, abort) = pi_agent::AbortController::new();
        let running = tokio::spawn({
            let tool = fixture.tool("bash");
            async move {
                let mut context = make_ctx(json!({
                    "command": "printf started > running-marker; sleep 30"
                }));
                context.abort = abort;
                (tool.execute)(context).await
            }
        });
        // The marker only appears once the command crossed the start boundary
        // and is actually running (it holds the brush lock), so the abort
        // deterministically fires mid-run.
        wait_for_file(&d.join("running-marker")).await;
        controller.abort();
        let error = running
            .await
            .expect("running task joins")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Command aborted"), "{error}");
        assert!(
            !error.contains("before start"),
            "a running abort must not classify as pre-start: {error}"
        );
    }

    /// Danger-mode brush commands stay serialized on the session scheduling lock:
    /// the second command cannot cross its start boundary until the first
    /// finishes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn danger_brush_commands_remain_serialized() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        assert!(
            Arc::ptr_eq(
                &fixture.application.session().brush_exec_lock(),
                &fixture.generation().brush_exec_lock(),
            ),
            "generation must carry the Session scheduling lock, not a second identity"
        );
        let gate = d.join("gate");
        let first = tokio::spawn({
            let tool = fixture.tool("bash");
            let gate = gate.clone();
            async move {
                let command = format!(
                    "printf started > c1.start; while [ ! -s {} ]; do sleep 0.05; done; printf done > c1.end",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command }))).await
            }
        });
        let second = tokio::spawn({
            let tool = fixture.tool("bash");
            async move {
                (tool.execute)(make_ctx(json!({
                    "command": "printf started > c2.start; echo ok > c2.out",
                    "timeout": 30
                })))
                .await
            }
        });
        wait_for_file(&d.join("c1.start")).await;
        for _ in 0..20 {
            assert!(
                !d.join("c2.start").exists(),
                "second brush command started while the first still held the lock"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        fs::write(&gate, b"release").expect("release the first command");
        first
            .await
            .expect("first task joins")
            .expect("first command completes");
        second
            .await
            .expect("second task joins")
            .expect("queued second command completes after the holder releases");
        let c2_out = fs::read_to_string(d.join("c2.out")).expect("c2.out exists");
        assert_eq!(c2_out, "ok\n");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reviewer_hold_does_not_block_coordinator() {
        let reviewer_dir = tmpdir();
        let coordinator_dir = tmpdir();
        let reviewer = tool_generation_fixture(&reviewer_dir).await;
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        assert!(
            !Arc::ptr_eq(
                &reviewer.generation().brush_exec_lock(),
                &coordinator.generation().brush_exec_lock(),
            ),
            "coordinator and reviewer sessions must own distinct scheduling locks"
        );
        let gate = reviewer_dir.join("gate");
        let reviewer_run = tokio::spawn({
            let tool = reviewer.tool("bash");
            let gate = gate.clone();
            async move {
                let command = format!(
                    "printf started > r.ready; while [ ! -s {} ]; do sleep 0.05; done",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command, "timeout": 30 }))).await
            }
        });
        wait_for_file(&reviewer_dir.join("r.ready")).await;
        let result = (coordinator.tool("bash").execute)(make_ctx(json!({
            "command": "printf ok",
            "timeout": 0.5
        })))
        .await
        .expect("coordinator must run while a reviewer holds its own tree's lock");
        assert_eq!(text_of(&result), "ok");
        fs::write(&gate, b"release").expect("release reviewer");
        reviewer_run
            .await
            .expect("reviewer task joins")
            .expect("reviewer command completes");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn coordinator_and_subagent_brush_commands_run_concurrently() {
        let coordinator_dir = tmpdir();
        let subagent_dir = tmpdir();
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        let subagent = tool_generation_fixture(&subagent_dir).await;
        assert!(
            !Arc::ptr_eq(
                &coordinator.generation().brush_exec_lock(),
                &subagent.generation().brush_exec_lock(),
            ),
            "coordinator and subagent session trees must own distinct brush locks"
        );
        let coordinator_gate = coordinator_dir.join("gate");
        let subagent_gate = subagent_dir.join("gate");
        let coordinator_run = tokio::spawn({
            let tool = coordinator.tool("bash");
            let gate = coordinator_gate.clone();
            async move {
                let command = format!(
                    "printf started > c.start; while [ ! -s {} ]; do sleep 0.05; done",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command, "timeout": 30 }))).await
            }
        });
        let subagent_run = tokio::spawn({
            let tool = subagent.tool("bash");
            let gate = subagent_gate.clone();
            async move {
                let command = format!(
                    "printf started > s.start; while [ ! -s {} ]; do sleep 0.05; done",
                    gate.display()
                );
                (tool.execute)(make_ctx(json!({ "command": command, "timeout": 30 }))).await
            }
        });
        wait_for_file(&coordinator_dir.join("c.start")).await;
        wait_for_file(&subagent_dir.join("s.start")).await;
        assert!(
            !coordinator_gate.exists() && !subagent_gate.exists(),
            "both session trees must have started while the other still held its run"
        );
        fs::write(&coordinator_gate, b"release").expect("release coordinator");
        fs::write(&subagent_gate, b"release").expect("release subagent");
        coordinator_run
            .await
            .expect("coordinator task joins")
            .expect("coordinator command must not queue-fail");
        subagent_run
            .await
            .expect("subagent task joins")
            .expect("subagent command must not queue-fail");
    }

    #[cfg(target_os = "linux")]
    async fn timeout_tracked_sleep_isolated(
        timed_out: &ToolGenerationFixture,
        timed_out_dir: &Path,
        timed_out_stem: &str,
        survivor: TrackedSleepRun,
    ) -> TrackedSleepRun {
        let error = (timed_out.tool("bash").execute)(make_ctx(json!({
            "command": tracked_sleep_command(timed_out_stem),
            "timeout": 0.5
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("timed out"),
            "session must execute then time out, not queue: {error}"
        );
        assert!(
            !error.contains("could not start"),
            "session must not queue behind the survivor: {error}"
        );
        wait_for_file(&timed_out_dir.join(format!("{timed_out_stem}.pid"))).await;
        let pid = read_recorded_pid(&timed_out_dir.join(format!("{timed_out_stem}.pid")));
        let start_time = read_recorded_start_time(&timed_out_dir.join(format!("{timed_out_stem}.start")));
        wait_until_identity_gone(pid, start_time).await;
        assert!(
            linux_pid_identity_live(survivor.pid, survivor.start_time),
            "survivor pid {} start {} must survive the other session timeout",
            survivor.pid,
            survivor.start_time
        );
        survivor
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn subagent_brush_timeout_reaps_only_own_descendants() {
        let coordinator_dir = tmpdir();
        let subagent_dir = tmpdir();
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        let subagent = tool_generation_fixture(&subagent_dir).await;
        let coordinator_held =
            spawn_tracked_sleep(&coordinator, &coordinator_dir, "coord", 30.0).await;
        let coordinator_held = timeout_tracked_sleep_isolated(
            &subagent,
            &subagent_dir,
            "sub",
            coordinator_held,
        )
        .await;
        abort_tracked_sleep(coordinator_held).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn coordinator_brush_timeout_reaps_only_own_descendants() {
        let coordinator_dir = tmpdir();
        let subagent_dir = tmpdir();
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        let subagent = tool_generation_fixture(&subagent_dir).await;
        let subagent_held = spawn_tracked_sleep(&subagent, &subagent_dir, "sub", 30.0).await;
        let subagent_held = timeout_tracked_sleep_isolated(
            &coordinator,
            &coordinator_dir,
            "coord",
            subagent_held,
        )
        .await;
        abort_tracked_sleep(subagent_held).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn subagent_brush_abort_reaps_only_own_descendants() {
        let coordinator_dir = tmpdir();
        let subagent_dir = tmpdir();
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        let subagent = tool_generation_fixture(&subagent_dir).await;
        let coordinator_held =
            spawn_tracked_sleep(&coordinator, &coordinator_dir, "coord", 30.0).await;
        let subagent_held = spawn_tracked_sleep(&subagent, &subagent_dir, "sub", 30.0).await;
        abort_tracked_sleep(subagent_held).await;
        assert!(
            linux_pid_identity_live(coordinator_held.pid, coordinator_held.start_time),
            "coordinator pid {} start {} must survive the subagent abort",
            coordinator_held.pid,
            coordinator_held.start_time
        );
        abort_tracked_sleep(coordinator_held).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn simultaneous_reviewer_aborts_do_not_reap_coordinator() {
        let coordinator_dir = tmpdir();
        let reviewer_a_dir = tmpdir();
        let reviewer_b_dir = tmpdir();
        let coordinator = tool_generation_fixture(&coordinator_dir).await;
        let reviewer_a = tool_generation_fixture(&reviewer_a_dir).await;
        let reviewer_b = tool_generation_fixture(&reviewer_b_dir).await;
        let coordinator_held =
            spawn_tracked_sleep(&coordinator, &coordinator_dir, "coord", 30.0).await;
        let reviewer_a_held =
            spawn_tracked_sleep(&reviewer_a, &reviewer_a_dir, "ra", 30.0).await;
        let reviewer_b_held =
            spawn_tracked_sleep(&reviewer_b, &reviewer_b_dir, "rb", 30.0).await;
        reviewer_a_held.controller.abort();
        reviewer_b_held.controller.abort();
        join_aborted_tracked_sleep(reviewer_a_held).await;
        join_aborted_tracked_sleep(reviewer_b_held).await;
        assert!(
            linux_pid_identity_live(coordinator_held.pid, coordinator_held.start_time),
            "coordinator pid {} start {} must survive simultaneous reviewer aborts",
            coordinator_held.pid,
            coordinator_held.start_time
        );
        abort_tracked_sleep(coordinator_held).await;
    }

    /// Danger-mode PTY runs bypass the brush execution lock entirely: the
    /// command executes and returns while the test seam still holds the lock.
    #[tokio::test]
    async fn danger_pty_bypasses_brush_exec_lock() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let held = fixture.hold_brush_exec_lock().await;
        let result = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "printf pty-unblocked",
            "pty": true
        })))
        .await
        .expect("danger PTY run must bypass the brush lock");
        drop(held);
        assert!(
            text_of(&result).contains("pty-unblocked"),
            "PTY output must come from a run that bypassed the brush lock: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn bash_invalid_timeout_rejected() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "echo hi",
            "timeout": -1
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Invalid timeout"));
    }

    #[tokio::test]
    async fn bash_missing_command_errors_actionably() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({})))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing required argument"), "{error}");
        assert!(error.contains("command"), "{error}");
    }

    #[tokio::test]
    async fn bash_explicit_empty_command_follows_schema() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("bash").execute)(make_ctx(json!({ "command": "" })))
            .await
            .expect("explicit empty command is schema-valid");
        assert_eq!(text_of(&res), "(no output)");
    }

    #[tokio::test]
    async fn bash_streams_partial_update() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let updates = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = updates.clone();
        let on_update: ToolUpdateFn = Arc::new(move |result: AgentToolResult| {
            if let Some(ContentBlock::Text { text, .. }) = result.content.first() {
                captured.lock().push(text.clone());
            }
        });
        let (_controller, abort) = pi_agent::AbortController::new();
        let context = ToolCallContext {
            tool_call_id: "t".to_owned(),
            arguments: json!({ "command": "echo streaming-output" }),
            on_update,
            abort,
            model: None,
            approval_token: None,
        };
        (fixture.tool("bash").execute)(context).await.unwrap();
        let got = updates.lock();
        assert!(
            got.iter().any(|text| text.contains("streaming-output")),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn bash_preserves_stdout_stderr_write_order() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "printf 'OUT1\\n'; sleep 0.05; printf 'ERR1\\n' >&2; sleep 0.05; printf 'OUT2\\n'"
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&result), "OUT1\nERR1\nOUT2\n");
    }

    #[tokio::test]
    async fn bash_rejects_unsupervised_detach_without_rejecting_literal_ampersands() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        for command in [
            "nohup python3 -m http.server 8765 &",
            "python3 -m http.server 8765 &",
            "setsid python3 -m http.server 8765",
            "sleep 30; disown",
        ] {
            let error = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("background=true"), "{command}: {error}");
            assert!(error.contains("/ps"), "{command}: {error}");
        }
        for substitution in [
            "printf '%s' \"$(sleep 1 &)\"",
            "printf '%s' \"`sleep 1 &`\"",
        ] {
            let error = (tool.execute)(make_ctx(json!({ "command": substitution })))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("background=true"), "{substitution}: {error}");
            assert!(error.contains("/ps"), "{substitution}: {error}");
        }
        let result = (tool.execute)(make_ctx(json!({
            "command": "printf '%s\\n' 'quoted & text' escaped\\&value nohup setsid disown '$(sleep 1 &)' '$(nohup true)' '`sleep 1 &`'"
        })))
        .await
        .unwrap();
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
        assert_eq!(
            normalized,
            " python3 -m http.server 8765 >/tmp/http.log 2>&1 "
        );

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

    #[test]
    fn shell_detach_analysis_accepts_finite_foreground_constructs() {
        let foreground = [
            // Exact reported regression: a finite while loop and list
            // separators inside a quoted command substitution that the
            // enclosing foreground shell waits on.
            "printf \"%s\\n\" \"$(while false; do :; done; printf ok)\"",
            "printf '%s' \"$(for i in 1 2 3; do printf .; done)\"",
            "printf '%s' \"$(printf '&')\"",
            r"case $1 in a) echo first;;& b) echo second;; \*) echo star;; esac",
            // Ampersands that are shell text, not operators.
            "printf '%s\\n' 'q & t' \"d & d\" escaped\\&value '&&'",
            "echo $'it\\'s & done'",
            "echo héllo wörld '$(&)'",
            // Comments swallow trailing ampersands, including after
            // backslash-newline continuations.
            "echo alive # pretend & background",
            "echo alive \\\n# continuation & comment",
            // Foreground list operators.
            "echo one && echo two && echo three",
            "echo first || echo fallback; echo last",
            // Foreground finite loops, substitutions, groups, functions.
            "for i in 1 2 3; do echo $i; done",
            "i=0; while [ $i -lt 3 ]; do i=$((i+1)); done; echo ok",
            "(echo subshell; echo subshell)",
            "{ echo braces; echo braces; }",
            "f() { echo body; }; f",
            // Sibling substitutions are independent scopes.
            "printf '%s' \"$(true)$(true)\"",
            // Heredoc bodies are data: quoted delimiters never expand, and
            // unquoted ones only expose substitutions, never operators.
            "cat <<'EOF'\n& \nEOF",
            "cat <<EOF\n& $(true) \nEOF",
            // Redirection, arithmetic, and parameter grammars embed '&'
            // characters without any detachment.
            "echo done >/tmp/done.log 2>&1",
            "((2 & 3))",
            "echo $((2 & 3))",
            "echo $(( (1 & 0) + (2 & 4) ))",
            "v=x; echo ${v//x/&}",
        ];
        for command in foreground {
            let analysis = shell_detach_analysis(command);
            assert!(!analysis.has_detach_intent(), "false positive: {command}");
            assert!(
                !analysis.parse_failed && !analysis.unclassifiable,
                "command failed to classify: {command}"
            );
        }
    }

    #[test]
    fn shell_detach_analysis_rejects_every_real_detachment() {
        let detachment = [
            "sleep 30 &",
            "sleep 30 | cat &",
            "sleep 30 && echo done &",
            "echo start; sleep 30 &",
            "sleep 30 & echo after",
            "(sleep 30 &)",
            "while true; do sleep 30 & done",
            "f() { sleep 30 & }",
            // coprocesses run asynchronously by definition.
            "coproc sleep 30",
            "coproc NAME sleep 30",
            // Process substitutions create children bash does not wait for.
            "printf '%s\\n' <(sleep 30)",
            "true >(sleep 30)",
            "nohup python3 -m http.server 8765 &",
            "setsid python3 -m http.server 8765",
            "sleep 30; disown",
            "python3 -m http.server 8765 & disown",
            "env -i setsid sleep 30",
            // Background inside a substitution outlives the substitution's
            // own lifetime in bash, so it must fail closed at any depth.
            "printf '%s' \"$(sleep 1 &)\"",
            "printf '%s' \"`sleep 1 &`\"",
            "$( (sleep 30 &) )",
            "VAR=\"$(sleep 1 &)\"",
            // Each sibling substitution is its own scope.
            "printf '%s' \"$(true)$(nohup sleep 30)\"",
            "printf '%s' \"$(sleep 1 &)$(true)\"",
            "echo \"$(true)\" \"$(disown)\"",
            // Expanding heredoc bodies execute their substitutions live.
            "cat <<EOF\n$(sleep 30 &)\nEOF",
        ];
        for command in detachment {
            let analysis = shell_detach_analysis(command);
            assert!(analysis.has_detach_intent(), "missed detach: {command}");
        }
    }

    #[test]
    fn shell_detach_analysis_preserves_error_copy_contract() {
        // The historical substitution-specific error copy for detach
        // constructs inside substitutions (any depth, word or redirection).
        let substitution_error = shell_detach_error_string("printf '%s' \"$(sleep 1 &)\"");
        assert_eq!(
            substitution_error,
            "Cannot safely determine supervision for shell substitution containing background or control syntax. Move the long-lived command out of the substitution and run it with background=true so it remains visible in /ps."
        );
        assert_eq!(
            shell_detach_error_string("printf '%s' \"`sleep 1 &`\""),
            substitution_error
        );
        // Top-level constructs keep the generic copy byte-for-byte.
        assert_eq!(
            shell_detach_error_string("sleep 30 &"),
            "Unsupervised background or detached bash execution is not allowed (detected shell background '&'). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        );
        assert_eq!(
            shell_detach_error_string("sleep 30; disown"),
            "Unsupervised background or detached bash execution is not allowed (detected disown). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        );
        assert_eq!(
            shell_detach_error_string("coproc sleep 30"),
            "Unsupervised background or detached bash execution is not allowed (detected coproc). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        );
        assert_eq!(
            shell_detach_error_string("printf '%s\\n' <(sleep 30)"),
            "Unsupervised background or detached bash execution is not allowed (detected process substitution). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        );
    }
    fn shell_detach_error_string(command: &str) -> String {
        shell_detach_analysis(command)
            .classify_rejection()
            .expect("detachment expected")
            .to_string()
    }

    #[test]
    fn shell_detach_analysis_fails_closed_on_malformed_syntax() {
        // Commands the grammar cannot parse classify as malformed. They are
        // rejected up front when the raw text carries any detach marker, and
        // otherwise fall through to the governed runner's own parse-error
        // settlement, which spawns nothing.
        let malformed_benign = [
            "printf \"%s\\n\" \"$(while false; do :; done; printf ok\"",
            "echo $((2 &",
            "echo 'unterminated",
            "echo \"unterminated",
            "echo `unterminated backtick",
        ];
        for command in malformed_benign {
            let analysis = shell_detach_analysis(command);
            assert!(
                analysis.parse_failed || analysis.unclassifiable,
                "malformed command not flagged: {command}"
            );
            assert!(!analysis.has_detach_intent(), "invented detach: {command}");
        }
        // For unparseable input, detach intent is carried by the run path's
        // conservative marker routing: every detaching malformed form must
        // produce a pre-spawn rejection (and the benign forms must not).
        let malformed_detaching = [
            "echo \"$(sleep 1 &\"",
            "echo `sleep 1 &",
            "sleep 30 & echo \"unterminated",
        ];
        for command in malformed_detaching {
            let analysis = shell_detach_analysis(command);
            assert!(
                analysis.parse_failed || analysis.unclassifiable,
                "malformed command not flagged: {command}"
            );
            assert!(
                unclassified_supervision_error(command).is_some(),
                "malformed detaching command must fail closed: {command}"
            );
        }
    }

    #[test]
    fn shell_detach_analysis_boundaries_fail_closed() {
        // Past the substitution depth limit the walk stops classifying and
        // must reject rather than trust an unbounded nested parse.
        let nested = format!(
            "{}{}{}",
            "echo $(echo ".repeat((MAX_SUBSTITUTION_DEPTH + 2) as usize),
            "true",
            ")".repeat((MAX_SUBSTITUTION_DEPTH + 2) as usize),
        );
        let analysis = shell_detach_analysis(&nested);
        assert!(analysis.unclassifiable, "depth beyond bound must fail closed");
        let within = format!(
            "{}{}{}",
            "echo $(echo ".repeat((MAX_SUBSTITUTION_DEPTH - 1) as usize),
            "true",
            ")".repeat((MAX_SUBSTITUTION_DEPTH - 1) as usize),
        );
        let analysis = shell_detach_analysis(&within);
        assert!(
            !analysis.has_detach_intent() && !analysis.unclassifiable,
            "nested substitutions inside the bound must classify: {within}"
        );
    }

    #[tokio::test]
    async fn bash_runs_finite_substitution_with_foreground_control() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        let result = (tool.execute)(make_ctx(json!({
            "command": "printf \"%s\\n\" \"$(while false; do :; done; printf ok)\""
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&result), "ok\n");
    }

    #[tokio::test]
    async fn bash_malformed_syntax_fails_closed_by_category() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        // Unparseable syntax carrying a detach marker fails closed with the
        // historical category copies (an unclosed substitution containing
        // control syntax used the substitution copy even when it was benign);
        // marker-free unbalanced syntax settles through the runner's own
        // parse error, which spawns nothing.
        let substitution_error = (tool.execute)(make_ctx(json!({
            "command": "echo \"$(sleep 1 &\""
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            substitution_error,
            "Cannot safely determine supervision for shell substitution containing background or control syntax. Move the long-lived command out of the substitution and run it with background=true so it remains visible in /ps."
        );
        let background_marker_error = (tool.execute)(make_ctx(json!({
            "command": "sleep 30 & echo \"unterminated"
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            background_marker_error,
            "Unsupervised background or detached bash execution is not allowed (detected shell background '&'). Re-run the long-lived command with background=true so ProcessManager owns it and /ps reports its stable process id, logs, status, stop, and wait controls."
        );
        let shell_error = (tool.execute)(make_ctx(json!({
            "command": "echo 'unterminated"
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(
            shell_error.contains("unexpected EOF"),
            "marker-free unbalanced command must settle with a shell parse error: {shell_error}"
        );
    }

    #[tokio::test]
    async fn bash_background_uses_supervised_manager() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let manager = fixture.application.process_manager();
        let owner = fixture.application.session().process_owner_id();
        let tool = bash_tool_with_generation(
            &d.to_string_lossy(),
            None,
            Some(BashProcessContext {
                manager: manager.clone(),
                owner_id: owner.clone(),

                auth_manager: None,
            }),
            fixture.capture(),
        );
        let result = (tool.execute)(make_ctx(json!({
            "command": "printf background-ok",
            "background": true
        })))
        .await
        .expect("background start");
        let id: crate::ProcessId =
            serde_json::from_value(result.details["id"].clone()).expect("opaque process id");
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
    async fn bash_background_normalizes_legacy_nohup_and_terminal_ampersand() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let manager = fixture.application.process_manager();
        let owner = fixture.application.session().process_owner_id();
        let tool = bash_tool_with_generation(
            &d.to_string_lossy(),
            None,
            Some(BashProcessContext {
                manager: manager.clone(),
                owner_id: owner.clone(),
                auth_manager: None,
            }),
            fixture.capture(),
        );
        let result = (tool.execute)(make_ctx(json!({
            "command": "nohup printf legacy-normalized &",
            "background": true
        })))
        .await
        .expect("legacy detach syntax must reach normalization");
        let id: crate::ProcessId =
            serde_json::from_value(result.details["id"].clone()).expect("opaque process id");
        manager
            .wait(&owner, &id, Some(Duration::from_secs(3)))
            .await
            .expect("normalized process exit");
        let logs = manager
            .logs(&owner, &id, 0, None, false, None)
            .await
            .expect("normalized process logs");
        let output = logs
            .chunks
            .iter()
            .flat_map(crate::ProcessLogChunk::bytes)
            .collect::<Vec<_>>();
        assert_eq!(output, b"legacy-normalized");
    }

    #[tokio::test]
    async fn bash_parser_depth_preflight_rejects_decoy_fi_arguments() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        let depth = (MAX_PARSER_DELIMITER_DEPTH + 1) as usize;

        // Arguments that merely LOOK like reserved closers (`echo fi`) must
        // not pop structural nesting. Every level of this construction hides
        // its decoy BEFORE the next structural open, so a context-free closer
        // match would exhaust the stack and miss the bound, while the correct
        // grammar-position tracking still rejects before Brush parses
        // thousands of nested clauses.
        let mut deep_interleaved = String::from("true");
        for _ in 0..depth {
            deep_interleaved = format!("if true; then echo fi; {deep_interleaved}; fi");
        }
        let analysis = shell_detach_analysis(&deep_interleaved);
        assert!(
            analysis.unclassifiable,
            "recursively interleaved decoy 'echo fi' arguments beyond the structural bound must be rejected before parse"
        );
        let deep_error = (tool.execute)(make_ctx(json!({
            "command": deep_interleaved
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(deep_error, BAILOUT_SUPERVISION_ERROR);
    }

    #[tokio::test]
    async fn bash_parser_depth_preflight_track_braces_and_functions() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        let depth = (MAX_PARSER_DELIMITER_DEPTH + 1) as usize;

        // Nested `{ ... }` brace groups are structural tokens at command
        // position: beyond the bound they must bail out before parse.
        let mut deep_braces = String::from("true");
        for _ in 0..depth {
            deep_braces = format!("{{ echo fi; {deep_braces}; }}");
        }
        let analysis = shell_detach_analysis(&deep_braces);
        assert!(
            analysis.unclassifiable,
            "deeply nested brace groups must be rejected before parse"
        );

        // Nested function definitions with brace bodies (if Brush's grammar
        // permits deep function-body nesting): `name() { ... }` at command
        // position.
        let mut deep_functions = String::from("true");
        for _ in 0..depth {
            deep_functions = format!("f{}() {{ {deep_functions}; }}", 1);
        }
        let analysis = shell_detach_analysis(&deep_functions);
        assert!(
            analysis.unclassifiable,
            "deeply nested function brace bodies must be rejected before parse"
        );

        // Pipeline connector forms keep command mode across nesting.
        let mut deep_with_pipe_and_negation = String::from("true");
        for _ in 0..depth {
            deep_with_pipe_and_negation = format!(
                "true && if true; then ! echo fi | {deep_with_pipe_and_negation}; fi"
            );
        }
        let analysis = shell_detach_analysis(&deep_with_pipe_and_negation);
        assert!(
            analysis.unclassifiable,
            "deep nesting interleaved with pipeline connectors must be rejected before parse"
        );
        let pipe_error = (tool.execute)(make_ctx(json!({ "command": deep_with_pipe_and_negation })))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(pipe_error, BAILOUT_SUPERVISION_ERROR);

        // Connector and pipeline-prefix forms must preserve a fresh command
        // position for the following compound command.
        for command in [
            "true && if true; then true; fi",
            "true | if true; then true; fi",
            "true |& if true; then true; fi",
            "! if true; then true; fi",
            "time if true; then true; fi",
        ] {
            let analysis = shell_detach_analysis(command);
            assert!(
                !analysis.unclassifiable,
                "connector/pipeline-prefix command must classify: {command}"
            );
        }

        // 129 SEQUENTIAL (shallow) compounds of every tracked kind must not
        // trip the depth bound; breadth remains at zero structural nesting.
        // Chunks must stay `;`-separated: bare adjacency between compounds is
        // a bash syntax error, and a syntax error drops the analysis into the
        // parse-failure `&` marker check. The trailing `true` clears the `!`
        // negation of the final timed pipeline so the sequence exits 0.
        let shallow = "if true; then true; fi; ".repeat(129)
            + &"case x in x) true;; esac; ".repeat(129)
            + &"while false; do break; done; ".repeat(129)
            + &"{ true; }; ".repeat(129)
            + "time ! if true; then true; fi | true |& cat; true";
        let shallow_analysis = shell_detach_analysis(&shallow);
        assert!(
            !shallow_analysis.unclassifiable,
            "sequential shallow structures must not trip the depth bound"
        );
        (tool.execute)(make_ctx(json!({ "command": shallow })))
            .await
            .expect("shallow compound sequences must execute");
    }
    #[tokio::test]
    async fn bash_analysis_limits_reject_before_execution() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        // Brush tokenization removes comments and preserves quoted/heredoc
        // bodies as word data, so literal parentheses never consume the
        // structural pre-parse depth budget.
        let literal_parens = "(".repeat((MAX_PARSER_DELIMITER_DEPTH + 1) as usize);
        let literal_command = format!(
            "printf '%s\\n' '{0}' \"{0}\" $'{0}' >/dev/null; # {0}\ncat <<'EOF' >/dev/null\n{0}\nEOF\nprintf literal-parens-ok",
            literal_parens
        );
        let literal_analysis = shell_detach_analysis(&literal_command);
        assert!(
            !literal_analysis.unclassifiable,
            "quoted/comment/heredoc parentheses must not trip the parser depth bound"
        );
        let literal_result = (tool.execute)(make_ctx(json!({
            "command": literal_command
        })))
        .await
        .expect("literal parentheses must execute");
        assert_eq!(text_of(&literal_result), "literal-parens-ok");


        let oversized = "x".repeat(MAX_SUPERVISION_COMMAND_BYTES + 1);
        let oversized_error = (tool.execute)(make_ctx(json!({ "command": oversized })))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(oversized_error, BAILOUT_SUPERVISION_ERROR);

        let deep_substitution = format!(
            "{}{}{}",
            "echo $(echo ".repeat((MAX_SUBSTITUTION_DEPTH + 1) as usize),
            "true",
            ")".repeat((MAX_SUBSTITUTION_DEPTH + 1) as usize),
        );
        let substitution_error = (tool.execute)(make_ctx(json!({
            "command": deep_substitution
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(substitution_error, BAILOUT_SUPERVISION_ERROR);

        // Repeated `if` clauses are syntactically valid nested
        // CompoundCommand::IfClause nodes (unlike bare repeated parentheses,
        // which Brush may parse as arithmetic). Prove the analysis flag before
        // exercising the production runner's fixed bailout copy.
        let deep_compound = format!(
            "{}{}{}",
            "if true; then ".repeat((MAX_AST_DEPTH + 1) as usize),
            "true",
            "; fi".repeat((MAX_AST_DEPTH + 1) as usize),
        );
        let analysis = shell_detach_analysis(&deep_compound);
        assert!(
            analysis.unclassifiable,
            "nested compound AST beyond MAX_AST_DEPTH must be unclassifiable"
        );
        let compound_error = (tool.execute)(make_ctx(json!({
            "command": deep_compound
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(compound_error, BAILOUT_SUPERVISION_ERROR);
    }

    #[tokio::test]
    async fn bash_background_rejects_pre_aborted_call_without_spawning() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let manager = fixture.application.process_manager();
        let owner = fixture.application.session().process_owner_id();
        let tool = bash_tool_with_generation(
            &d.to_string_lossy(),
            None,
            Some(BashProcessContext {
                manager: manager.clone(),
                owner_id: owner.clone(),
                auth_manager: None,
            }),
            fixture.capture(),
        );
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let error = (tool.execute)(ToolCallContext {
            tool_call_id: "cancelled-background".to_owned(),
            arguments: json!({ "command": "sleep 30", "background": true }),
            on_update: noop_update(),
            abort,
            model: None,
            approval_token: None,
        })
        .await
        .expect_err("pre-aborted background command must fail");
        assert!(error.to_string().contains("aborted"));
        assert!(manager.list(&owner).is_empty());
    }

    // -----------------------------------------------------------------------
    // Embedded brush shell (default unsandboxed path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn bash_default_path_uses_embedded_brush() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": "printf 'shell=%s' \"${BASH_VERSION:-brush}\""
        })))
        .await
        .unwrap();
        assert_eq!(text_of(&res), "shell=brush");
    }

    #[tokio::test]
    async fn bash_brush_rebuilds_env_explicitly() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let session_env: SessionEnvFn =
            Arc::new(|| HashMap::from([("PI_MODEL".to_owned(), "session-value".to_owned())]));
        let tool = bash_tool_with_generation(
            &d.to_string_lossy(),
            Some(session_env),
            None,
            fixture.capture(),
        );
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
        let env: HashMap<String, String> =
            bash_command_env(Some(&session_env)).into_iter().collect();
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
    fn confined_safe_requested_env_drops_reserved_baseline_and_keeps_ordinary() {
        let env = vec![
            ("PATH".to_owned(), "/host/bin".to_owned()),
            ("HOME".to_owned(), "/host/home".to_owned()),
            ("TMPDIR".to_owned(), "/host/tmp".to_owned()),
            ("USER".to_owned(), "host".to_owned()),
            ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
            ("AWS_PAGER".to_owned(), String::new()),
            ("GH_PAGER".to_owned(), "cat".to_owned()),
            ("GH_PROMPT_DISABLED".to_owned(), "1".to_owned()),
            ("npm_config_yes".to_owned(), "true".to_owned()),
            ("PI_SESSION_ID".to_owned(), "sess".to_owned()),
            ("PAGER".to_owned(), "cat".to_owned()),
            ("CI".to_owned(), "1".to_owned()),
            ("ORDINARY_FG".to_owned(), "keep".to_owned()),
            ("BAD NAME".to_owned(), "x".to_owned()),
        ];
        let filtered = bash::foreground::confined_safe_requested_env(&env);
        for dropped in [
            "PATH",
            "HOME",
            "TMPDIR",
            "USER",
            "LANG",
            "AWS_PAGER",
            "GH_PAGER",
            "GH_PROMPT_DISABLED",
            "npm_config_yes",
            "PI_SESSION_ID",
            "BAD NAME",
        ] {
            assert!(
                !filtered.contains_key(std::ffi::OsStr::new(dropped)),
                "{dropped} must not be forwarded as a confined requested override"
            );
        }
        for kept in ["PAGER", "CI", "ORDINARY_FG"] {
            assert_eq!(
                filtered
                    .get(std::ffi::OsStr::new(kept))
                    .and_then(|value| value.as_ref()),
                Some(&std::ffi::OsString::from(match kept {
                    "PAGER" => "cat",
                    "CI" => "1",
                    _ => "keep",
                })),
                "{kept} must remain an ordinary confined request"
            );
        }
    }

    #[test]
    fn confined_pty_unattended_env_drops_reserved_and_baseline_keys() {
        let names: Vec<&str> = NON_INTERACTIVE_COMMAND_ENV
            .iter()
            .filter(|(key, _)| bash::foreground::is_confined_safe_pty_requested_name(key))
            .map(|(key, _)| *key)
            .collect();
        for dropped in [
            "TERM",
            "GH_PAGER",
            "AWS_PAGER",
            "GH_PROMPT_DISABLED",
            "npm_config_yes",
            "npm_config_fund",
        ] {
            assert!(
                !names.contains(&dropped),
                "{dropped} must not reach confined PTY requested_env"
            );
        }
        for kept in ["PAGER", "CI", "NO_COLOR", "GIT_EDITOR"] {
            assert!(
                names.contains(&kept),
                "{kept} must remain an ordinary confined PTY request"
            );
        }
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
        let env: HashMap<String, String> =
            bash_command_env(Some(&session_env)).into_iter().collect();
        assert_eq!(
            env.get("PI_MODEL").map(String::as_str),
            Some("session-model"),
            "the session value must win over any inherited host value"
        );
        for key in [
            "PI_SESSION_ID",
            "PI_SESSION_FILE",
            "PI_PROVIDER",
            "PI_REASONING_LEVEL",
        ] {
            assert!(
                !env.contains_key(key),
                "{key} must be scrubbed when the session provides no non-empty value"
            );
        }
    }

    #[tokio::test]
    async fn bash_brush_rejects_host_dangerous_builtins() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        for command in ["exec sleep 0.1", "suspend", "ulimit -n", "umask 077"] {
            let error = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("not supported in the embedded brush shell"),
                "{command}: {error}"
            );
        }
        let error = (tool.execute)(make_ctx(json!({ "command": "kill -9 $$" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refusing to signal the host process"),
            "{error}"
        );
        let result = (tool.execute)(make_ctx(json!({ "command": "kill -l" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("SIGTERM"));
        let result = (tool.execute)(make_ctx(json!({ "command": "(exec echo subshell-ok)" })))
            .await
            .unwrap();
        assert_eq!(text_of(&result), "subshell-ok\n");
    }

    #[tokio::test]
    async fn bash_brush_kill_guard_checks_every_numeric_target() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("bash");
        for command in [
            "kill 1234 $$",
            "kill -9 1234 $$",
            "kill $$ 1234",
            "kill 1234 5678 $$",
        ] {
            let error = (tool.execute)(make_ctx(json!({ "command": command })))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("refusing to signal the host process"),
                "{command}: {error}"
            );
        }
        let error = (tool.execute)(make_ctx(json!({ "command": "kill 1234 5678" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("refusing to signal the host process"),
            "{error}"
        );
        let result = (tool.execute)(make_ctx(json!({ "command": "kill -l" })))
            .await
            .unwrap();
        assert!(text_of(&result).contains("SIGTERM"));
    }

    #[tokio::test]
    async fn bash_brush_parse_failure_runs_governed_foreground_fallback() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({ "command": "echo )" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("syntax error"), "{error}");
        assert!(
            !error.contains("requires governed subprocess execution"),
            "{error}"
        );
    }

    fn unattended_bash_env() -> Vec<(String, String)> {
        let mut env = bash_command_env(None);
        for (key, value) in NON_INTERACTIVE_COMMAND_ENV {
            env.push(((*key).to_owned(), (*value).to_owned()));
        }
        env
    }

    #[cfg(unix)]
    async fn run_foreground_sleep(
        fixture: &ToolGenerationFixture,
        cwd: &Path,
        timeout: Option<Duration>,
        abort: pi_agent::AbortSignal,
    ) -> anyhow::Result<bash::foreground::ForegroundRunOutput> {
        let generation = fixture.generation();
        let (shell, shell_args) = get_shell_config(&generation).expect("trusted bash");
        let mut argv = Vec::with_capacity(shell_args.len() + 2);
        argv.push(shell);
        argv.extend(shell_args);
        argv.push("sleep 30".to_owned());
        bash::foreground::run_confined_command(
            generation,
            None,
            cwd,
            &argv,
            &unattended_bash_env(),
            timeout,
            abort,
            Arc::new(|_data: &[u8]| {}),
        )
        .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_timeout_returns_timed_out_without_reader_io_error() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let started = Instant::now();
        let (_controller, abort) = pi_agent::AbortController::new();
        let outcome = run_foreground_sleep(&fixture, &d, Some(Duration::from_millis(400)), abort)
            .await
            .expect("timed-out foreground must not become a reader I/O error");
        assert!(
            outcome.timed_out && !outcome.cancelled && outcome.exit_code.is_none(),
            "timeout must remain the primary outcome: timed_out={} cancelled={} exit={:?}",
            outcome.timed_out,
            outcome.cancelled,
            outcome.exit_code
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout cleanup must bound child wait and reader drain: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_cancel_returns_cancelled_without_reader_io_error() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let started = Instant::now();
        let (controller, abort) = pi_agent::AbortController::new();
        let run = run_foreground_sleep(&fixture, &d, Some(Duration::from_secs(10)), abort);
        let aborting = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            controller.abort();
        };
        let (outcome, ()) = tokio::join!(run, aborting);
        let outcome = outcome.expect("cancelled foreground must not become a reader I/O error");
        assert!(
            outcome.cancelled && !outcome.timed_out && outcome.exit_code.is_none(),
            "cancel must remain the primary outcome: timed_out={} cancelled={} exit={:?}",
            outcome.timed_out,
            outcome.cancelled,
            outcome.exit_code
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel cleanup must bound child wait and reader drain: {:?}",
            started.elapsed()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_brush_timeout_reaps_descendants() {
        let d = tmpdir();
        let fixture = tool_generation_fixture(&d).await;
        let error = (fixture.tool("bash").execute)(make_ctx(json!({
            "command": tracked_sleep_command("child"),
            "timeout": 0.5
        })))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");
        wait_for_file(&d.join("child.pid")).await;
        let child = read_recorded_pid(&d.join("child.pid"));
        let start_time = read_recorded_start_time(&d.join("child.start"));
        wait_until_identity_gone(child, start_time).await;
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
        let (_application, generation) = pdf_test_generation(&d).await;
        let tool = captured_bash_tool(&d, generation);
        // 60000 bytes > 50 KiB display cap → spills to a temp file; exit 0.
        let res = (tool.execute)(make_ctx(
            json!({ "command": "yes x | head -c 60000", "timeout": 10 }),
        ))
        .await
        .unwrap();
        let text = text_of(&res);
        let path = extract_full_output_path(&text);
        assert!(!path.is_empty(), "expected Full output path in: {text}");
        assert_eq!(
            bash::read_full_output_path(&path, 64 * 1024)
                .expect("published spill must remain registered")
                .expect("read registered spill")
                .len(),
            60_000,
            "published spill must retain the complete output"
        );
        cleanup_full_output_path(&path);
        assert!(
            bash::read_full_output_path(&path, 64 * 1024).is_none(),
            "cleanup must release the registered retained spill"
        );
        cleanup_full_output_path(&path);
    }

    #[tokio::test]
    async fn bash_full_output_spill_removed_on_nonzero_exit() {
        let d = tmpdir();
        let (_application, generation) = pdf_test_generation(&d).await;
        let tool = captured_bash_tool(&d, generation);
        // Big output (spill) + nonzero exit: run_bash must clean up the spill
        // and must NOT publish a (dead) Full output path in the error text.
        // (Owned-file cleanup is covered race-free by bash.rs Drop/take unit
        // tests; this test asserts the no-dead-path error contract.)
        let err = (tool.execute)(make_ctx(
            json!({ "command": "yes x | head -c 60000; exit 7", "timeout": 10 }),
        ))
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
        let (_application, generation) = pdf_test_generation(&d).await;
        let tool = captured_bash_tool(&d, generation);
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

    #[tokio::test]
    async fn ls_lists_entries_with_dir_suffix() {
        let d = tmpdir();
        fs::create_dir_all(d.join("subdir")).unwrap();
        fs::write(d.join("file.txt"), b"x").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("ls").execute)(make_ctx(json!({})))
            .await
            .unwrap();
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
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let res = (fixture.tool("find").execute)(make_ctx(json!({ "pattern": "*.ts" })))
            .await
            .unwrap();
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
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("glob");
        assert_eq!(tool.name, "glob");
        let res = (tool.execute)(make_ctx(json!({ "pattern": "*.ts" })))
            .await
            .unwrap();
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
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("glob");

        let default = (tool.execute)(make_ctx(json!({ "pattern": "*.rs" })))
            .await
            .unwrap();
        let default_text = text_of(&default);
        assert!(default_text.contains("visible.rs"), "got: {default_text}");
        assert!(
            !default_text.contains(".secret.rs"),
            "hidden leaked: {default_text}"
        );
        assert!(
            !default_text.contains("ignored.rs"),
            "gitignored leaked: {default_text}"
        );

        let hidden = (tool.execute)(make_ctx(json!({ "pattern": "*.rs", "hidden": true })))
            .await
            .unwrap();
        assert!(
            text_of(&hidden).contains(".secret.rs"),
            "hidden=true should include: {}",
            text_of(&hidden)
        );

        let ignored = (tool.execute)(make_ctx(json!({ "pattern": "*.rs", "gitignore": false })))
            .await
            .unwrap();
        let ignored_text = text_of(&ignored);
        assert!(
            ignored_text.contains("visible.rs"),
            "gitignore=false still returns ordinary matches: {ignored_text}"
        );
        assert!(
            ignored_text.contains("ignored.rs"),
            "gitignore=false must reveal gitignored files: {ignored_text}"
        );
        assert!(
            !ignored_text.contains(".secret.rs"),
            "hidden entries stay skipped without hidden=true: {ignored_text}"
        );
    }

    #[tokio::test]
    async fn glob_tool_clamps_limit() {
        let d = tmpdir();
        for i in 0..5 {
            fs::write(d.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let tool = fixture.tool("glob");

        let limited = (tool.execute)(make_ctx(json!({ "pattern": "*.txt", "limit": 2 })))
            .await
            .unwrap();
        let count = text_of(&limited)
            .lines()
            .filter(|line| line.ends_with(".txt"))
            .count();
        assert_eq!(count, 2, "{}", text_of(&limited));

        let high = (tool.execute)(make_ctx(json!({ "pattern": "*.txt", "limit": 9999 })))
            .await
            .unwrap();
        let count = text_of(&high)
            .lines()
            .filter(|line| line.ends_with(".txt"))
            .count();
        assert_eq!(count, 5, "{}", text_of(&high));
    }

    #[tokio::test]
    async fn find_limit_zero_uses_effective_generation_bound() {
        let d = tmpdir();
        for name in ["a.go", "b.go", "c.go"] {
            fs::write(d.join(name), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("find").execute)(make_ctx(json!({
            "pattern": "*.go",
            "limit": 0
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert_eq!(
            text.lines().filter(|line| line.ends_with(".go")).count(),
            1,
            "limit=0 clamps to the minimum positive bound: {text}"
        );
    }

    #[tokio::test]
    async fn find_limit_negative_uses_effective_generation_bound() {
        let d = tmpdir();
        for name in ["a.go", "b.go"] {
            fs::write(d.join(name), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("find").execute)(make_ctx(json!({
            "pattern": "*.go",
            "limit": -1
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert_eq!(
            text.lines().filter(|line| line.ends_with(".go")).count(),
            1,
            "negative limits clamp to the minimum positive bound: {text}"
        );
    }

    #[tokio::test]
    async fn grep_matches_pattern_with_line_numbers() {
        let d = tmpdir();
        fs::write(d.join("g.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "beta" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("g.txt:2: beta"), "got: {text}");
        // Default context is zero: the match row must be the only row.
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            ["g.txt:2: beta"],
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let d = tmpdir();
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        fs::write(d.join(".gitignore"), b"ignoreme.txt\n").unwrap();
        fs::write(d.join("ignoreme.txt"), b"secret\n").unwrap();
        fs::write(d.join("keep.txt"), b"secret\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "secret" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("keep.txt"), "got: {text}");
        assert!(
            !text.contains("ignoreme.txt"),
            "gitignored file leaked: {text}"
        );
    }

    fn grep_schema_fields() -> Vec<&'static str> {
        vec![
            "pattern",
            "path",
            "glob",
            "ignoreCase",
            "literal",
            "context",
            "limit",
        ]
    }

    fn assert_grep_schema(tool: &AgentTool) {
        let schema = serde_json::to_value(&tool.parameters).expect("grep schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"].as_object().expect("grep properties");
        let names: Vec<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(names.len(), grep_schema_fields().len(), "{names:?}");
        // serde_json::to_value sorts map keys, so declaration-order
        // preservation must be asserted on the serialized wire text (stable
        // provider prompt caches); see s_object's property_order contract.
        let wire = serde_json::to_string(&tool.parameters).expect("grep wire");
        let mut prev = 0usize;
        for field in grep_schema_fields() {
            let key = format!("\"{field}\":");
            let at = wire
                .find(&key)
                .unwrap_or_else(|| panic!("{key} missing in {wire}"));
            assert!(at >= prev, "grep field {field} out of order in {wire}");
            prev = at;
        }
        assert_eq!(schema["required"], json!(["pattern"]));
        let path = properties["path"]["description"]
            .as_str()
            .expect("path description");
        assert!(
            path.contains("Workspace-authorized") && path.contains("Relative paths resolve from cwd"),
            "{path}"
        );
        let valid = json!({
            "pattern": "function _materializeBucket",
            "path": "src",
            "glob": "Bridge.sol",
            "context": 8,
            "limit": 20
        });
        tool.parameters.validate(&valid).expect("current Grep args");
        let extra = json!({
            "pattern": "needle",
            "unknownField": true
        });
        let error = tool
            .parameters
            .validate(&extra)
            .expect_err("unknown fields")
            .to_string();
        assert!(
            error.contains("additional property not allowed"),
            "{error}"
        );
    }

    #[test]
    fn s_object_preserves_declaration_order_and_first_duplicate() {
        let schema = s_object(
            vec![
                ("zeta", s_string("z")),
                ("alpha", s_string("a")),
                ("zeta", s_number("later duplicate lost")),
                ("mid", s_boolean("m")),
            ],
            vec!["alpha"],
        );
        assert_eq!(
            schema.property_order,
            ["zeta", "alpha", "mid"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(schema.properties.len(), schema.property_order.len());
        let wire = serde_json::to_string(&schema).expect("wire schema");
        let mut prev = 0usize;
        for field in ["zeta", "alpha", "mid"] {
            let key = format!("\"{field}\":");
            let at = wire
                .find(&key)
                .unwrap_or_else(|| panic!("{key} missing in {wire}"));
            assert!(at >= prev, "field {field} out of order in {wire}");
            prev = at;
        }
        let value = serde_json::to_value(&schema).expect("wire value");
        // First declaration wins for a duplicate name.
        assert_eq!(value["properties"]["zeta"]["type"], "string");
        assert_eq!(value["properties"].as_object().expect("props").len(), 3);
        assert_eq!(value["required"], json!(["alpha"]));
    }

    #[test]
    fn grep_definitions_share_strict_current_schema() {
        let standalone = grep_tool("/tmp");
        let generation_bound = grep_tool_with_generation(
            crate::WorkspaceRoots::for_tool_factory("/tmp"),
            Arc::new(|| Err(anyhow!("unused"))),
        );
        assert_grep_schema(&standalone);
        assert_grep_schema(&generation_bound);
        assert_eq!(
            serde_json::to_value(&standalone.parameters).expect("standalone"),
            serde_json::to_value(&generation_bound.parameters).expect("generation")
        );
    }

    fn write_bridge_src(root: &Path) {
        let src = root.join("src");
        fs::create_dir_all(&src).expect("src");
        fs::write(
            src.join("Bridge.sol"),
            "contract Bridge {\n    // setup\n    function _materializeBucket() internal {\n        uint256 x = 1;\n    }\n}\n",
        )
        .expect("Bridge.sol");
        fs::write(src.join("Other.sol"), "function _materializeBucket() {}\n").expect("Other.sol");
    }

    fn assert_path_outside_workspace(error: &str, supplied: &Path, undisclosed: &[&Path]) {
        assert!(
            error.starts_with("path_outside_workspace:"),
            "{error}"
        );
        assert!(
            error.contains("--add-dir <project-root>"),
            "{error}"
        );
        assert!(
            error.contains("repo-relative path inside the current workspace/additional roots"),
            "{error}"
        );
        assert!(
            !error.contains("path resolved to no target"),
            "{error}"
        );
        if supplied.is_absolute() {
            assert!(
                error.contains("<absolute path>"),
                "rejected absolute path is redacted: {error}"
            );
            let supplied = supplied.to_string_lossy();
            assert!(
                !error.contains(supplied.as_ref()),
                "rejected absolute supplied path must not appear: {error}"
            );
        } else {
            let supplied = supplied.to_string_lossy();
            assert!(
                error.contains(supplied.as_ref()),
                "relative supplied path is user-visible bounded context: {error}"
            );
        }
        for root in undisclosed {
            assert!(
                !error.contains(&root.display().to_string()),
                "must not disclose workspace configuration {root:?}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn grep_directory_glob_on_authorized_additional_root_succeeds() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        write_bridge_src(additional.path());
        let workspace =
            crate::WorkspaceRoots::new(cwd.path(), [additional.path()]).expect("workspace");
        let fixture = tool_generation_fixture_for_workspace(workspace).await;
        let src = additional.path().join("src");
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "function _materializeBucket",
            "path": src,
            "glob": "Bridge.sol",
            "context": 8,
            "limit": 20
        })))
        .await
        .expect("authorized additional-root grep");
        let text = text_of(&result);
        assert!(
            text.contains("Bridge.sol:3:     function _materializeBucket() internal {"),
            "got: {text}"
        );
        assert!(
            text.contains("Bridge.sol-2-     // setup"),
            "context rows missing: {text}"
        );
        assert!(
            !text.contains("Other.sol"),
            "basename glob leaked Other.sol: {text}"
        );
        assert!(
            !text.contains("path resolved to no target"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn grep_directory_glob_outside_workspace_returns_scope_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        let external = tempfile::tempdir().expect("external");
        write_bridge_src(external.path());
        let workspace =
            crate::WorkspaceRoots::new(cwd.path(), [additional.path()]).expect("workspace");
        let fixture = tool_generation_fixture_for_workspace(workspace).await;
        let src = external.path().join("src");
        let error = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "function _materializeBucket",
            "path": src,
            "glob": "Bridge.sol",
            "context": 8,
            "limit": 20
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_path_outside_workspace(
            &error,
            &src,
            &[cwd.path(), additional.path()],
        );
    }

    #[tokio::test]
    async fn glob_outside_workspace_returns_scope_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        let external = tempfile::tempdir().expect("external");
        write_bridge_src(external.path());
        let workspace =
            crate::WorkspaceRoots::new(cwd.path(), [additional.path()]).expect("workspace");
        let fixture = tool_generation_fixture_for_workspace(workspace).await;
        let src = external.path().join("src");
        let error = (fixture.tool("glob").execute)(make_ctx(json!({
            "pattern": "Bridge.sol",
            "path": src
        })))
        .await
        .unwrap_err()
        .to_string();
        assert_path_outside_workspace(
            &error,
            &src,
            &[cwd.path(), additional.path()],
        );
    }

    // -----------------------------------------------------------------------
    // Abort propagation for ls / find / glob / grep
    // -----------------------------------------------------------------------

    /// The generation-bound tool closures check the abort signal in their own
    /// bodies (not just via the harness wrapper): a pre-aborted call must fail
    /// with the canonical "Operation aborted" error.
    #[tokio::test]
    async fn glob_find_grep_ls_check_abort_in_tool_body() {
        let d = tmpdir();
        for i in 0..8 {
            fs::write(d.join(format!("f{i}.rs")), b"needle\n").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        for (tool, args) in [
            (
                glob_tool_with_generation(fixture.workspace.clone(), fixture.capture()),
                json!({ "pattern": "*.rs" }),
            ),
            (
                find_tool_with_generation(fixture.workspace.clone(), fixture.capture()),
                json!({ "pattern": "*.rs" }),
            ),
            (
                grep_tool_with_generation(fixture.workspace.clone(), fixture.capture()),
                json!({ "pattern": "needle" }),
            ),
            (
                ls_tool_with_generation(fixture.workspace.clone(), fixture.capture()),
                json!({}),
            ),
        ] {
            let context = ToolCallContext {
                tool_call_id: "cancelled".to_owned(),
                arguments: args,
                on_update: noop_update(),
                abort: abort.clone(),
                model: None,
                approval_token: None,
            };
            let error = (tool.execute)(context).await.unwrap_err().to_string();
            assert_eq!(error, "Operation aborted", "{}", tool.name);
        }
    }

    fn real_text_file_beyond_64mib(path: &Path, suffix: &str) {
        let mut file = fs::File::create(path).expect("large text file");
        use std::io::Write;
        let row = [b'x'; 4095];
        let mut written = 0u64;
        while written <= 64 * 1024 * 1024 {
            file.write_all(&row).expect("write filler");
            file.write_all(b"\n").expect("write newline");
            written += 4096;
        }
        file.write_all(suffix.as_bytes()).expect("write suffix");
    }

    #[tokio::test]
    async fn grep_directory_skips_oversized_file_and_returns_other_matches() {
        let d = tmpdir();
        fs::write(d.join("early.txt"), b"needle early\n").unwrap();
        real_text_file_beyond_64mib(&d.join("huge.txt"), "needle huge\n");
        fs::write(d.join("later.txt"), b"needle later\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle" })))
            .await
            .expect("grep oversized directory");
        let text = text_of(&result);
        assert!(text.contains("early.txt:1: needle early"), "got: {text}");
        assert!(text.contains("later.txt:1: needle later"), "got: {text}");
        assert!(
            text.contains("huge.txt:"),
            "oversized text file match missing: {text}"
        );
        assert!(
            !text.contains("67108864"),
            "directory grep must not abort on oversized file: {text}"
        );
    }

    #[tokio::test]
    async fn grep_finds_match_beyond_64mib_with_bounded_line_buffer() {
        let d = tmpdir();
        real_text_file_beyond_64mib(&d.join("tail.txt"), "unique-after-cap\n");
        let fixture = tool_generation_fixture(&d).await;
        let result =
            (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "unique-after-cap" })))
                .await
                .expect("grep large text");
        let text = text_of(&result);
        assert!(text.contains("tail.txt:"), "got: {text}");
        assert!(text.contains("unique-after-cap"), "got: {text}");
        assert!(!text.contains("67108864"), "got: {text}");
    }

    #[tokio::test]
    async fn grep_skips_oversized_binary_file_in_directory() {
        let d = tmpdir();
        fs::write(d.join("keep.txt"), b"needle keep\n").unwrap();
        let file = fs::File::create(d.join("blob.bin")).unwrap();
        file.set_len(64 * 1024 * 1024).unwrap();
        drop(file);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(d.join("blob.bin"))
            .unwrap();
        use std::io::Write;
        file.write_all(&[0, 1, 2, 3, 0]).unwrap();
        fs::write(d.join("after.txt"), b"needle after\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle" })))
            .await
            .expect("grep binary oversized");
        let text = text_of(&result);
        assert!(text.contains("keep.txt:1: needle keep"), "got: {text}");
        assert!(text.contains("after.txt:1: needle after"), "got: {text}");
        assert!(
            !text.contains("blob.bin"),
            "binary oversized file leaked: {text}"
        );
        assert!(!text.contains("67108864"), "got: {text}");
    }

    #[tokio::test]
    async fn grep_consecutive_matches_with_context_and_limit() {
        let d = tmpdir();
        fs::write(
            d.join("g.txt"),
            b"needle one\nneedle two\nneedle three\nplain one\nplain two\nneedle ignored\n",
        )
        .unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "needle",
            "context": 2,
            "limit": 3
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        let rows: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("g.txt"))
            .collect();
        assert_eq!(
            rows,
            vec![
                "g.txt:1: needle one",
                "g.txt-2- needle two",
                "g.txt-3- needle three",
                "g.txt-1- needle one",
                "g.txt:2: needle two",
                "g.txt-3- needle three",
                "g.txt-4- plain one",
                "g.txt-1- needle one",
                "g.txt-2- needle two",
                "g.txt:3: needle three",
                "g.txt-4- plain one",
                "g.txt-5- plain two",
            ],
            "each consecutive match must retain its complete ordered context block: {text}"
        );
        assert!(
            !text.contains("needle ignored"),
            "matches beyond the limit must not be emitted: {text}"
        );
    }

    #[test]
    fn grep_overlong_line_with_buffered_terminator_preserves_next_line() {
        let mut bytes = vec![b'x'; GREP_MAX_RAW_LINE + 1];
        bytes.extend_from_slice(b"\nneedle next\n");
        let mut reader = BufReader::with_capacity(bytes.len(), bytes.as_slice());
        let (_, abort) = pi_agent::AbortController::new();
        let regex = regex::Regex::new("needle").expect("valid needle regex");
        let mut output = GrepOutput::default();
        let mut matched = 0usize;
        let mut lines_truncated = false;
        let mut output_truncated = false;
        scan_grep_lines(
            &mut reader,
            "big.txt",
            &regex,
            0,
            100,
            &abort,
            &mut output,
            &mut matched,
            &mut lines_truncated,
            &mut output_truncated,
        )
        .expect("over-long line scan");
        assert_eq!(
            matched, 1,
            "next line was lost after over-long terminator: {output:?}"
        );
        assert_eq!(&*output, &["big.txt:2: needle next".to_owned()]);
    }

    #[test]
    fn grep_oversized_line_inside_context_preserves_other_rows() {
        let mut bytes = b"before\nneedle match\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', GREP_MAX_RAW_LINE + 1));
        bytes.extend_from_slice(b"\nafter\n");
        let mut reader = BufReader::with_capacity(64 * 1024, bytes.as_slice());
        let (_, abort) = pi_agent::AbortController::new();
        let regex = regex::Regex::new("needle").expect("valid needle regex");
        let mut output = GrepOutput::default();
        let mut matched = 0usize;
        let mut lines_truncated = false;
        let mut output_truncated = false;
        scan_grep_lines(
            &mut reader,
            "big.txt",
            &regex,
            2,
            100,
            &abort,
            &mut output,
            &mut matched,
            &mut lines_truncated,
            &mut output_truncated,
        )
        .expect("context scan");

        assert_eq!(matched, 1);
        assert_eq!(
            &*output,
            &[
                "big.txt-1- before".to_owned(),
                "big.txt:2: needle match".to_owned(),
                "big.txt-4- after".to_owned(),
            ],
            "oversized context line must be omitted without erasing neighboring context"
        );
    }
    #[tokio::test]
    async fn grep_arbitrary_context_uses_byte_budget_without_public_clamp() {
        let d = tmpdir();
        let content = (0..40_000)
            .map(|index| {
                if index == 39_999 {
                    "needle".to_owned()
                } else {
                    format!("context {index:05} {}", "x".repeat(80))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(d.join("large-context.txt"), content).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "needle",
            "context": 1_000_000,
            "limit": 1,
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("large-context.txt:40000: needle"),
            "match row missing: {text}"
        );
        assert!(
            text.contains("50.0KB limit reached"),
            "byte-budget notice missing: {text}"
        );
        assert_eq!(result.details["truncation"]["truncated"], json!(true));
        assert_eq!(result.details["outputTruncated"], json!(true));
        assert!(
            text.len() < 55 * 1024,
            "arbitrary public context must stay byte-bounded"
        );
    }

    #[tokio::test]
    async fn grep_preceding_context_truncation_sets_notice_and_details() {
        let d = tmpdir();
        fs::write(
            d.join("preceding.txt"),
            format!("{}\nneedle\n", "x".repeat(600)),
        )
        .unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result =
            (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle", "context": 1 })))
                .await
                .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Some lines truncated to 500 chars"),
            "preceding truncation notice missing: {text}"
        );
        assert_eq!(result.details["linesTruncated"], json!(true));
    }

    #[tokio::test]
    async fn grep_binary_probe_skips_ascii_prefixed_binary_file() {
        let d = tmpdir();
        fs::write(
            d.join("binary.dat"),
            b"header\0needle hidden\nneedle later\n",
        )
        .unwrap();
        fs::write(d.join("text.txt"), b"needle visible\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("text.txt:1: needle visible"),
            "text match missing: {text}"
        );
        assert!(
            !text.contains("binary.dat"),
            "ASCII-prefixed binary file leaked rows: {text}"
        );
    }
    #[tokio::test]
    async fn grep_standalone_cr_and_crlf_line_numbers() {
        let d = tmpdir();
        fs::write(d.join("cr.txt"), b"first\rsecond\rneedle cr\n").unwrap();
        fs::write(d.join("crlf.txt"), b"first\r\nneedle crlf\r\ntail\r\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("cr.txt:3: needle cr"),
            "standalone CR must split lines: {text}"
        );
        assert!(
            text.contains("crlf.txt:2: needle crlf"),
            "CRLF must count as one break: {text}"
        );
    }

    #[tokio::test]
    async fn grep_late_nul_after_binary_probe_keeps_legacy_text_scan() {
        let d = tmpdir();
        let mut bytes = vec![b'x'; 8192];
        bytes.extend_from_slice(b"\nneedle\0visible\n");
        fs::write(d.join("late-nul.txt"), bytes).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("late-nul.txt:2: needle"),
            "late NUL changed legacy text scan: {text}"
        );
    }

    /// Arms the AbortController on the second fill_buf call: the scan's
    /// first buffer holds an over-long line, and the drain pass must observe
    /// the abort instead of reading to EOF.
    struct DrainAbortReader {
        data: Vec<u8>,
        position: usize,
        controller: pi_agent::AbortController,
        fills: usize,
    }

    impl Read for DrainAbortReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            unreachable!("fill_buf is used exclusively")
        }
    }
    impl BufRead for DrainAbortReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.fills += 1;
            if self.fills >= 2 {
                self.controller.abort();
            }
            static EMPTY: &[u8] = &[];
            Ok(self.data.get(self.position..).unwrap_or(EMPTY))
        }
        fn consume(&mut self, amt: usize) {
            self.position += amt;
        }
    }

    #[test]
    fn grep_abort_during_overlong_line_drain() {
        let (controller, abort) = pi_agent::AbortController::new();
        let mut data = vec![b'x'; GREP_MAX_RAW_LINE + 4096];
        data.extend_from_slice(b"tail-without-newline");
        let mut reader = DrainAbortReader {
            data,
            position: 0,
            controller,
            fills: 0,
        };
        let mut output = GrepOutput::default();
        let mut matched = 0usize;
        let mut lines_truncated = false;
        let mut output_truncated = false;
        let regex = regex::Regex::new("needle").expect("valid needle regex");
        scan_grep_lines(
            &mut Box::new(reader) as &mut dyn BufRead,
            "big.txt",
            &regex,
            0,
            100,
            &abort,
            &mut output,
            &mut matched,
            &mut lines_truncated,
            &mut output_truncated,
        )
        .expect("drain settles");
        assert_eq!(matched, 0, "over-long drain must not emit rows: {output:?}");
        let error = check_aborted(&abort).unwrap_err().to_string();
        assert_eq!(error, "Operation aborted");
    }

    #[tokio::test]
    async fn grep_finds_match_beyond_64mib_offset_in_real_text() {
        let d = tmpdir();
        let path = d.join("real-big.txt");
        let mut file = fs::File::create(&path).unwrap();
        use std::io::{Seek, Write};
        let line = format!("{} \n", "x".repeat(96));
        let mut written = 0u64;
        let mut index = 0u64;
        while written <= 64 * 1024 * 1024 {
            let row = format!("{index} filler line beyond the old cap\n");
            file.write_all(row.as_bytes()).unwrap();
            written += row.len() as u64;
            index += 1;
        }
        file.write_all(b"needle deep beyond cap\n").unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        drop(file);
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({ "pattern": "needle deep" })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("real-big.txt:") && text.contains("needle deep beyond cap"),
            "multi-line real text past 64 MiB must match: {text}"
        );
        assert!(!text.contains("67108864"), "got: {text}");
    }

    /// grep's per-file scan stops on abort without emitting a partial result:
    /// an aborted signal observed at scan time must yield no rows at all.
    #[tokio::test]
    async fn grep_entry_scan_stops_immediately_on_abort() {
        let d = tmpdir();
        fs::write(d.join("a.txt"), b"needle\nneedle\nneedle\n").unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let generation = fixture.generation();
        let fs = captured_policy_fs(&generation).expect("policy fs");
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let mut ignores = IgnoreStack::new(generation, &d.to_string_lossy(), true, false)
            .await
            .expect("ignore stack");
        let mut output = GrepOutput::default();
        let mut matched = 0usize;
        let mut lines_truncated = false;
        let mut output_truncated = false;
        let regex = regex::Regex::new("needle").expect("regex");
        grep_policy_entry(
            &fs,
            &mut ignores,
            &d,
            Path::new(""),
            &regex,
            "",
            0,
            100,
            &abort,
            &mut output,
            &mut matched,
            &mut lines_truncated,
            &mut output_truncated,
        )
        .expect("entry scan");
        assert!(output.is_empty(), "aborted grep must not scan: {output:?}");
        assert_eq!(matched, 0);
    }

    /// Barrier-driven abort seam: a BufRead whose second chunk arms the
    /// AbortController before yielding. The scan must stop on its per-line
    /// abort check with no output, and the post-scan `check_aborted` gate
    /// used by run_grep_captured must surface "Operation aborted". No sleeps.
    struct AbortArmingReader {
        lines: Vec<Vec<u8>>,
        position: usize,
        controller: pi_agent::AbortController,
    }
    impl Read for AbortArmingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            unreachable!("fill_buf is used exclusively")
        }
    }
    impl BufRead for AbortArmingReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            static EMPTY: &[u8] = &[];
            Ok(self
                .lines
                .get(self.position)
                .map(Vec::as_slice)
                .unwrap_or(EMPTY))
        }
        fn consume(&mut self, amt: usize) {
            if let Some(front) = self.lines.get(self.position) {
                if amt >= front.len() {
                    if self.position == 0 {
                        // Arm the barrier exactly between line 1 and line 2:
                        // the scan must observe it at its next per-line gate.
                        self.controller.abort();
                    }
                    self.position += 1;
                } else {
                    self.lines[self.position].drain(..amt);
                }
            }
        }
    }

    #[tokio::test]
    async fn grep_mid_stream_abort_settles_as_abort() {
        let (controller, abort) = pi_agent::AbortController::new();
        let reader = AbortArmingReader {
            lines: vec![b"alpha\n".to_vec(), b"beta\n".to_vec(), b"gamma\n".to_vec()],
            position: 0,
            controller,
        };
        let regex = regex::Regex::new("beta").expect("regex");
        let mut output = GrepOutput::default();
        let mut matched = 0usize;
        let mut lines_truncated = false;
        let mut output_truncated = false;
        scan_grep_lines(
            &mut Box::new(reader) as &mut dyn BufRead,
            "stream.txt",
            &regex,
            0,
            100,
            &abort,
            &mut output,
            &mut matched,
            &mut lines_truncated,
            &mut output_truncated,
        )
        .expect("scan settles");
        assert_eq!(
            matched, 0,
            "abort must fire before beta is recorded: {output:?}"
        );
        assert!(
            output.is_empty(),
            "partial rows leaked past abort: {output:?}"
        );
        let error = check_aborted(&abort).unwrap_err().to_string();
        assert_eq!(error, "Operation aborted");
    }

    // -----------------------------------------------------------------------
    // grep match-count limit is independent of context rows
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn grep_limit_counts_matches_not_context_rows() {
        let d = tmpdir();
        let content = (1..=40)
            .map(|i| format!("line {i} needle"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(d.join("g.txt"), content).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "needle",
            "context": 2,
            "limit": 3
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        // Match rows are `g.txt:NN: ...`; context rows are `g.txt-NN- ...`.
        let match_rows = text
            .lines()
            .filter(|line| {
                let rest = line.strip_prefix("g.txt").unwrap_or(line);
                rest.starts_with(':')
            })
            .count();
        assert_eq!(
            match_rows, 3,
            "limit must cap matches, not context rows: {text}"
        );
        assert!(
            text.contains("3 matches limit reached. Use limit=6 for more, or refine pattern"),
            "match-limit notice missing: {text}"
        );
        assert_eq!(result.details["matchLimitReached"], json!(3));
        assert!(
            text.contains("g.txt-4- line 4 needle"),
            "first trailing context row missing: {text}"
        );
        assert!(
            text.contains("g.txt-5- line 5 needle"),
            "second trailing context row missing: {text}"
        );
    }

    #[tokio::test]
    async fn grep_reports_match_limit_notice_and_details() {
        let d = tmpdir();
        let content = (1..=50)
            .map(|i| format!("needle {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(d.join("g.txt"), content).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "needle",
            "limit": 7
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("g.txt:"))
                .count(),
            7,
            "got: {text}"
        );
        assert!(
            text.contains("7 matches limit reached. Use limit=14 for more, or refine pattern"),
            "got: {text}"
        );
        assert_eq!(result.details["matchLimitReached"], json!(7));
    }

    // -----------------------------------------------------------------------
    // 50 KiB caps, notices, and details for ls / find / glob / grep
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ls_truncates_output_at_byte_cap_with_notice() {
        let d = tmpdir();
        let long = "x".repeat(240);
        for i in 0..400 {
            fs::write(d.join(format!("{long}{i:03}.txt")), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("ls").execute)(make_ctx(json!({})))
            .await
            .unwrap();
        let text = text_of(&result);
        // ~248 bytes per entry x 400 entries ~= 99 KiB -> byte cap wins.
        assert!(
            text.contains("50.0KB limit reached"),
            "byte-cap notice missing: {text:?}"
        );
        assert_eq!(result.details["truncation"]["truncated"], json!(true));
        assert!(
            text.len() < 55 * 1024,
            "ls output must respect the byte cap"
        );
    }

    #[tokio::test]
    async fn find_and_glob_truncate_at_byte_cap_with_notice() {
        let d = tmpdir();
        let long = "y".repeat(240);
        for i in 0..220 {
            let first = d.join(format!("batch-{i:03}"));
            let second = first.join("nested");
            fs::create_dir_all(&second).unwrap();
            fs::write(second.join(format!("{long}{i:03}.rs")), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        for (name, args) in [
            ("find", json!({ "pattern": "*.rs" })),
            ("glob", json!({ "pattern": "*.rs" })),
        ] {
            let result = (fixture.tool(name).execute)(make_ctx(args)).await.unwrap();
            let text = text_of(&result);
            assert!(
                text.contains("50.0KB limit reached"),
                "{name} byte-cap notice missing: {text:?}"
            );
            assert_eq!(
                result.details["truncation"]["truncated"],
                json!(true),
                "{name} truncation details missing"
            );
            assert!(text.len() < 55 * 1024, "{name} must respect the byte cap");
        }
    }

    #[tokio::test]
    async fn grep_truncates_output_at_byte_cap_with_notice() {
        let d = tmpdir();
        // 600-char lines: truncate_line caps each row at 500 chars. With a
        // long filename each of the 100 default-limit rows is ~620 bytes, so
        // the joined output (~62 KiB) exceeds the 50 KiB byte cap.
        let name = format!("{}fixture.txt", "t".repeat(110));
        let content = (0..100)
            .map(|i| format!("match {i} {}", "z".repeat(600)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(d.join(&name), content).unwrap();
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("grep").execute)(make_ctx(json!({
            "pattern": "^match"
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("50.0KB limit reached"),
            "byte-cap notice missing: {text:?}"
        );
        assert_eq!(result.details["truncation"]["truncated"], json!(true));
        assert!(
            text.contains("Some lines truncated to 500 chars. Use read tool to see full lines"),
            "line-truncation notice missing: {text:?}"
        );
        assert_eq!(result.details["linesTruncated"], json!(true));
        assert!(
            text.len() < 55 * 1024,
            "grep output must respect the byte cap"
        );
    }

    #[tokio::test]
    async fn ls_reports_entry_limit_notice_and_details() {
        let d = tmpdir();
        if d.join(".pi").exists() {
            fs::remove_dir_all(d.join(".pi")).unwrap();
        }
        for i in 0..12 {
            fs::write(d.join(format!("f{i:02}.txt")), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("ls").execute)(make_ctx(json!({ "limit": 5 })))
            .await
            .unwrap();
        let text = text_of(&result);
        assert_eq!(
            text.lines()
                .filter(|line| !line.is_empty() && !line.starts_with('['))
                .count(),
            5,
            "got: {text}"
        );
        assert!(
            text.contains("5 entries limit reached. Use limit=10 for more"),
            "got: {text}"
        );
        assert_eq!(result.details["entryLimitReached"], json!(5));
    }

    #[tokio::test]
    async fn find_reports_result_limit_notice_and_details() {
        let d = tmpdir();
        for i in 0..12 {
            fs::write(d.join(format!("f{i:02}.rs")), b"").unwrap();
        }
        let fixture = tool_generation_fixture(&d).await;
        let result = (fixture.tool("find").execute)(make_ctx(json!({
            "pattern": "*.rs",
            "limit": 5
        })))
        .await
        .unwrap();
        let text = text_of(&result);
        assert_eq!(
            text.lines().filter(|line| line.ends_with(".rs")).count(),
            5,
            "got: {text}"
        );
        assert!(
            text.contains("5 results limit reached. Use limit=10 for more, or refine pattern"),
            "got: {text}"
        );
        assert_eq!(result.details["resultLimitReached"], json!(5));
    }

}
