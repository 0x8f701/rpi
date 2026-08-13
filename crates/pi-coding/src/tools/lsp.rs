//! `lsp` tool: per-language code intelligence over an LSP language server.
//!
//! Speaks JSON-RPC 2.0 (Content-Length framing, `lsp-types` message shapes)
//! over a child process's stdio. Supported actions:
//!
//! - `hover`, `definition`, `references`, `diagnostics`, `symbols`,
//!   `rename`, `code_actions`, `capabilities`, `status`, `reload`
//! - deferred in this build (explicit error): `rename_file`,
//!   `implementation`, `type_definition`, `request`
//!
//! ## Server lifecycle: one server per invocation
//!
//! Every call that needs a server spawns it for that call only and runs the
//! shutdown/exit handshake before returning. Rationale for the MVP:
//!
//! - No long-lived process state to leak or race: a hung server is killed at
//!   call end, and there is no stale client whose file view diverges from
//!   disk after the `edit`/`write` tools mutate files.
//! - Abort safety is trivial: the child is killed on drop.
//! - Cost: per-call spawn+initialize latency (rust-analyzer ~1-3s cold).
//!   A per-`(lang, cwd)` cached pool with idle timeout + didChange sync
//!   (OMP-style client keying) is the documented follow-up.
//!
//! ## Language detection
//!
//! The `lang` argument overrides detection from the target path extension:
//!
//! | language key      | server binary           | extensions                        |
//! |-------------------|-------------------------|-----------------------------------|
//! | `rust`            | `rust-analyzer`         | `.rs`                             |
//! | `typescript`      | `typescript-language-server --stdio` | `.ts .tsx .mts .cts`  |
//! | `javascript`      | `typescript-language-server --stdio` | `.js .jsx .mjs .cjs`  |
//! | `go`              | `gopls`                 | `.go`                             |
//! | `python`          | `pyright-langserver --stdio` | `.py`                          |
//!
//! The server binary is resolved from `PATH`; a missing binary is a clear
//! error naming the command. Positions in `line`/`character` are 0-based and
//! interpreted as LSP UTF-16 code units, matching every server.
//!
//! `rename` applies the server's `WorkspaceEdit` text edits directly to disk
//! (serialized per file through the same mutation queue the `edit` tool
//! uses). `rename` is a mutation action: the tool instance must be built with
//! the `Write` capability, and every workspace-edit target is preflighted
//! before anything is written — the URI must be `file://` resolving to a
//! canonical path inside one of the configured workspace roots, and the
//! configured path permission rules must not deny it. If any target is
//! outside or denied, the whole edit is rejected atomically. File
//! create/rename/delete operations inside a workspace edit are rejected
//! explicitly as unsupported in the MVP.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::FutureExt;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::{Position, Range, Uri};
use serde_json::{json, Value};

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};

use crate::redact::redact_secrets;
use crate::settings::{
    permission_verdict_for_paths, PermissionRule, PermissionRulesSource, PermissionTool,
    PermissionVerdict,
};
use crate::WorkspaceRoots;

use super::lsp_client::{path_to_uri, uri_to_path, LspClient, REQUEST_TIMEOUT};
use super::mutation_queue::with_file_mutation_queue_path;
use super::{arg_int, arg_str, check_aborted, s_number, s_object, s_string, text_result};

/// Implemented actions, listed in the schema and in validation errors.
const ACTIONS: &str = "hover, definition, references, diagnostics, symbols, rename, code_actions, capabilities, status, reload";
/// Actions deferred from the OMP 14-action surface in this MVP build.
const DEFERRED_ACTIONS: &[&str] = &["rename_file", "implementation", "type_definition", "request"];
/// Cap on files opened per diagnostics call (OMP's `MAX_GLOB_DIAGNOSTIC_TARGETS`).
const MAX_DIAGNOSTIC_FILES: usize = 20;
/// Cap on rendered locations (definitions/references).
const MAX_LOCATIONS: usize = 50;
/// Cap on rendered symbols (workspace-wide searches can be large).
const MAX_SYMBOLS: usize = 200;
/// Cap on rendered code actions.
const MAX_CODE_ACTIONS: usize = 50;
/// Test-only: the rename target URIs the fake LSP server returns for the
/// `fake` language seam (see [`with_server`]). Per-test mutable so each test
/// controls the server's workspace edit without touching the process env
/// (edition 2024 forbids `unsafe`, which `std::env::set_var` requires).
#[cfg(test)]
static FAKE_LSP_RENAME_TARGETS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// Max directory depth walked for workspace diagnostics.
const DIAGNOSTICS_MAX_DEPTH: usize = 6;

/// One configured language server.
struct ServerSpec {
    command: &'static str,
    args: &'static [&'static str],
    language_id: &'static str,
}

/// Maps a language key (or alias) to its server specification.
fn server_for_lang(lang: &str) -> Option<ServerSpec> {
    let spec = match lang {
        "rust" | "rs" => ServerSpec {
            command: "rust-analyzer",
            args: &[],
            language_id: "rust",
        },
        "typescript" | "ts" => ServerSpec {
            command: "typescript-language-server",
            args: &["--stdio"],
            language_id: "typescript",
        },
        "javascript" | "js" => ServerSpec {
            command: "typescript-language-server",
            args: &["--stdio"],
            language_id: "javascript",
        },
        "go" | "golang" => ServerSpec {
            command: "gopls",
            args: &[],
            language_id: "go",
        },
        "python" | "py" => ServerSpec {
            command: "pyright-langserver",
            args: &["--stdio"],
            language_id: "python",
        },
        _ => return None,
    };
    Some(spec)
}

/// Normalizes a user-supplied `lang` argument, or errors listing the options.
fn normalize_lang(lang: &str) -> Result<String> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Ok("rust".to_owned()),
        "typescript" | "ts" | "tsx" => Ok("typescript".to_owned()),
        "javascript" | "js" | "jsx" => Ok("javascript".to_owned()),
        "go" | "golang" => Ok("go".to_owned()),
        "python" | "py" => Ok("python".to_owned()),
        // Test seam: drives the re-executed fake server via [`with_server`].
        #[cfg(test)]
        "fake" => Ok("fake".to_owned()),
        _ => bail!(
            "unsupported lsp language `{lang}` (supported: rust, typescript, javascript, go, python)"
        ),
    }
}

/// Maps a file extension to a language key.
fn lang_key_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "py" => Some("python"),
        _ => None,
    }
}

/// Maps a file path to a language key from its extension.
fn lang_key_for_path(path: &str) -> Option<&'static str> {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .and_then(|ext| lang_key_for_ext(&ext))
}

/// Maps a language key to its canonical source extension (diagnostics walks).
fn lang_ext(lang: &str) -> Option<&'static str> {
    match lang {
        "rust" => Some("rs"),
        "typescript" => Some("ts"),
        "javascript" => Some("js"),
        "go" => Some("go"),
        "python" => Some("py"),
        _ => None,
    }
}

/// Resolves the language key from the `lang` argument, then the path.
fn resolve_lang(args: &Value) -> Result<Option<String>> {
    let lang = arg_str(args, "lang");
    if !lang.is_empty() {
        return Ok(Some(normalize_lang(&lang)?));
    }
    let path = arg_str(args, "path");
    if path.is_empty() {
        return Ok(None);
    }
    Ok(lang_key_for_path(&path).map(String::from))
}

/// Resolves the language key, erroring when it cannot be determined.
fn resolve_lang_or(args: &Value, path: &str) -> Result<String> {
    if let Some(lang) = resolve_lang(args)? {
        return Ok(lang);
    }
    bail!(
        "cannot determine language for `{path}` (no `lang` argument and the path extension is not \
         one of: rs, ts, tsx, mts, cts, js, jsx, mjs, cjs, go, py)"
    )
}

/// Locates `binary` on `$PATH`.
fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolves a tool file/directory argument like the `read` tool does
/// (absolute, parent-relative, and `file://` paths allowed).
fn resolve_target(cwd: &str, path: &str) -> Result<String> {
    let workspace = crate::WorkspaceRoots::for_tool_factory(cwd);
    super::paths::resolve_read_path(path, &workspace)
}

/// Maps an lsp action to the tool capability its operations need:
/// `hover`/`definition`/`references`/`diagnostics`/`symbols`/`code_actions`/
/// `capabilities`/`status`/`reload` → Read (query-only), `rename` → Write
/// (applies the server's workspace edit to disk). The tool computes this
/// BEFORE dispatch and refuses actions outside the granted capability set, so
/// a read-only instance can query without gaining the ability to mutate, and
/// a mutation action can never reach the server on an instance built without
/// Write. Deferred and unknown actions have no capability (validation errors).
fn action_capability(action: &str) -> Option<ToolCapability> {
    match action {
        "hover" | "definition" | "references" | "diagnostics" | "symbols"
        | "code_actions" | "capabilities" | "status" | "reload" => {
            Some(ToolCapability::Read)
        }
        "rename" => Some(ToolCapability::Write),
        _ => None,
    }
}

fn capability_name(capability: ToolCapability) -> &'static str {
    match capability {
        ToolCapability::Read => "read",
        ToolCapability::Write => "write",
        ToolCapability::Exec => "exec",
    }
}

/// The full capability set a standalone lsp tool is granted.
fn all_capabilities() -> Vec<ToolCapability> {
    vec![ToolCapability::Read, ToolCapability::Write]
}

/// Builds the `lsp` tool for standalone construction: no path permission
/// rules (an empty source that never matches). Production factories pass the
/// session's live rules via [`lsp_tool_with_rules`].
pub(crate) fn lsp_tool(cwd: &str) -> AgentTool {
    lsp_tool_with_rules(cwd, crate::empty_permission_rules())
}

/// [`lsp_tool`] with the session's live path-permission-rule source.
///
/// The source is read at execution time — every call re-invokes it before
/// dispatch — so `permissionRules` settings changes apply to rename preflight
/// without rebuilding the tool (the same live source the host approval hook
/// consults).
pub(crate) fn lsp_tool_with_rules(cwd: &str, rules: PermissionRulesSource) -> AgentTool {
    lsp_tool_with_capabilities_and_rules(cwd, all_capabilities(), rules)
}

/// [`lsp_tool`] with an explicit granted capability set: actions whose
/// required capability is not granted are refused before any server contact
/// (tests exercise the read-only role this way).
fn lsp_tool_with_capabilities(cwd: &str, granted: Vec<ToolCapability>) -> AgentTool {
    lsp_tool_with_capabilities_and_rules(cwd, granted, crate::empty_permission_rules())
}

/// [`lsp_tool_with_capabilities`] with the live permission-rule source used
/// by rename preflight (read fresh on every call).
fn lsp_tool_with_capabilities_and_rules(
    cwd: &str,
    granted: Vec<ToolCapability>,
    rules: PermissionRulesSource,
) -> AgentTool {
    let description = format!(
        "Query a per-language LSP server (rust-analyzer, typescript-language-server, gopls, \
         pyright-langserver) over stdio JSON-RPC and return code-intelligence results as text. \
         Actions: {ACTIONS}. (rename_file, implementation, type_definition, request are deferred \
         in this build.) Positions in line/character are 0-based UTF-16 code units. One server is \
         spawned per call and shut down afterward. `rename` applies the server's workspace edit \
         directly to files on disk: every edit target is confined to the configured workspace \
         roots and checked against the configured path permission rules before anything is \
         written."
    );
    let params = s_object(
        vec![
            (
                "action",
                s_string(&format!(
                    "LSP action to run. One of: {ACTIONS} (deferred: {})",
                    DEFERRED_ACTIONS.join(", ")
                )),
            ),
            (
                "path",
                s_string(
                    "File or directory path (relative to cwd, absolute, or file://). Required by \
                     most actions; for diagnostics it may be a directory (bounded scan) or omitted \
                     for the whole workspace",
                ),
            ),
            (
                "query",
                s_string("Search query for workspace-wide symbol search (symbols action)"),
            ),
            (
                "symbol",
                s_string("Symbol name for workspace-wide symbol search (symbols action); alias for query"),
            ),
            (
                "line",
                s_number("0-based line for position-based actions (hover, definition, references, rename, code_actions); default 0"),
            ),
            (
                "character",
                s_number("0-based character column (UTF-16 units) for position-based actions; default 0"),
            ),
            (
                "end_line",
                s_number("0-based end line for the code_actions range; defaults to line"),
            ),
            (
                "end_character",
                s_number("0-based end character for the code_actions range; defaults to character"),
            ),
            (
                "new_name",
                s_string("New symbol name for the rename action (required there)"),
            ),
            (
                "lang",
                s_string(
                    "Force the language/server instead of detecting from the path extension: \
                     rust, typescript, javascript, go, python",
                ),
            ),
        ],
        vec!["action"],
    );
    let cwd = cwd.to_owned();
    let granted = std::sync::Arc::new(granted);
    // The workspace roots confine workspace-edit targets; `for_tool_factory`
    // roots the tool at the session cwd (sessions with additional roots build
    // the tool through the workspace-aware factory).
    let workspace = WorkspaceRoots::for_tool_factory(&cwd);
    AgentTool::new("lsp", description, params, move |ctx: ToolCallContext| {
        let cwd = cwd.clone();
        let granted = granted.clone();
        let workspace = workspace.clone();
        let rules = rules.clone();
        async move {
            // Live lookup: the session's permission-rule source is re-invoked
            // on every call, so `permissionRules` changes apply to rename
            // preflight without rebuilding the tool (same source the host
            // approval hook consults).
            let rules = rules();
            run_lsp_with(&cwd, &workspace, &granted, &rules, ctx.arguments, ctx.abort).await
        }
    })
    // The declared capability is Write (the strongest action, `rename`,
    // mutates files) so harness-level approval and read-only filtering treat
    // the tool honestly; the per-action gate above is the action-level
    // enforcement.
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Prefer lsp for symbol navigation: hover, definition, references, and diagnostics over blind grepping.".to_string(),
        "lsp spawns a fresh language server per call; expect a brief startup delay for heavy servers like rust-analyzer.".to_string(),
        "rename applies the server's workspace edit directly to files on disk — every target must stay inside the workspace; review the reported changes.".to_string(),
    ])
}

/// Entry point: validates the action, applies the action-aware capability
/// gate (queries → Read, `rename` → Write) BEFORE any server contact, and
/// dispatches. The tool instance captures its granted capability set at build
/// time ([`lsp_tool_with_capabilities`]), so a read-only instance can query
/// but can never reach a server with a mutation action.
pub(crate) async fn run_lsp(cwd: &str, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    let workspace = WorkspaceRoots::for_tool_factory(cwd);
    run_lsp_with(cwd, &workspace, &all_capabilities(), &[], args, abort).await
}

/// [`run_lsp`] with an explicit workspace, granted capability set, and path
/// permission rules (used by the tool instance and by tests).
async fn run_lsp_with(
    cwd: &str,
    workspace: &WorkspaceRoots,
    granted: &[ToolCapability],
    rules: &[PermissionRule],
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action");
    let action = action.trim();
    if action.is_empty() {
        bail!("lsp action is required (one of: {ACTIONS})");
    }
    if DEFERRED_ACTIONS.contains(&action) {
        bail!(
            "lsp action `{action}` is deferred in this build (implemented actions: {ACTIONS})"
        );
    }
    let Some(required) = action_capability(action) else {
        bail!("unknown lsp action `{action}` (expected one of: {ACTIONS})");
    };
    if !granted.contains(&required) {
        bail!(
            "lsp `{action}` requires the {} capability, which is not granted to this session",
            capability_name(required)
        );
    }
    match action {
        "hover" => run_position_query(cwd, &args, abort, PositionQuery::Hover).await,
        "definition" => run_position_query(cwd, &args, abort, PositionQuery::Definition).await,
        "references" => run_position_query(cwd, &args, abort, PositionQuery::References).await,
        "diagnostics" => run_diagnostics(cwd, &args, abort).await,
        "symbols" => run_symbols(cwd, &args, abort).await,
        "rename" => run_rename(cwd, workspace, rules, &args, abort).await,
        "code_actions" => run_code_actions(cwd, &args, abort).await,
        "capabilities" => run_capabilities(cwd, &args, abort).await,
        "status" => run_status(cwd, &args),
        "reload" => Ok(text_result(
            "lsp reload: no-op — this build spawns a fresh server per call, so there are no cached \
             clients or configuration to reload. A per-(lang, cwd) server pool with config caching \
             is the planned follow-up.",
        )),
        _ => unreachable!("action_capability validated the action"),
    }
}

/// Resolves the language server for `lang` and runs a session through
/// [`with_server_command`].
async fn with_server<T, F>(cwd: &str, lang: &str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut LspClient) -> futures_util::future::BoxFuture<'a, Result<T>>
        + Send
        + 'static,
{
    // Test seam: the `fake` language drives the re-executed fake server, so
    // tool-level execution (the full factory closure, including the live
    // permission-rule lookup) is exercisable without a real LSP binary on
    // PATH. The rename targets come from the per-test registry so each test
    // controls the server's workspace edit without touching process env.
    #[cfg(test)]
    if lang == "fake" {
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = tokio::process::Command::new(exe);
        command
            .arg("tools::lsp_client::tests::fake_lsp_server_process")
            .arg("--nocapture")
            .env("PI_FAKE_LSP_SERVER", "1");
        if let Some(targets) = FAKE_LSP_RENAME_TARGETS
            .lock()
            .expect("fake lsp targets lock")
            .clone()
        {
            command.env("PI_FAKE_LSP_RENAME_TARGETS", targets);
        }
        return with_server_command(command, "fake-lsp", cwd, f).await;
    }
    let spec = server_for_lang(lang)
        .ok_or_else(|| anyhow!("unsupported lsp language `{lang}`"))?;
    let binary = find_in_path(spec.command).ok_or_else(|| {
        anyhow!(
            "LSP server binary `{}` not found in PATH (required for language `{lang}`); \
             install it or pass a different lang",
            spec.command
        )
    })?;
    let mut command = tokio::process::Command::new(&binary);
    command.args(spec.args);
    with_server_command(command, &binary.display().to_string(), cwd, f).await
}

/// Runs a server session for an already-configured command (tests point it at
/// a fake server process): spawn, initialize, action, shutdown.
///
/// The shutdown handshake runs even when `f` fails so a server is never left
/// behind by a failed action.
async fn with_server_command<T, F>(
    mut command: tokio::process::Command,
    binary_path: &str,
    cwd: &str,
    f: F,
) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut LspClient) -> futures_util::future::BoxFuture<'a, Result<T>>
        + Send
        + 'static,
{
    command.current_dir(cwd);
    let mut client = LspClient::spawn_command(command).await?;
    client.initialize(cwd).await.map_err(|error| {
        let stderr = redact_secrets(&client.stderr_tail());
        let mut message = format!("LSP server `{binary_path}` failed to initialize: {error}");
        if !stderr.trim().is_empty() {
            message.push_str("\n--- server stderr ---\n");
            message.push_str(&stderr);
        }
        // The initialize error text is server-controlled; run the whole
        // message through the redactor, not just the tail.
        anyhow!(redact_secrets(&message))
    })?;
    let result = f(&mut client).await;
    client.shutdown().await;
    result
}

/// Opens `path` in the server with its current on-disk content (version 1).
async fn open_file(client: &mut LspClient, path: &str, lang: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let params = lsp_types::DidOpenTextDocumentParams {
        text_document: lsp_types::TextDocumentItem {
            uri: path_to_uri(path)?,
            language_id: lang.to_owned(),
            version: 1,
            text,
        },
    };
    client
        .notify(
            lsp_types::notification::DidOpenTextDocument::METHOD,
            serde_json::to_value(params)?,
        )
        .await
}

/// Reads a non-negative integer argument, defaulting to `default`.
fn position_arg(args: &Value, key: &str, default: u32) -> Result<u32> {
    Ok(arg_int(args, key)?
        .unwrap_or(i64::from(default))
        .clamp(0, i64::from(u32::MAX)) as u32)
}

/// Resolves the required target file for position-based actions.
fn resolve_target_file(cwd: &str, args: &Value) -> Result<String> {
    let path = arg_str(args, "path");
    if path.trim().is_empty() {
        bail!("lsp `path` is required for this action");
    }
    let resolved = resolve_target(cwd, &path)?;
    if !Path::new(&resolved).is_file() {
        bail!("lsp target is not a file: {resolved}");
    }
    Ok(resolved)
}

/// Which position-based query to run.
#[derive(Clone, Copy)]
enum PositionQuery {
    Hover,
    Definition,
    References,
}

impl PositionQuery {
    fn method(self) -> &'static str {
        match self {
            Self::Hover => lsp_types::request::HoverRequest::METHOD,
            Self::Definition => lsp_types::request::GotoDefinition::METHOD,
            Self::References => lsp_types::request::References::METHOD,
        }
    }
}

async fn run_position_query(
    cwd: &str,
    args: &Value,
    abort: AbortSignal,
    query: PositionQuery,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = resolve_target_file(cwd, args)?;
    let line = position_arg(args, "line", 0)?;
    let character = position_arg(args, "character", 0)?;
    let lang = resolve_lang_or(args, &path)?;
    let lang_for_server = lang.clone();

    with_server(cwd, &lang, move |client| {
        let lang = lang_for_server;
        async move {
            open_file(client, &path, &lang).await?;
            let uri = path_to_uri(&path)?;
            // Best-effort analysis readiness (shared helper): servers push
            // publishDiagnostics once the document is analyzed; when the first
            // push is empty (cold-start initial load) the helper settles and
            // pumps a documentSymbol barrier so the query does not race the
            // first analysis (rust-analyzer returns null or `ContentModified`
            // -32801 otherwise). Bounded and non-fatal: the request goes out
            // regardless.
            let _ = client.wait_ready_diagnostics(uri.as_str()).await;
            let mut params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            });
            if matches!(query, PositionQuery::References) {
                params["context"] = json!({ "includeDeclaration": true });
            }
            let result = client
                .request_with_retry(query.method(), params, REQUEST_TIMEOUT)
                .await?;
            Ok(text_result(match query {
                PositionQuery::Hover => format_hover(&result),
                PositionQuery::Definition => format_locations(&result, "definition"),
                PositionQuery::References => format_locations(&result, "references"),
            }))
        }
        .boxed()
    })
    .await
}

/// Collects source files under `root` for `lang`, bounded and skipping
/// dependency/vendor directories.
fn workspace_source_files(root: &str, lang: &str) -> Result<Vec<String>> {
    let Some(ext) = lang_ext(lang) else {
        bail!("unsupported lsp language `{lang}`");
    };
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(DIAGNOSTICS_MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                "target" | "node_modules" | ".git" | "dist" | "build" | "vendor"
            )
        })
    {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            files.push(entry.path().display().to_string());
            if files.len() >= MAX_DIAGNOSTIC_FILES {
                break;
            }
        }
    }
    Ok(files)
}

async fn run_diagnostics(
    cwd: &str,
    args: &Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let lang = resolve_lang_or(args, &arg_str(args, "path")).map_err(|e| {
        anyhow!(
            "{e}\n(hint: for workspace-wide diagnostics pass lang=rust|typescript|javascript|go|python)"
        )
    })?;

    let path_arg = arg_str(args, "path");
    let targets: Vec<String> = if path_arg.trim().is_empty() {
        workspace_source_files(cwd, &lang)?
    } else {
        let resolved = resolve_target(cwd, &path_arg)?;
        if Path::new(&resolved).is_dir() {
            workspace_source_files(&resolved, &lang)?
        } else {
            vec![resolved]
        }
    };
    if targets.is_empty() {
        return Ok(text_result(format!(
            "diagnostics: no `{lang}` source files found under the target"
        )));
    }

    let lang_for_server = lang.clone();

    with_server(cwd, &lang, move |client| {
        async move {
            for target in &targets {
                open_file(client, target, &lang_for_server).await?;
            }
            let mut reports = Vec::with_capacity(targets.len());
            for target in &targets {
                let uri = path_to_uri(target)?;
                // The readiness helper reports the LATEST push for the URI:
                // a cold-start server's initial empty set is skipped via the
                // documentSymbol analysis barrier, so a file with real errors
                // is never reported as clean.
                let params = client.wait_ready_diagnostics(uri.as_str()).await?;
                reports.push(format_diagnostics(&params, "diagnostics"));
            }
            // Diagnostics the server pushed for documents we did not open
            // (e.g. whole-workspace pushes) are reported as a secondary note.
            let mut extra = 0usize;
            for params in &client.diagnostics {
                if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                    if !targets.iter().any(|t| {
                        path_to_uri(t).is_ok_and(|u| u.as_str() == uri)
                    }) {
                        reports.push(format_diagnostics(params, "diagnostics (workspace)"));
                        extra += 1;
                        if extra >= MAX_DIAGNOSTIC_FILES {
                            break;
                        }
                    }
                }
            }
            Ok(text_result(reports.join("\n")))
        }
        .boxed()
    })
    .await
}

async fn run_symbols(cwd: &str, args: &Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path_arg = arg_str(args, "path");
    let query = {
        let query = arg_str(args, "query");
        let symbol = arg_str(args, "symbol");
        if query.is_empty() { symbol } else { query }
    };
    let lang = resolve_lang_or(args, &path_arg)?;

    // A file target → document symbols; a directory/absent target or a query
    // → workspace-wide symbol search.
    let file_target = if path_arg.trim().is_empty() {
        None
    } else {
        let resolved = resolve_target(cwd, &path_arg)?;
        Path::new(&resolved).is_file().then_some(resolved)
    };

    let lang_for_server = lang.clone();

    with_server(cwd, &lang, move |client| {
        async move {
            let result = if let Some(path) = &file_target {
                open_file(client, path, &lang_for_server).await?;
                let uri = path_to_uri(path)?;
                client
                    .request_with_retry(
                        lsp_types::request::DocumentSymbolRequest::METHOD,
                        json!({ "textDocument": { "uri": uri } }),
                        REQUEST_TIMEOUT,
                    )
                    .await?
            } else {
                client
                    .request_with_retry(
                        lsp_types::request::WorkspaceSymbolRequest::METHOD,
                        json!({ "query": query }),
                        REQUEST_TIMEOUT,
                    )
                    .await?
            };
            Ok(text_result(format_symbols(&result)))
        }
        .boxed()
    })
    .await
}

async fn run_rename(
    cwd: &str,
    workspace: &WorkspaceRoots,
    rules: &[PermissionRule],
    args: &Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = resolve_target_file(cwd, args)?;
    let line = position_arg(args, "line", 0)?;
    let character = position_arg(args, "character", 0)?;
    let new_name = arg_str(args, "new_name");
    if new_name.trim().is_empty() {
        bail!("lsp rename requires new_name");
    }
    let lang = resolve_lang_or(args, &path)?;
    let lang_for_server = lang.clone();
    let workspace = workspace.clone();
    let rules = rules.to_vec();
    with_server(cwd, &lang, move |client| {
        let lang = lang_for_server;
        async move {
            rename_with_client(client, &path, &lang, line, character, &new_name, &workspace, &rules)
                .await
        }
        .boxed()
    })
    .await
}

/// The rename exchange against an already-initialized client: open the
/// document, request the rename, and apply the resulting workspace edit.
/// Tests point [`with_server_command`] at the fake server and call this
/// directly to exercise the full rename path.
async fn rename_with_client(
    client: &mut LspClient,
    path: &str,
    lang: &str,
    line: u32,
    character: u32,
    new_name: &str,
    workspace: &WorkspaceRoots,
    rules: &[PermissionRule],
) -> Result<AgentToolResult> {
    open_file(client, path, lang).await?;
    let uri = path_to_uri(path)?;
    // Analysis readiness: rename must not be issued before the server has
    // analyzed the document — a cold-start rust-analyzer refuses (or fails)
    // renames that race the initial project load. The shared helper waits for
    // the document's diagnostics, skipping a stale initial empty push via the
    // documentSymbol analysis barrier, and is strict here: a rename is never
    // fired against an unanalyzed document.
    client.wait_ready_diagnostics(uri.as_str()).await?;
    let params = lsp_types::RenameParams {
        text_document_position: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
            position: Position { line, character },
        },
        work_done_progress_params: Default::default(),
        new_name: new_name.to_owned(),
    };
    let result = client
        .request_with_retry(
            lsp_types::request::Rename::METHOD,
            serde_json::to_value(params)?,
            REQUEST_TIMEOUT,
        )
        .await?;
    Ok(text_result(apply_workspace_edit(&result, workspace, rules).await?))
}

async fn run_code_actions(cwd: &str, args: &Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = resolve_target_file(cwd, args)?;
    let line = position_arg(args, "line", 0)?;
    let character = position_arg(args, "character", 0)?;
    let end_line = position_arg(args, "end_line", line)?;
    let end_character = position_arg(args, "end_character", character)?;
    let lang = resolve_lang_or(args, &path)?;
    let lang_for_server = lang.clone();

    with_server(cwd, &lang, move |client| {
        let lang = lang_for_server;
        async move {
            open_file(client, &path, &lang).await?;
            let uri = path_to_uri(&path)?;
            // Seed the code-action context with the LATEST diagnostics the
            // server has published for this document (many actions are
            // diagnostic-driven): the shared readiness helper skips a stale
            // initial empty push via the documentSymbol analysis barrier.
            let mut diagnostics = Vec::new();
            let wait = client.wait_ready_diagnostics(uri.as_str()).await;
            if let Ok(params) = wait {
                if let Some(list) = params.get("diagnostics").and_then(Value::as_array) {
                    diagnostics.clone_from(list);
                }
            }
            let params = lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range: Range {
                    start: Position { line, character },
                    end: Position {
                        line: end_line,
                        character: end_character,
                    },
                },
                context: lsp_types::CodeActionContext {
                    diagnostics: serde_json::from_value(Value::Array(diagnostics))?,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let result = client
                .request_with_retry(
                    lsp_types::request::CodeActionRequest::METHOD,
                    serde_json::to_value(params)?,
                    REQUEST_TIMEOUT,
                )
                .await?;
            Ok(text_result(format_code_actions(&result)))
        }
        .boxed()
    })
    .await
}

async fn run_capabilities(cwd: &str, args: &Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let path = arg_str(args, "path");
    let lang = resolve_lang_or(args, &path)?;

    let lang_for_server = lang.clone();

    with_server(cwd, &lang, move |client| {
        let lang = lang_for_server;
        async move {
            let capabilities = client.capabilities.clone();
            Ok(text_result(format!(
                "LSP server capabilities for language `{lang}`:\n{}",
                serde_json::to_string_pretty(&capabilities)?
            )))
        }
        .boxed()
    })
    .await
}

/// Reports the server registry: which binary each language maps to and
/// whether it is present on `PATH`. No server is spawned.
fn run_status(cwd: &str, args: &Value) -> Result<AgentToolResult> {
    let lang = resolve_lang(args)?;
    let mut lines = vec![
        format!("lsp tool status (cwd: {cwd})"),
        "  lifecycle: one server per call — spawned on demand, shutdown handshake on return".to_owned(),
    ];
    let describe = |lang: &str, lines: &mut Vec<String>| {
        if let Some(spec) = server_for_lang(lang) {
            let location = find_in_path(spec.command)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "NOT FOUND in PATH".to_owned());
            lines.push(format!(
                "  {lang}: {} {}{}  language_id={}",
                spec.command,
                if spec.args.is_empty() { "" } else { "(--stdio) " },
                location,
                spec.language_id
            ));
        } else {
            lines.push(format!("  {lang}: no server registered"));
        }
    };
    if let Some(lang) = lang {
        describe(&lang, &mut lines);
    } else {
        for key in ["rust", "typescript", "javascript", "go", "python"] {
            describe(key, &mut lines);
        }
        lines.push("  (pass path or lang to narrow status to one language)".to_owned());
    }
    Ok(text_result(lines.join("\n")))
}

// ---------------------------------------------------------------------------
// Result formatting
// ---------------------------------------------------------------------------

/// Formats a hover result (MarkupContent, MarkedString(s), or plain string).
fn format_hover(result: &Value) -> String {
    if result.is_null() {
        return "No hover information at this position.".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    match result.get("contents") {
        Some(Value::String(text)) => parts.push(text.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                push_marked_string(item, &mut parts);
            }
        }
        Some(Value::Object(object)) => {
            if object.contains_key("value") && object.contains_key("kind") {
                // MarkupContent { kind, value }
                if let Some(value) = object.get("value").and_then(Value::as_str) {
                    parts.push(value.to_owned());
                }
            } else {
                push_marked_string(result.get("contents").unwrap_or(&Value::Null), &mut parts);
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        return "No hover information at this position.".to_owned();
    }
    parts.join("\n\n")
}

fn push_marked_string(item: &Value, parts: &mut Vec<String>) {
    match item {
        Value::String(text) => parts.push(text.clone()),
        Value::Object(object) => {
            if let Some(value) = object.get("value").and_then(Value::as_str) {
                if let Some(language) = object.get("language").and_then(Value::as_str) {
                    parts.push(format!("```{language}\n{value}\n```"));
                } else {
                    parts.push(value.to_owned());
                }
            }
        }
        _ => {}
    }
}

/// Normalizes a definition/references response into a list of location
/// objects (handles single Location, Location[], and LocationLink[]).
fn normalize_locations(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![value.clone()],
        _ => Vec::new(),
    }
}

/// Formats `path:line:character` (1-based display) for a Location or
/// LocationLink object.
fn format_location(location: &Value) -> String {
    let (uri, range) = if let Some(target_uri) = location.get("targetUri") {
        (
            target_uri.as_str().unwrap_or_default(),
            location.get("targetRange").unwrap_or(&Value::Null),
        )
    } else {
        (
            location.get("uri").and_then(Value::as_str).unwrap_or_default(),
            location.get("range").unwrap_or(&Value::Null),
        )
    };
    let path = uri_to_path(uri)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| uri.to_string());
    let start = range.get("start").cloned().unwrap_or_else(|| json!({}));
    let line = start.get("line").and_then(Value::as_u64).unwrap_or(0) + 1;
    let character = start.get("character").and_then(Value::as_u64).unwrap_or(0) + 1;
    format!("{path}:{line}:{character}")
}

fn format_locations(locations: &Value, label: &str) -> String {
    let all = normalize_locations(locations);
    if all.is_empty() {
        return format!("{label}: no locations found");
    }
    let shown = all.len().min(MAX_LOCATIONS);
    let mut lines = vec![format!("{label} ({} location(s))", all.len())];
    for location in &all[..shown] {
        lines.push(format!("  {}", format_location(location)));
    }
    if all.len() > shown {
        lines.push(format!("  ... {} more omitted", all.len() - shown));
    }
    lines.join("\n")
}

fn severity_name(severity: u64) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "hint",
    }
}

/// Formats one publishDiagnostics params object as a text report.
fn format_diagnostics(params: &Value, label: &str) -> String {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or_default();
    let path = uri_to_path(uri)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| uri.to_string());
    let diagnostics = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if diagnostics.is_empty() {
        return format!("{label}: no diagnostics for {path}");
    }
    let mut lines = vec![format!(
        "{label}: {} diagnostic(s) for {path}",
        diagnostics.len()
    )];
    for diagnostic in diagnostics {
        let severity = diagnostic.get("severity").and_then(Value::as_u64).unwrap_or(3);
        let message = diagnostic
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let start = diagnostic
            .get("range")
            .and_then(|r| r.get("start"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let line = start.get("line").and_then(Value::as_u64).unwrap_or(0) + 1;
        let character = start.get("character").and_then(Value::as_u64).unwrap_or(0) + 1;
        lines.push(format!(
            "  {path}:{line}:{character} {}: {message}",
            severity_name(severity)
        ));
    }
    lines.join("\n")
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Symbol",
    }
}

/// Formats a documentSymbol tree (recursive) or flat SymbolInformation list.
fn format_symbols(result: &Value) -> String {
    let Some(items) = result.as_array() else {
        return if result.is_null() {
            "symbols: none found".to_owned()
        } else {
            "symbols: unexpected server response".to_owned()
        };
    };
    let nested = items.iter().any(|item| item.get("selectionRange").is_some());
    let mut out = String::new();
    let mut count = 0usize;
    if nested {
        for item in items {
            push_symbol_tree(item, 0, &mut out, &mut count);
            if count >= MAX_SYMBOLS {
                break;
            }
        }
    } else {
        for item in items {
            if count >= MAX_SYMBOLS {
                break;
            }
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let kind = item.get("kind").and_then(Value::as_u64).unwrap_or(0);
            let location = item.get("location").cloned().unwrap_or_else(|| json!({}));
            let at = format_location(&location);
            let container = item
                .get("containerName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let context = if container.is_empty() {
                String::new()
            } else {
                format!(" (in {container})")
            };
            out.push_str(&format!("  {name} — {} — {at}{context}\n", symbol_kind_name(kind)));
            count += 1;
        }
    }
    if out.is_empty() {
        return "symbols: none found".to_owned();
    }
    if count >= MAX_SYMBOLS {
        out.push_str(&format!("  ... symbol limit ({MAX_SYMBOLS}) reached\n"));
    }
    format!("symbols ({} shown):\n{out}", count.min(MAX_SYMBOLS))
}

fn push_symbol_tree(item: &Value, depth: usize, out: &mut String, count: &mut usize) {
    if *count >= MAX_SYMBOLS {
        return;
    }
    let indent = "  ".repeat(depth);
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let kind = item.get("kind").and_then(Value::as_u64).unwrap_or(0);
    let detail = item.get("detail").and_then(Value::as_str).unwrap_or_default();
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" — {detail}")
    };
    out.push_str(&format!("{indent}{name} ({}){detail}\n", symbol_kind_name(kind)));
    *count += 1;
    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            push_symbol_tree(child, depth + 1, out, count);
        }
    }
}

/// Formats a codeAction response (actions are listed, not applied).
fn format_code_actions(result: &Value) -> String {
    let Some(actions) = result.as_array() else {
        return if result.is_null() {
            "code_actions: none available at this position".to_owned()
        } else {
            "code_actions: unexpected server response".to_owned()
        };
    };
    if actions.is_empty() {
        return "code_actions: none available at this position".to_owned();
    }
    let shown = actions.len().min(MAX_CODE_ACTIONS);
    let mut lines = vec![format!("code_actions ({} shown):", shown)];
    for action in &actions[..shown] {
        let title = action.get("title").and_then(Value::as_str).unwrap_or_default();
        let kind = action
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = if kind.is_empty() {
            String::new()
        } else {
            format!(" [{kind}]")
        };
        let diagnostics = action
            .get("diagnostics")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let note = if diagnostics > 0 {
            format!(" (fixes {diagnostics} diagnostic(s))")
        } else {
            String::new()
        };
        lines.push(format!("  - {title}{kind}{note}"));
    }
    if actions.len() > shown {
        lines.push(format!("  ... {} more omitted", actions.len() - shown));
    }
    lines.push("  (actions are listed only; apply is deferred in this build)".to_owned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Workspace edit application (rename)
// ---------------------------------------------------------------------------

/// Converts an LSP position (0-based line, UTF-16 code-unit character) to a
/// byte offset in `text`. Positions past the end clamp to the end.
fn position_to_byte_offset(text: &str, line: u32, character: u32) -> usize {
    let mut line_start = 0usize;
    let mut current_line = 0u32;
    for (index, byte) in text.bytes().enumerate() {
        if current_line >= line {
            break;
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = index + 1;
        }
    }
    if current_line < line {
        return text.len();
    }
    // Walk UTF-16 code units from the line start.
    let mut seen = 0u32;
    let mut offset = line_start;
    for ch in text[line_start..].chars() {
        if ch == '\n' || seen >= character {
            break;
        }
        let units = ch.len_utf16() as u32;
        if seen + units > character {
            break;
        }
        seen += units;
        offset += ch.len_utf8();
    }
    offset
}

/// Converts an LSP range to byte offsets, clamping to the document bounds.
fn range_to_byte_offsets(text: &str, range: &Value) -> Result<(usize, usize)> {
    let start = range
        .get("start")
        .ok_or_else(|| anyhow!("text edit range missing start"))?;
    let end = range
        .get("end")
        .ok_or_else(|| anyhow!("text edit range missing end"))?;
    let start_offset = position_to_byte_offset(
        text,
        start.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    );
    let end_offset = position_to_byte_offset(
        text,
        end.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        end.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    );
    let start_offset = start_offset.min(text.len());
    let end_offset = end_offset.clamp(start_offset, text.len());
    Ok((start_offset, end_offset))
}

/// Applies a list of text edits to `path` (already resolved and preflighted),
/// serialized through the per-file mutation queue. Returns the number of edits
/// applied.
async fn apply_text_edits(path: &Path, edits: &[Value]) -> Result<usize> {
    let path_str = path.to_string_lossy().into_owned();
    with_file_mutation_queue_path(path, || async move {
        let mut content =
            std::fs::read_to_string(path).with_context(|| format!("reading {path_str} for edit"))?;
        // Apply from the end of the document backwards so earlier ranges stay
        // valid.
        let mut sorted = edits.to_vec();
        sorted.sort_by_key(|edit| {
            let range = edit.get("range").cloned().unwrap_or_else(|| json!({}));
            let start = range.get("start").cloned().unwrap_or_else(|| json!({}));
            let line = start.get("line").and_then(Value::as_u64).unwrap_or(0);
            let character = start.get("character").and_then(Value::as_u64).unwrap_or(0);
            (line, character)
        });
        sorted.reverse();
        let mut applied = 0usize;
        for edit in sorted {
            let range = edit
                .get("range")
                .ok_or_else(|| anyhow!("text edit missing range"))?;
            let new_text = edit.get("newText").and_then(Value::as_str).unwrap_or_default();
            let (start, end) = range_to_byte_offsets(&content, range)?;
            content.replace_range(start..end, new_text);
            applied += 1;
        }
        std::fs::write(path, &content).with_context(|| format!("writing {path_str}"))?;
        Ok(applied)
    })
    .await
}

/// Resolves a WorkspaceEdit target URI to a path confined within one of the
/// configured workspace roots. Only `file://` URIs are accepted; the resolved
/// path is checked lexically against the roots and the nearest existing
/// ancestor is canonicalized, so a symlink inside the workspace cannot
/// redirect the write outside.
fn resolve_edit_target(uri: &str, workspace: &WorkspaceRoots) -> Result<String> {
    let path = uri_to_path(uri).map_err(|_| {
        anyhow!("lsp rename refused: workspace edit target is not a file:// uri: `{uri}`")
    })?;
    super::paths::resolve_scoped_path(&path.to_string_lossy(), workspace).map_err(|_| {
        anyhow!(
            "lsp rename refused: workspace edit target `{uri}` is outside the configured \
             workspace roots"
        )
    })
}

/// Preflights every target of a WorkspaceEdit: each URI must resolve to a
/// path inside the configured workspace roots and must not be denied (or
/// require host approval) by the configured path permission rules. Resource
/// operations (create/rename/delete documentChanges) are unsupported and
/// rejected explicitly. Returns the resolved `(uri, path, edits)` list, or an
/// error that rejects the whole edit before anything is written.
fn preflight_workspace_edit(
    edit: &Value,
    workspace: &WorkspaceRoots,
    rules: &[PermissionRule],
) -> Result<Vec<(String, String, Vec<Value>)>> {
    let mut targets: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            let Some(text_document) = change.get("textDocument") else {
                // CreateFile / RenameFile / DeleteFile resource operations.
                bail!(
                    "lsp rename refused: workspace edit contains an unsupported file \
                     create/rename/delete operation"
                );
            };
            // Defense in depth: an entry carrying a resource-operation `kind`
            // marker is rejected even if it also embeds a textDocument.
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                bail!(
                    "lsp rename refused: workspace edit contains an unsupported file \
                     create/rename/delete operation (`{kind}`)"
                );
            }
            let uri = text_document
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("workspace edit textDocument missing uri"))?;
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            targets.push((uri.to_owned(), edits));
        }
    } else if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            targets.push((
                uri.clone(),
                edits.as_array().cloned().unwrap_or_default(),
            ));
        }
    }
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // Containment: every target must resolve inside the configured workspace
    // roots (canonicalized). Any failure rejects the entire edit atomically.
    let mut resolved: Vec<(String, String, Vec<Value>)> = Vec::with_capacity(targets.len());
    for (uri, edits) in &targets {
        let path = resolve_edit_target(uri, workspace)?;
        resolved.push((uri.clone(), path, edits.clone()));
    }

    // Permission rules: the same evaluation used for edit/write targets,
    // strongest verdict across targets wins. `Ask` cannot be honored from
    // inside the tool (no interactive channel), so it rejects too.
    let rule_targets: Vec<PathBuf> = resolved.iter().map(|(_, path, _)| PathBuf::from(path)).collect();
    match permission_verdict_for_paths(
        PermissionTool::Lsp,
        &rule_targets,
        workspace.cwd(),
        rules,
    ) {
        PermissionVerdict::Deny(reason) => bail!("lsp rename refused: {reason}"),
        PermissionVerdict::Ask => bail!(
            "lsp rename refused: a workspace edit target requires host approval"
        ),
        PermissionVerdict::Allow | PermissionVerdict::NoMatch => {}
    }
    Ok(resolved)
}

/// Applies a WorkspaceEdit (from a rename) to disk and renders a report.
///
/// Every target is preflighted BEFORE any write (containment within the
/// configured workspace roots + path permission rules); if any target is
/// outside or denied, the whole edit is rejected and nothing is written.
async fn apply_workspace_edit(
    edit: &Value,
    workspace: &WorkspaceRoots,
    rules: &[PermissionRule],
) -> Result<String> {
    if edit.is_null() {
        return Ok("rename: the server returned no workspace edit (nothing to change)".to_owned());
    }
    let targets = preflight_workspace_edit(edit, workspace, rules)?;
    if targets.is_empty() {
        return Ok("rename: the server returned an empty workspace edit".to_owned());
    }

    let mut reports: Vec<String> = Vec::new();
    let mut applied_total = 0usize;
    for (_, path, edits) in &targets {
        let applied = apply_text_edits(Path::new(path), edits).await?;
        applied_total += applied;
        reports.push(format!("  {path}: {applied} text edit(s) applied"));
    }
    reports.insert(0, format!("rename: applied {applied_total} text edit(s)"));
    Ok(reports.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::AbortController;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-lsp-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn abort() -> AbortSignal {
        let (controller, abort) = AbortController::new();
        std::mem::forget(controller);
        abort
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let cwd = tmpdir().to_string_lossy().into_owned();
        let err = run_lsp(&cwd, json!({ "action": "nope" }), abort())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown lsp action `nope`"), "{err}");
        assert!(err.contains("hover"), "{err}");
    }

    #[tokio::test]
    async fn missing_action_is_rejected() {
        let cwd = tmpdir().to_string_lossy().into_owned();
        let err = run_lsp(&cwd, json!({ "path": "x.rs" }), abort())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("action is required"), "{err}");
    }

    #[tokio::test]
    async fn deferred_actions_are_rejected_with_clear_error() {
        let cwd = tmpdir().to_string_lossy().into_owned();
        for action in ["rename_file", "implementation", "type_definition", "request"] {
            let err = run_lsp(&cwd, json!({ "action": action }), abort())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("deferred") && err.contains(action),
                "action {action}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn position_actions_validate_before_spawn() {
        let cwd = tmpdir().to_string_lossy().into_owned();
        // No path → error, no server spawn attempted.
        let err = run_lsp(&cwd, json!({ "action": "hover" }), abort())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("path"), "{err}");
        // Path is not a file → error.
        let err = run_lsp(&cwd, json!({ "action": "hover", "path": "missing.rs" }), abort())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a file"), "{err}");
    }

    #[tokio::test]
    async fn unknown_language_is_rejected() {
        let dir = tmpdir();
        let path = dir.join("sample.xyz");
        std::fs::write(&path, "x").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let err = run_lsp(
            &cwd,
            json!({ "action": "hover", "path": "sample.xyz", "lang": "cobol" }),
            abort(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("unsupported lsp language `cobol`"), "{err}");
    }

    #[tokio::test]
    async fn rename_requires_new_name() {
        let dir = tmpdir();
        std::fs::write(dir.join("sample.rs"), "fn main() {}").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let err = run_lsp(
            &cwd,
            json!({ "action": "rename", "path": "sample.rs", "line": 0, "character": 3 }),
            abort(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("new_name"), "{err}");
    }

    #[tokio::test]
    async fn missing_server_binary_is_a_clear_error() {
        let dir = tmpdir();
        let path = dir.join("sample.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let err = run_lsp(&cwd, json!({ "action": "hover", "path": "sample.py" }), abort())
            .await
            .unwrap_err()
            .to_string();
        // pyright-langserver is not installed in test environments; the error
        // must name the binary it looked for.
        if err.contains("not found in PATH") {
            assert!(err.contains("pyright-langserver"), "{err}");
        } else {
            // If it IS installed, the error must come from the server itself
            // (initialize) — never a panic or silent success.
            assert!(!err.is_empty(), "expected an error, got success");
        }
    }

    #[tokio::test]
    async fn initialize_failure_redacts_secrets_from_server_stderr() {
        // A fake LSP server (this test binary re-executed in fake-server
        // mode) logs credential-shaped text to stderr and then refuses
        // initialize: the error must embed the stderr tail but never the
        // secret values.
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = tokio::process::Command::new(exe);
        command
            .arg("tools::lsp_client::tests::fake_lsp_server_process")
            .arg("--nocapture")
            .env("PI_FAKE_LSP_SERVER", "1")
            .env("PI_FAKE_LSP_BOOM", "1");
        let cwd = tmpdir().to_string_lossy().into_owned();
        let err = with_server_command(command, "fake-lsp", &cwd, |_client| async { Ok(()) }.boxed())
            .await
            .expect_err("initialize must fail");
        let text = err.to_string();
        assert!(text.contains("failed to initialize"), "{text}");
        assert!(text.contains("--- server stderr ---"), "{text}");
        let secrets = [
            ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghij0123456789"].concat(),
            ["s", "k-", "abcdefghijklmnop", "1234"].concat(),
        ];
        for secret in &secrets {
            assert!(!text.contains(secret.as_str()), "{secret} leaked: {text}");
        }
        assert!(text.contains("[REDACTED]"), "redaction marker missing: {text}");
    }

    #[tokio::test]
    async fn status_reports_registry_without_spawning() {
        let dir = tmpdir();
        std::fs::write(dir.join("sample.rs"), "fn main() {}").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let result = run_lsp(
            &cwd,
            json!({ "action": "status", "path": "sample.rs" }),
            abort(),
        )
        .await
        .expect("status action");
        let text = text_of(&result);
        assert!(text.contains("rust-analyzer"), "{text}");
        assert!(text.contains("one server per call"), "{text}");
    }

    #[tokio::test]
    async fn reload_is_a_noop_under_per_call_lifecycle() {
        let cwd = tmpdir().to_string_lossy().into_owned();
        let result = run_lsp(&cwd, json!({ "action": "reload" }), abort())
            .await
            .expect("reload action");
        let text = text_of(&result);
        assert!(text.contains("no-op"), "{text}");
    }

    #[test]
    fn language_detection_covers_registry() {
        for (path, expected) in [
            ("a.rs", Some("rust")),
            ("a.ts", Some("typescript")),
            ("a.tsx", Some("typescript")),
            ("a.mts", Some("typescript")),
            ("a.js", Some("javascript")),
            ("a.mjs", Some("javascript")),
            ("a.go", Some("go")),
            ("a.py", Some("python")),
            ("a.txt", None),
        ] {
            assert_eq!(lang_key_for_path(path), expected, "{path}");
        }
        for (lang, command) in [
            ("rust", "rust-analyzer"),
            ("typescript", "typescript-language-server"),
            ("javascript", "typescript-language-server"),
            ("go", "gopls"),
            ("python", "pyright-langserver"),
        ] {
            assert_eq!(server_for_lang(lang).unwrap().command, command, "{lang}");
        }
        assert_eq!(normalize_lang("TS").unwrap(), "typescript");
        assert!(normalize_lang("cobol").is_err());
    }

    #[test]
    fn position_offsets_are_utf16_aware() {
        // h é l l o — é is 2 bytes but 1 UTF-16 unit.
        let text = "héllo\nwörld 世界\n";
        assert_eq!(position_to_byte_offset(text, 0, 0), 0);
        assert_eq!(position_to_byte_offset(text, 0, 1), 1); // é
        assert_eq!(position_to_byte_offset(text, 0, 4), 5); // o — 1+2+1+1 bytes
        let line2_start = text.find('\n').unwrap() + 1;
        assert_eq!(position_to_byte_offset(text, 1, 0), line2_start); // w
        // Units on line 1: w ö r l d ' ' 世 界; 界 is unit 7 and starts at
        // 1+2+1+1+1+1+3 = 10 bytes past the line start.
        assert_eq!(position_to_byte_offset(text, 1, 7), line2_start + 10);
        // Astral chars count as 2 UTF-16 units and 4 bytes.
        let emoji = "a😀b\n";
        assert_eq!(position_to_byte_offset(emoji, 0, 0), 0); // a
        assert_eq!(position_to_byte_offset(emoji, 0, 4), 6); // right after b
        // Past the last line clamps to the end.
        assert_eq!(position_to_byte_offset(text, 99, 0), text.len());
    }

    #[test]
    fn format_locations_handles_single_array_and_links() {
        let array = json!([
            { "uri": "file:///tmp/a.rs", "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 9 } } }
        ]);
        let out = format_locations(&array, "definition");
        assert!(out.contains("definition (1 location(s))"), "{out}");
        assert!(out.contains("/tmp/a.rs:3:5"), "{out}");

        let single = json!({ "uri": "file:///tmp/a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 2 } } });
        let out = format_locations(&single, "definition");
        assert!(out.contains(":1:1"), "{out}");

        // LocationLink shape.
        let link = json!([{
            "targetUri": "file:///tmp/b.rs",
            "targetRange": { "start": { "line": 9, "character": 0 }, "end": { "line": 9, "character": 3 } },
            "targetSelectionRange": { "start": { "line": 9, "character": 0 }, "end": { "line": 9, "character": 3 } }
        }]);
        assert!(format_locations(&link, "definition").contains("/tmp/b.rs:10:1"));

        assert!(format_locations(&Value::Null, "references").contains("no locations"));
    }

    #[test]
    fn format_diagnostics_renders_severity_and_position() {
        let params = json!({
            "uri": "file:///tmp/a.rs",
            "diagnostics": [
                { "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 6 } }, "severity": 1, "message": "boom" },
                { "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 1 } }, "severity": 2, "message": "meh" }
            ]
        });
        let out = format_diagnostics(&params, "diagnostics");
        assert!(out.contains("2 diagnostic(s) for /tmp/a.rs"), "{out}");
        assert!(out.contains("/tmp/a.rs:2:3 error: boom"), "{out}");
        assert!(out.contains("/tmp/a.rs:4:1 warning: meh"), "{out}");

        let clean = json!({ "uri": "file:///tmp/a.rs", "diagnostics": [] });
        assert!(format_diagnostics(&clean, "diagnostics").contains("no diagnostics"));
    }

    #[test]
    fn format_symbols_handles_nested_and_flat() {
        let nested = json!([
            {
                "name": "main", "kind": 12, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } },
                "children": [
                    { "name": "inner", "kind": 6, "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 9 } }, "selectionRange": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 9 } } }
                ]
            }
        ]);
        let out = format_symbols(&nested);
        assert!(out.contains("main (Function)"), "{out}");
        assert!(out.contains("  inner (Method)"), "{out}");

        let flat = json!([
            { "name": "helper", "kind": 12, "location": { "uri": "file:///tmp/a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } } }, "containerName": "mod" }
        ]);
        let out = format_symbols(&flat);
        assert!(out.contains("helper — Function"), "{out}");
        assert!(out.contains("(in mod)"), "{out}");
        assert!(format_symbols(&Value::Null).contains("none found"));
    }

    #[test]
    fn format_hover_covers_markup_and_language_strings() {
        let markup = json!({ "contents": { "kind": "markdown", "value": "**doc**" } });
        assert_eq!(format_hover(&markup), "**doc**");
        let language = json!({ "contents": { "language": "rust", "value": "fn x()" } });
        assert_eq!(format_hover(&language), "```rust\nfn x()\n```");
        let array = json!({ "contents": ["plain", { "language": "go", "value": "y" }] });
        let out = format_hover(&array);
        assert!(out.contains("plain") && out.contains("```go"), "{out}");
        assert!(format_hover(&Value::Null).contains("No hover"));
    }

    #[test]
    fn format_code_actions_lists_with_kind_and_diagnostic_count() {
        let actions = json!([
            { "title": "Fix it", "kind": "quickfix", "diagnostics": [{ "message": "x" }] },
            { "title": "Extract", "kind": "refactor.extract" }
        ]);
        let out = format_code_actions(&actions);
        assert!(out.contains("- Fix it [quickfix] (fixes 1 diagnostic(s))"), "{out}");
        assert!(out.contains("- Extract [refactor.extract]"), "{out}");
        assert!(out.contains("apply is deferred"), "{out}");
        assert!(format_code_actions(&Value::Null).contains("none available"));
    }

    #[test]
    fn apply_workspace_edit_applies_text_edits_to_disk() {
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\nfn main() { old(); }\n").unwrap();
        let uri = path_to_uri(&path.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(async {
            let mut changes = serde_json::Map::new();
            changes.insert(
                uri.clone(),
                json!([
                    { "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" },
                    { "range": { "start": { "line": 1, "character": 12 }, "end": { "line": 1, "character": 15 } }, "newText": "new" }
                ]),
            );
            apply_workspace_edit(&json!({ "changes": changes }), &workspace, &[]).await.unwrap()
        });
        assert!(out.contains("2 text edit(s) applied"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn new() {}\nfn main() { new(); }\n"
        );
    }

    #[test]
    fn apply_workspace_edit_rejects_resource_operations() {
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let uri = path_to_uri(&path.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "documentChanges": [
                            { "textDocument": { "uri": uri, "version": 1 }, "edits": [
                                { "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "renamed" }
                            ] },
                            { "kind": "create", "uri": "file:///tmp/other.rs" }
                        ]
                    }),
                    &workspace,
                    &[],
                )
                .await
                .unwrap_err()
                .to_string()
            });
        assert!(
            err.contains("unsupported file create/rename/delete operation"),
            "{err}"
        );
        // The text edit alongside the resource operation must not be applied:
        // the whole edit is rejected atomically.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn apply_workspace_edit_rejects_out_of_workspace_target_atomically() {
        let dir = tmpdir();
        let safe = dir.join("lib.rs");
        let outside = std::env::temp_dir().join(format!(
            "pi-lsp-outside-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&safe, "fn old() {}\n").unwrap();
        std::fs::write(&outside, "fn old() {}\n").unwrap();
        let safe_uri = path_to_uri(&safe.display().to_string()).unwrap().to_string();
        let outside_uri = path_to_uri(&outside.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            safe_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }],
                            outside_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &[],
                )
                .await
                .unwrap_err()
                .to_string()
            });
        assert!(err.contains("outside the configured workspace roots"), "{err}");
        // Mixed safe + unsafe: NOTHING is applied, not even the safe file.
        assert_eq!(
            std::fs::read_to_string(&safe).unwrap(),
            "fn old() {}\n",
            "safe target must be untouched when any target is unsafe"
        );
    }

    #[test]
    fn apply_workspace_edit_rejects_non_file_uri() {
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let uri = path_to_uri(&path.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rt = tokio::runtime::Runtime::new().unwrap();
        for bad in ["http://evil.example/x.rs", "untitled:Untitled-1", "vscode-userdata:///x"] {
            let err = rt
                .block_on(async {
                    apply_workspace_edit(
                        &json!({
                            "changes": {
                                uri.clone(): [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }],
                                bad: [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "newText": "x" }]
                            }
                        }),
                        &workspace,
                        &[],
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                });
            assert!(err.contains("not a file:// uri"), "{bad}: {err}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn apply_workspace_edit_denied_permission_rule_rejects_atomically() {
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let uri = path_to_uri(&path.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rules = vec![PermissionRule {
            action: crate::settings::PermissionRuleAction::Deny,
            path: path.display().to_string(),
            tools: Some(vec![PermissionTool::Lsp]),
            extra: serde_json::Map::new(),
        }];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &rules,
                )
                .await
                .unwrap_err()
                .to_string()
            });
        assert!(err.contains("denied by path permission rule"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn apply_workspace_edit_ask_permission_rule_rejects_atomically() {
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let uri = path_to_uri(&path.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rules = vec![PermissionRule {
            action: crate::settings::PermissionRuleAction::Ask,
            path: path.display().to_string(),
            tools: Some(vec![PermissionTool::Lsp]),
            extra: serde_json::Map::new(),
        }];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &rules,
                )
                .await
                .unwrap_err()
                .to_string()
            });
        assert!(err.contains("requires host approval"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn apply_workspace_edit_allows_in_workspace_multi_file_rename() {
        let dir = tmpdir();
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "fn old() {}\n").unwrap();
        std::fs::write(&b, "fn old() {}\n").unwrap();
        let a_uri = path_to_uri(&a.display().to_string()).unwrap().to_string();
        let b_uri = path_to_uri(&b.display().to_string()).unwrap().to_string();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&dir.to_string_lossy());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            a_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }],
                            b_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &[],
                )
                .await
                .unwrap()
            });
        assert!(
            out.contains("rename: applied 2 text edit(s)"),
            "{out}"
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn new() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "fn new() {}\n");
    }

    #[test]
    fn apply_workspace_edit_respects_additional_workspace_roots() {
        let cwd = tmpdir();
        let additional = std::env::temp_dir().join(format!(
            "pi-lsp-additional-root-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&additional).unwrap();
        let in_additional = additional.join("shared.rs");
        std::fs::write(&in_additional, "fn old() {}\n").unwrap();
        let outside = std::env::temp_dir().join(format!(
            "pi-lsp-not-a-root-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&outside, "fn old() {}\n").unwrap();
        let workspace =
            crate::WorkspaceRoots::new(cwd.as_path(), [&additional]).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();

        // A target inside an additional root is a legitimate multi-root edit.
        let shared_uri = path_to_uri(&in_additional.display().to_string())
            .unwrap()
            .to_string();
        let ok = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            shared_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &[],
                )
                .await
                .unwrap()
            });
        assert!(ok.contains("1 text edit(s) applied"), "{ok}");
        assert_eq!(
            std::fs::read_to_string(&in_additional).unwrap(),
            "fn new() {}\n"
        );

        // A target in a directory that is NOT a configured root is rejected.
        let outside_uri = path_to_uri(&outside.display().to_string()).unwrap().to_string();
        let err = rt
            .block_on(async {
                apply_workspace_edit(
                    &json!({
                        "changes": {
                            outside_uri: [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "new" }]
                        }
                    }),
                    &workspace,
                    &[],
                )
                .await
                .unwrap_err()
                .to_string()
            });
        assert!(err.contains("outside the configured workspace roots"), "{err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "fn old() {}\n");
    }

    fn make_ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "lsp-test".to_owned(),
            arguments: args,
            on_update: std::sync::Arc::new(|_r: AgentToolResult| {}),
            abort,
            model: None,
        }
    }

    /// Builds a command that re-executes this test binary as the fake LSP
    /// server, with `extra` environment overrides.
    fn fake_lsp_server_command(extra: &[(&str, &str)]) -> tokio::process::Command {
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = tokio::process::Command::new(exe);
        command
            .arg("tools::lsp_client::tests::fake_lsp_server_process")
            .arg("--nocapture")
            .env("PI_FAKE_LSP_SERVER", "1");
        for (key, value) in extra {
            command.env(key, value);
        }
        command
    }

    #[tokio::test]
    async fn read_only_instance_rejects_rename_but_serves_hover() {
        let dir = tmpdir();
        std::fs::write(dir.join("sample.rs"), "fn main() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let read_only = lsp_tool_with_capabilities(&cwd, vec![ToolCapability::Read]);

        // rename → refused by the per-action capability gate BEFORE any
        // server contact (fast and deterministic, no server spawned).
        let err = (read_only.execute)(make_ctx(json!({
            "action": "rename", "path": "sample.rs", "line": 0, "character": 3, "new_name": "renamed"
        })))
        .await
        .expect_err("rename must be refused for a read-only instance")
        .to_string();
        assert!(err.contains("requires the write capability"), "{err}");
        assert!(err.contains("rename"), "{err}");

        // hover → the gate lets it through to the server layer: an installed
        // server answers, and a missing binary fails with a server error —
        // never a capability refusal.
        let hover = (read_only.execute)(make_ctx(json!({
            "action": "hover", "path": "sample.rs"
        })))
        .await;
        match hover {
            Ok(_) => {}
            Err(error) => {
                let text = error.to_string();
                assert!(
                    !text.contains("capability"),
                    "hover must not be capability-blocked: {text}"
                );
            }
        }
    }

    #[tokio::test]
    async fn rename_end_to_end_rejects_out_of_workspace_target_atomically() {
        let dir = tmpdir();
        let sentinel = dir.join("lib.rs");
        std::fs::write(&sentinel, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let outside = std::env::temp_dir().join(format!(
            "pi-lsp-e2e-outside-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&outside, "fn old() {}\n").unwrap();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd);

        let targets = format!(
            "{};{}",
            path_to_uri(&sentinel.display().to_string()).unwrap().to_string(),
            path_to_uri(&outside.display().to_string()).unwrap().to_string()
        );
        let command = fake_lsp_server_command(&[("PI_FAKE_LSP_RENAME_TARGETS", &targets)]);
        let workspace_for_client = workspace.clone();
        let sentinel_path = sentinel.display().to_string();
        let err = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let workspace = workspace_for_client.clone();
            let sentinel_path = sentinel_path.clone();
            async move {
                rename_with_client(
                    client,
                    &sentinel_path,
                    "rust",
                    0,
                    0,
                    "renamed",
                    &workspace,
                    &[],
                )
                .await
            }
            .boxed()
        })
        .await
        .expect_err("out-of-workspace target must reject the whole rename")
        .to_string();
        assert!(
            err.contains("outside the configured workspace roots"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "fn old() {}\n",
            "sentinel must be unchanged"
        );
    }

    #[tokio::test]
    async fn rename_end_to_end_applies_in_workspace_multi_file_edit() {
        let dir = tmpdir();
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "fn old() {}\n").unwrap();
        std::fs::write(&b, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd);

        let targets = format!(
            "{};{}",
            path_to_uri(&a.display().to_string()).unwrap().to_string(),
            path_to_uri(&b.display().to_string()).unwrap().to_string()
        );
        let command = fake_lsp_server_command(&[("PI_FAKE_LSP_RENAME_TARGETS", &targets)]);
        let workspace_for_client = workspace.clone();
        let a_path = a.display().to_string();
        let result = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let workspace = workspace_for_client.clone();
            let a_path = a_path.clone();
            async move {
                rename_with_client(
                    client,
                    &a_path,
                    "rust",
                    0,
                    0,
                    "renamed",
                    &workspace,
                    &[],
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("in-workspace multi-file rename must succeed");
        let text = text_of(&result);
        assert!(
            text.contains("rename: applied 2 text edit(s)"),
            "{text}"
        );
        // The fake server replaces characters 0..3 of line 0 ("fn ") with
        // "NEW" in both targets.
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "NEWold() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "NEWold() {}\n");
    }

    #[tokio::test]
    async fn rename_end_to_end_rejects_resource_operations() {
        let dir = tmpdir();
        let sentinel = dir.join("lib.rs");
        std::fs::write(&sentinel, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd);

        let command = fake_lsp_server_command(&[("PI_FAKE_LSP_RENAME_RESOURCE_OP", "1")]);
        let workspace_for_client = workspace.clone();
        let sentinel_path = sentinel.display().to_string();
        let err = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let workspace = workspace_for_client.clone();
            let sentinel_path = sentinel_path.clone();
            async move {
                rename_with_client(
                    client,
                    &sentinel_path,
                    "rust",
                    0,
                    0,
                    "renamed",
                    &workspace,
                    &[],
                )
                .await
            }
            .boxed()
        })
        .await
        .expect_err("resource operations must reject the rename")
        .to_string();
        assert!(
            err.contains("unsupported file create/rename/delete operation"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "fn old() {}\n");
    }

    #[tokio::test]
    async fn rename_end_to_end_rejects_permission_denied_target() {
        let dir = tmpdir();
        let target = dir.join("lib.rs");
        std::fs::write(&target, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd);
        let rules = vec![PermissionRule {
            action: crate::settings::PermissionRuleAction::Deny,
            path: target.display().to_string(),
            tools: Some(vec![PermissionTool::Lsp]),
            extra: serde_json::Map::new(),
        }];

        let target_uri = path_to_uri(&target.display().to_string())
            .unwrap()
            .to_string();
        let command =
            fake_lsp_server_command(&[("PI_FAKE_LSP_RENAME_TARGETS", &target_uri)]);
        let workspace_for_client = workspace.clone();
        let target_path = target.display().to_string();
        let err = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let workspace = workspace_for_client.clone();
            let rules = rules.clone();
            let target_path = target_path.clone();
            async move {
                rename_with_client(
                    client,
                    &target_path,
                    "rust",
                    0,
                    0,
                    "renamed",
                    &workspace,
                    &rules,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect_err("permission-denied target must reject the rename")
        .to_string();
        assert!(err.contains("denied by path permission rule"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn old() {}\n");
    }

    #[tokio::test]
    async fn rename_end_to_end_waits_for_analysis_barrier_in_progressive_mode() {
        // Cold-start stand-in: didOpen publishes an EMPTY diagnostics set and
        // rename is refused (-32602) until a documentSymbol analysis barrier.
        // The readiness-gated rename must pump the barrier first and then
        // apply the workspace edit to every target — the old client, which
        // renamed immediately after didOpen, was refused here.
        let dir = tmpdir();
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "fn old() {}\n").unwrap();
        std::fs::write(&b, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd);

        let targets = format!(
            "{};{}",
            path_to_uri(&a.display().to_string()).unwrap().to_string(),
            path_to_uri(&b.display().to_string()).unwrap().to_string()
        );
        let command = fake_lsp_server_command(&[
            ("PI_FAKE_LSP_PROGRESSIVE", "1"),
            ("PI_FAKE_LSP_RENAME_TARGETS", &targets),
        ]);
        let workspace_for_client = workspace.clone();
        let a_path = a.display().to_string();
        let result = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let workspace = workspace_for_client.clone();
            let a_path = a_path.clone();
            async move {
                rename_with_client(
                    client,
                    &a_path,
                    "rust",
                    0,
                    0,
                    "renamed",
                    &workspace,
                    &[],
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("analysis-gated rename must succeed after the barrier");
        let text = text_of(&result);
        assert!(text.contains("rename: applied 2 text edit(s)"), "{text}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "NEWold() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "NEWold() {}\n");
    }

    #[tokio::test]
    async fn progressive_fake_refuses_rename_before_analysis_barrier() {
        // The progressive fake's analysis gate is deterministic (request-
        // driven, not sleep-based): a rename issued straight after didOpen,
        // before any documentSymbol barrier, is refused with -32602 — exactly
        // what the old client raced into on a cold-starting rust-analyzer.
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let command = fake_lsp_server_command(&[("PI_FAKE_LSP_PROGRESSIVE", "1")]);
        let err = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let path = path.display().to_string();
            async move {
                open_file(client, &path, "rust").await?;
                let uri = path_to_uri(&path)?;
                let params = lsp_types::RenameParams {
                    text_document_position: lsp_types::TextDocumentPositionParams {
                        text_document: lsp_types::TextDocumentIdentifier { uri },
                        position: Position { line: 0, character: 0 },
                    },
                    work_done_progress_params: Default::default(),
                    new_name: "renamed".to_owned(),
                };
                client
                    .request(
                        lsp_types::request::Rename::METHOD,
                        serde_json::to_value(params)?,
                    )
                    .await
            }
            .boxed()
        })
        .await
        .expect_err("rename before the analysis barrier must be refused")
        .to_string();
        assert!(err.contains("-32602"), "{err}");
        assert!(err.contains("refused before analysis barrier"), "{err}");
    }

    #[tokio::test]
    async fn diagnostics_skip_initial_empty_push_via_analysis_barrier() {
        // Progressive fake: the didOpen push is EMPTY and the real diagnostic
        // is published only behind the documentSymbol analysis barrier. The
        // diagnostics path must return the real error, never the stale empty
        // set (the false "no diagnostics" cold-start bug).
        let dir = tmpdir();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "fn main() { let x: u32 = \"oops\"; }\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let command = fake_lsp_server_command(&[("PI_FAKE_LSP_PROGRESSIVE", "1")]);
        let path = path.display().to_string();
        let params = with_server_command(command, "fake-lsp", &cwd, move |client| {
            let path = path.clone();
            async move {
                open_file(client, &path, "rust").await?;
                let uri = path_to_uri(&path)?;
                client.wait_ready_diagnostics(uri.as_str()).await
            }
            .boxed()
        })
        .await
        .expect("diagnostics readiness");
        let text = format_diagnostics(&params, "diagnostics");
        assert!(text.contains("progressive real diagnostic"), "{text}");
        assert!(!text.contains("no diagnostics"), "{text}");
    }

    /// Production-factory path: [`crate::create_all_tools_with_rules`] builds
    /// the lsp tool with the session's live permission-rule source; the tool
    /// re-reads the source on EVERY call, so a live rules change applies to
    /// the SAME instance without rebuilding. The fake server returns a
    /// workspace edit covering the source file AND a different in-workspace
    /// sibling, so the preflight must evaluate the sibling against the
    /// current rules and reject the whole edit atomically when it is denied
    /// or asks.
    #[tokio::test]
    async fn factory_built_lsp_rename_obeys_live_permission_rules() {
        let dir = tmpdir();
        let source_file = dir.join("a.rs");
        let sibling = dir.join("b.rs");
        let write_files = || {
            std::fs::write(&source_file, "fn old() {}\n").unwrap();
            std::fs::write(&sibling, "fn old() {}\n").unwrap();
        };
        write_files();
        let cwd = dir.to_string_lossy().into_owned();

        // Live rules source — the same shape `Session::permission_rules_source`
        // exposes (an Arc closure read fresh on every call).
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::<PermissionRule>::new()));
        let source: crate::PermissionRulesSource = {
            let shared = shared.clone();
            std::sync::Arc::new(move || shared.lock().expect("rules lock").clone())
        };

        // Build through the production Vec factory (the side-chat edit-mode
        // path), NOT the rule-less standalone primitive.
        let tools = crate::create_all_tools_with_rules(&cwd, Some(source));
        let lsp = tools
            .into_iter()
            .find(|tool| tool.name == "lsp")
            .expect("all-tools factory includes lsp");

        // The fake server edits BOTH files (source + different sibling) so the
        // preflight sees a server-derived target beyond the source path.
        let targets = format!(
            "{};{}",
            path_to_uri(&source_file.display().to_string()).unwrap().to_string(),
            path_to_uri(&sibling.display().to_string()).unwrap().to_string(),
        );
        *FAKE_LSP_RENAME_TARGETS.lock().expect("targets lock") = Some(targets);

        let rename_args = || {
            make_ctx(json!({
                "action": "rename", "path": "a.rs", "line": 0, "character": 0,
                "new_name": "renamed", "lang": "fake"
            }))
        };
        let rule = |action: crate::settings::PermissionRuleAction| PermissionRule {
            action,
            path: sibling.display().to_string(),
            tools: Some(vec![PermissionTool::Lsp]),
            extra: serde_json::Map::new(),
        };
        let set_rules = |rules: Vec<PermissionRule>| {
            *shared.lock().expect("rules lock") = rules;
        };

        // Phase 1: the sibling is deny-targeted -> the whole edit is rejected
        // atomically (the allowed source file is NOT written).
        set_rules(vec![rule(crate::settings::PermissionRuleAction::Deny)]);
        let err = (lsp.execute)(rename_args())
            .await
            .expect_err("deny-targeted sibling must reject the rename")
            .to_string();
        assert!(err.contains("denied by path permission rule"), "{err}");
        assert_eq!(std::fs::read_to_string(&source_file).unwrap(), "fn old() {}\n");
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "fn old() {}\n");

        // Phase 2: rules change live (reload semantics) -> the SAME instance
        // now succeeds; both server-derived targets are edited.
        set_rules(Vec::new());
        let result = (lsp.execute)(rename_args())
            .await
            .expect("allowed rename must succeed on the same instance");
        let text = text_of(&result);
        assert!(text.contains("applied 2 text edit(s)"), "{text}");
        assert_eq!(std::fs::read_to_string(&source_file).unwrap(), "NEWold() {}\n");
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "NEWold() {}\n");

        // Phase 3: the sibling is ask-targeted -> rejected without any partial
        // write (the tool has no interactive channel, so Ask cannot be
        // honored from inside the preflight).
        write_files();
        set_rules(vec![rule(crate::settings::PermissionRuleAction::Ask)]);
        let err = (lsp.execute)(rename_args())
            .await
            .expect_err("ask-targeted sibling must reject the rename")
            .to_string();
        assert!(err.contains("requires host approval"), "{err}");
        assert_eq!(std::fs::read_to_string(&source_file).unwrap(), "fn old() {}\n");
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "fn old() {}\n");

        *FAKE_LSP_RENAME_TARGETS.lock().expect("targets lock") = None;
    }

    /// True when `binary` resolves on PATH AND actually executes (guards
    /// against rustup shims that resolve to a toolchain without the binary).
    fn lsp_server_runs(binary: &str) -> bool {
        let Some(path) = find_in_path(binary) else {
            return false;
        };
        std::process::Command::new(&path)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Real rust-analyzer smoke test: spawns the actual server on a tiny
    /// temporary Cargo project and exercises hover/definition/references/
    /// diagnostics/symbols end to end. Skipped when rust-analyzer is not on
    /// `PATH` or does not execute (e.g. a rustup shim for a toolchain that
    /// does not ship it).
    #[tokio::test]
    async fn rust_analyzer_smoke_end_to_end() {
        if !lsp_server_runs("rust-analyzer") {
            eprintln!("SKIP rust_analyzer_smoke_end_to_end: rust-analyzer not usable on PATH");
            return;
        }
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"lsp-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n\nfn main() {\n    let message = greet(\"world\");\n    println!(\"{message}\");\n}\n",
        )
        .unwrap();
        let cwd = dir.to_string_lossy().into_owned();

        // Hover on `greet`'s definition (line 0, character 4 = the name).
        // rust-analyzer may need a moment to analyze the fresh project, so
        // retry with a fresh per-call server until it answers.
        let mut hover_text = String::new();
        for attempt in 0..10 {
            match run_lsp(
                &cwd,
                json!({ "action": "hover", "path": "src/main.rs", "line": 0, "character": 4 }),
                abort(),
            )
            .await
            {
                Ok(result) => {
                    let text = text_of(&result);
                    if text.contains("greet") && !text.contains("No hover") {
                        hover_text = text;
                        break;
                    }
                }
                Err(error) => eprintln!("hover attempt {attempt} failed: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        assert!(
            !hover_text.is_empty(),
            "rust-analyzer hover should return the function signature"
        );

        // Definition from the call site (line 5, character 19).
        let result = run_lsp(
            &cwd,
            json!({ "action": "definition", "path": "src/main.rs", "line": 5, "character": 19 }),
            abort(),
        )
        .await
        .expect("definition action");
        let def_text = text_of(&result);
        assert!(
            def_text.contains("src/main.rs:1:1") || def_text.contains("src/main.rs"),
            "definition should point into main.rs: {def_text}"
        );

        // References from the definition site.
        let result = run_lsp(
            &cwd,
            json!({ "action": "references", "path": "src/main.rs", "line": 0, "character": 4 }),
            abort(),
        )
        .await
        .expect("references action");
        let refs_text = text_of(&result);
        assert!(
            refs_text.contains("2 location(s)"),
            "references should find definition + call site: {refs_text}"
        );

        // Diagnostics on the clean file.
        let result = run_lsp(
            &cwd,
            json!({ "action": "diagnostics", "path": "src/main.rs" }),
            abort(),
        )
        .await
        .expect("diagnostics action");
        let diag_text = text_of(&result);
        assert!(
            diag_text.contains("no diagnostics"),
            "clean file should report no diagnostics: {diag_text}"
        );

        // Symbols for the document.
        let result = run_lsp(
            &cwd,
            json!({ "action": "symbols", "path": "src/main.rs" }),
            abort(),
        )
        .await
        .expect("symbols action");
        let sym_text = text_of(&result);
        assert!(
            sym_text.contains("greet") && sym_text.contains("main"),
            "symbols should list greet and main: {sym_text}"
        );

        // Capabilities via a real initialize handshake.
        let result = run_lsp(
            &cwd,
            json!({ "action": "capabilities", "path": "src/main.rs" }),
            abort(),
        )
        .await
        .expect("capabilities action");
        let caps_text = text_of(&result);
        assert!(
            caps_text.contains("capabilities") && caps_text.contains("hoverProvider"),
            "capabilities should reflect the server handshake: {caps_text}"
        );
    }
}
