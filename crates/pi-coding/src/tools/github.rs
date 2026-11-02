//! `github` tool: GitHub API access via the governed `gh` CLI or captured
//! session HTTP authority.
//!
//! Auth decision: prefer the governed `gh` CLI. Credentials live in the
//! user's own gh config (`~/.config/gh/hosts.yml`), read by gh itself — rpi
//! never touches, stores, or prints a token. Requests are spawned as `gh api`
//! with a fully argv-built request (no shell interpolation), mirroring the
//! gist-share module. When `gh` is unavailable, only a captured
//! session-generation HTTP executor may access the exact `api.github.com`
//! authority; the request is unauthenticated. Standalone factories fail
//! closed rather than opening ambient network access.
//!
//! Actions (read-only plus issue create/comment): `search_issues`,
//! `get_issue`, `list_issues`, `create_issue`, `comment_issue`, `list_prs`,
//! `get_pr`, `list_commits`, `view_file`, `search_code`. GitHub JSON is
//! parsed into bounded plain text (`title+number+state+url` blocks for
//! issues/PRs, `file:line:snippet` lines for code search, raw content for
//! `view_file`), truncated to [`OUTPUT_MAX_BYTES`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use serde_json::{Map, Value};

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};
use pi_ai::Schema;

use crate::application::generation::ApplicationGeneration;
use crate::process::{
    GovernedProcessContext, GovernedProcessSpec, TrustedToolProbe, run_trusted_tool_probe,
};
use crate::redact::redact_secrets;
use crate::truncate::truncate_head;

use super::{arg_int, arg_str, check_aborted, s_number, s_object, s_string, text_result};

/// The supported actions, in the order they appear in the tool schema.
const ACTIONS: &[&str] = &[
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
];

/// Hard cap on list items rendered from JSON arrays.
const LIST_ITEM_CAP: usize = 20;
/// Per-call timeout (covers connect + full body) for both backends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Output byte budget for the rendered tool text.
const OUTPUT_MAX_BYTES: usize = 32 * 1024;
/// Byte cap on `gh api` stdout (base64 file content and search payloads).
const GH_STDOUT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Byte cap on `gh api` stderr (error hints only).
const GH_STDERR_MAX_BYTES: usize = 16 * 1024;
/// Cap on error-hint text length embedded in error messages.
const HINT_MAX_CHARS: usize = 400;
const USER_AGENT: &str = concat!("rpi/", env!("CARGO_PKG_VERSION"));

/// HTTP method for a GitHub API request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhMethod {
    Get,
    Post,
}

/// A fully-validated GitHub API request shared by the governed CLI and
/// captured HTTP backends. `query` holds GET query parameters; `body` holds
/// the JSON body for POST actions; `headers` holds extra request headers.
#[derive(Debug, Clone, PartialEq)]
struct GitHubRequest {
    method: GhMethod,
    endpoint: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

/// Builds a standalone schema-compatible tool that fails closed at execution.
pub(crate) fn github_tool() -> AgentTool {
    let action_schema = schema_with_enum(
        "GitHub API action to perform",
        ACTIONS.iter().map(|a| (*a).to_string()).collect(),
    );
    let state_schema = schema_with_enum(
        "Filter by state (list_issues, list_prs)",
        ["open", "closed", "all"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    );
    let params = s_object(
        vec![
            ("action", action_schema),
            (
                "repo",
                s_string(
                    "Repository as owner/name (required for get_issue, list_issues, create_issue, \
                     comment_issue, list_prs, get_pr, list_commits, view_file)",
                ),
            ),
            (
                "query",
                s_string(
                    "Search query (required for search_issues and search_code; supports GitHub \
                     qualifiers like repo:owner/name, is:issue, is:pr)",
                ),
            ),
            (
                "number",
                s_number(
                    "Issue or pull request number (required for get_issue, comment_issue, get_pr)",
                ),
            ),
            ("title", s_string("Issue title (required for create_issue)")),
            (
                "body",
                s_string(
                    "Issue body / comment body (required for comment_issue; optional for create_issue)",
                ),
            ),
            (
                "path",
                s_string("File path within the repository (required for view_file)"),
            ),
            ("state", state_schema),
            (
                "ref",
                s_string("Git ref: branch, tag, or SHA (optional for list_commits and view_file)"),
            ),
        ],
        vec!["action"],
    );
    let description = format!(
        "GitHub API access via the governed gh CLI (preferred) or a captured session HTTP \
         executor restricted to api.github.com. Actions: search_issues, get_issue, list_issues, \
         create_issue, comment_issue, list_prs, get_pr, list_commits, view_file, search_code. \
         Output is bounded to {}KB of plain text.",
        OUTPUT_MAX_BYTES / 1024
    );
    AgentTool::new("github", description, params, move |ctx| async move {
        run_github(ctx.arguments, ctx.abort).await
    })
    .with_capability(ToolCapability::Write)
}

pub(crate) fn github_tool_with_generation(capture: crate::tools::GenerationCaptureFn) -> AgentTool {
    let mut tool = github_tool();
    tool.execute = Arc::new(move |ctx| {
        let capture = capture.clone();
        Box::pin(async move {
            let generation = capture()?;
            run_github_with_generation(generation, ctx.arguments, ctx.abort).await
        })
    });
    tool
}

async fn run_github_with_generation(
    generation: Arc<ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action");
    let request = build_request(&action, &args)?;
    let body = execute_with_generation(generation, &request, &abort).await?;
    check_aborted(&abort)?;
    let output = render(&action, &body);
    let tr = truncate_head(&output, usize::MAX, OUTPUT_MAX_BYTES);
    Ok(text_result(tr.content))
}

/// Builds a string schema with an `enum` constraint (mirrors the todo tool's
/// inline Schema construction).
fn schema_with_enum(description: &str, values: Vec<String>) -> Schema {
    Schema {
        schema_type: Some(Value::String("string".into())),
        description: Some(description.to_string()),
        enum_values: values.into_iter().map(Value::String).collect(),
        ..Default::default()
    }
}

/// Standalone entry point: validates the action, then fails closed because no
/// application generation has been captured.
pub(crate) async fn run_github(args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let _ = build_request(&arg_str(&args, "action"), &args)?;
    Err(anyhow!("GitHub tool requires an application generation"))
}

// ---------------------------------------------------------------------------
// Request construction (pure; unit-tested without network)
// ---------------------------------------------------------------------------

/// Validates `action` and the action-specific parameters, returning the API
/// request both backends can execute.
fn build_request(action: &str, args: &Value) -> Result<GitHubRequest> {
    let repo = || -> Result<String> {
        let repo = arg_str(args, "repo");
        validate_repo(&repo)?;
        Ok(repo)
    };
    let number = || -> Result<i64> {
        let number = arg_int(args, "number")?
            .ok_or_else(|| anyhow!("number is required for action {action}"))?;
        if number <= 0 {
            return Err(anyhow!("number must be a positive integer"));
        }
        Ok(number)
    };
    let non_empty = |key: &str| -> Result<String> {
        let value = arg_str(args, key);
        if value.trim().is_empty() {
            return Err(anyhow!("{key} is required for action {action}"));
        }
        Ok(value)
    };
    let optional_state = || -> Result<Option<String>> {
        let state = arg_str(args, "state");
        if state.is_empty() {
            return Ok(None);
        }
        if !matches!(state.as_str(), "open" | "closed" | "all") {
            return Err(anyhow!("state must be one of open, closed, all"));
        }
        Ok(Some(state))
    };
    let optional_ref = || -> Result<Option<String>> {
        let reference = arg_str(args, "ref");
        if reference.is_empty() {
            Ok(None)
        } else {
            Ok(Some(reference))
        }
    };
    let get = |endpoint: String, query: Vec<(String, String)>| GitHubRequest {
        method: GhMethod::Get,
        endpoint,
        query,
        headers: Vec::new(),
        body: None,
    };

    match action {
        "search_issues" => Ok(get(
            "/search/issues".to_string(),
            vec![("q".to_string(), non_empty("query")?)],
        )),
        "get_issue" => Ok(get(
            format!("/repos/{}/issues/{}", repo()?, number()?),
            Vec::new(),
        )),
        "list_issues" => {
            let mut query = Vec::new();
            if let Some(state) = optional_state()? {
                query.push(("state".to_string(), state));
            }
            Ok(get(format!("/repos/{}/issues", repo()?), query))
        }
        "create_issue" => {
            let mut body = Map::new();
            body.insert("title".to_string(), Value::String(non_empty("title")?));
            let body_text = arg_str(args, "body");
            if !body_text.is_empty() {
                body.insert("body".to_string(), Value::String(body_text));
            }
            Ok(GitHubRequest {
                method: GhMethod::Post,
                endpoint: format!("/repos/{}/issues", repo()?),
                query: Vec::new(),
                headers: Vec::new(),
                body: Some(Value::Object(body)),
            })
        }
        "comment_issue" => {
            let mut body = Map::new();
            body.insert("body".to_string(), Value::String(non_empty("body")?));
            Ok(GitHubRequest {
                method: GhMethod::Post,
                endpoint: format!("/repos/{}/issues/{}/comments", repo()?, number()?),
                query: Vec::new(),
                headers: Vec::new(),
                body: Some(Value::Object(body)),
            })
        }
        "list_prs" => {
            let mut query = Vec::new();
            if let Some(state) = optional_state()? {
                query.push(("state".to_string(), state));
            }
            Ok(get(format!("/repos/{}/pulls", repo()?), query))
        }
        "get_pr" => Ok(get(
            format!("/repos/{}/pulls/{}", repo()?, number()?),
            Vec::new(),
        )),
        "list_commits" => {
            let mut query = Vec::new();
            if let Some(reference) = optional_ref()? {
                query.push(("sha".to_string(), reference));
            }
            Ok(get(format!("/repos/{}/commits", repo()?), query))
        }
        "view_file" => {
            let path = non_empty("path")?;
            let mut query = Vec::new();
            if let Some(reference) = optional_ref()? {
                query.push(("ref".to_string(), reference));
            }
            Ok(get(
                format!(
                    "/repos/{}/contents/{}",
                    repo()?,
                    encode_contents_path(&path)
                ),
                query,
            ))
        }
        "search_code" => Ok(GitHubRequest {
            method: GhMethod::Get,
            endpoint: "/search/code".to_string(),
            query: vec![("q".to_string(), non_empty("query")?)],
            headers: vec![(
                "Accept".to_string(),
                "application/vnd.github.text-match+json".to_string(),
            )],
            body: None,
        }),
        other => Err(anyhow!(
            "unknown action {other:?}; expected one of {}",
            ACTIONS.join(", ")
        )),
    }
}

/// `repo` must be exactly `owner/name` with non-empty, whitespace-free parts.
fn validate_repo(repo: &str) -> Result<()> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(anyhow!("repo must be in owner/name format (got {repo:?})"));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || owner.chars().any(char::is_whitespace)
        || name.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("repo must be in owner/name format (got {repo:?})"));
    }
    Ok(())
}

/// Percent-encodes one path segment (RFC 3986): unreserved chars pass
/// through, everything else becomes %XX.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Encodes a `view_file` path for the contents endpoint: each segment is
/// percent-encoded, `/` separators are kept literal (GitHub accepts both,
/// literal slashes keep the endpoint readable in argv).
fn encode_contents_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Builds the `gh api` argv (excluding the program name). GET query
/// parameters and POST bodies both use `-f key=value`; `-H` passes extra
/// headers. The method is always passed explicitly: `gh api` silently
/// switches to POST when `-f` fields are present, so GET queries require
/// `--method GET` (per `gh api --help`).
fn gh_api_args(request: &GitHubRequest) -> Vec<String> {
    let mut args = vec!["api".to_string()];
    match request.method {
        GhMethod::Get => {
            args.push("--method".to_string());
            args.push("GET".to_string());
        }
        GhMethod::Post => {
            args.push("--method".to_string());
            args.push("POST".to_string());
        }
    }
    for (key, value) in &request.headers {
        args.push("-H".to_string());
        args.push(format!("{key}: {value}"));
    }
    args.push(request.endpoint.clone());
    for (key, value) in &request.query {
        args.push("-f".to_string());
        args.push(format!("{key}={value}"));
    }
    if let Some(body) = &request.body {
        if let Some(object) = body.as_object() {
            for (key, value) in object {
                args.push("-f".to_string());
                let value = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                args.push(format!("{key}={value}"));
            }
        }
    }
    args
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Outcome of a `gh api` spawn.
#[derive(Debug)]
enum GhOutcome {
    /// `gh` exited 0 and stdout parsed as JSON.
    Ok(Value),
    /// `gh` ran but exited nonzero; carries a redacted, bounded hint.
    Failed(String),
}

/// Runs `gh api` with the request, honoring the abort signal and a per-call
/// timeout. `gh` is injectable so tests can point at a fake executable.
async fn run_gh_api_with(
    generation: Arc<ApplicationGeneration>,
    gh: &Path,
    request: &GitHubRequest,
    abort: &AbortSignal,
) -> Result<GhOutcome> {
    let args = gh_api_args(request);
    let cwd = gh
        .parent()
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| Path::new("/"));
    let output = GovernedProcessContext::new(generation, None)
        .run_with_abort(
            pi_sandbox::SpawnFamily::GithubCli,
            GovernedProcessSpec {
                program: gh.to_path_buf(),
                args,
                cwd: cwd.to_path_buf(),
                environment: BTreeMap::new(),
                timeout: REQUEST_TIMEOUT,
                stdout_bytes: GH_STDOUT_MAX_BYTES,
                stderr_bytes: GH_STDERR_MAX_BYTES,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
            Some(abort.clone()),
        )
        .await?;
    if output.timed_out {
        return Err(anyhow!(
            "{} api timed out after {}s",
            gh.display(),
            REQUEST_TIMEOUT.as_secs()
        ));
    }
    if output.cancelled {
        return Err(anyhow!("Operation aborted"));
    }
    let code = output.status.unwrap_or(1);
    if code == 0 {
        let text = String::from_utf8_lossy(&output.stdout);
        let trimmed = text.trim();
        let value = serde_json::from_str(trimmed).map_err(|e| {
            anyhow!(
                "{} api returned non-JSON output: {e}: {}",
                gh.display(),
                redact_secrets(&hint(trimmed, HINT_MAX_CHARS))
            )
        })?;
        return Ok(GhOutcome::Ok(value));
    }
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let api_message = serde_json::from_str::<Value>(stdout_text.trim())
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
        });
    let hint_text = api_message
        .or_else(|| {
            let text = stderr_text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        })
        .unwrap_or_default();
    let detail = hint(&hint_text, HINT_MAX_CHARS);
    Ok(GhOutcome::Failed(format!(
        "exit code {code}{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", redact_secrets(&detail))
        }
    )))
}

async fn execute_with_generation(
    generation: Arc<ApplicationGeneration>,
    request: &GitHubRequest,
    abort: &AbortSignal,
) -> Result<Value> {
    if let Ok(gh) = run_trusted_tool_probe(&generation, TrustedToolProbe::GithubCli) {
        match run_gh_api_with(generation.clone(), gh.as_path(), request, abort).await? {
            GhOutcome::Ok(value) => return Ok(value),
            GhOutcome::Failed(hint) => return Err(anyhow!("gh api failed: {hint}")),
        }
    }

    let executor = generation
        .sandbox()
        .http()
        .executor(pi_sandbox::HostHttpClass::GithubApi);
    run_http_executor(request, abort, executor).await
}

async fn run_http_executor(
    request: &GitHubRequest,
    abort: &AbortSignal,
    executor: pi_ai::SharedHttpExecutor,
) -> Result<Value> {
    let mut url = url::Url::parse(&format!("https://api.github.com{}", request.endpoint))?;
    if !request.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&request.query);
    }
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/vnd.github+json"),
    );
    for (name, value) in &request.headers {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes())?,
            http::HeaderValue::from_str(value)?,
        );
    }
    let body = request
        .body
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()?
        .unwrap_or_default();
    if !body.is_empty() {
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
    }
    let method = match request.method {
        GhMethod::Get => pi_ai::HttpMethod::Get,
        GhMethod::Post => pi_ai::HttpMethod::Post,
    };
    let request = pi_ai::request_from_url(
        method,
        url.as_str(),
        &headers,
        body,
        &pi_ai::StreamOptions {
            timeout_ms: Some(REQUEST_TIMEOUT.as_millis() as u64),
            abort_signal: Some(abort.cancellation_token()),
            ..Default::default()
        },
        GH_STDOUT_MAX_BYTES as u64,
    )?;
    let response = executor.execute(request).await.map_err(|error| {
        anyhow!(
            "GitHub API request failed: {}",
            redact_secrets(&error.to_string())
        )
    })?;
    let status = response.status();
    let bytes = pi_ai::collect_response_body(response)
        .await
        .map_err(|error| {
            anyhow!(
                "GitHub API response failed: {}",
                redact_secrets(&error.to_string())
            )
        })?;
    let text = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| text.trim().to_owned());
        return Err(anyhow!(
            "GitHub API error (HTTP {status}): {}",
            redact_secrets(&hint(&message, HINT_MAX_CHARS))
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!("GitHub API response parse failed: {error}"))
}

// ---------------------------------------------------------------------------
// Rendering (pure; unit-tested without network)
// ---------------------------------------------------------------------------

/// First `max` chars of `s` for error hints.
fn hint(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Renders the parsed GitHub JSON for `action` as bounded plain text.
fn render(action: &str, body: &Value) -> String {
    match action {
        // Search results are whatever the user's query asked for (including
        // `is:pr`), so render them unfiltered.
        "search_issues" => render_issue_search(items_of(body.get("items")), false),
        "list_issues" => render_issue_search(items_of(Some(body)), true),
        "list_prs" => render_pr_list(items_of(Some(body))),
        "get_issue" | "get_pr" => render_detail(body),
        "list_commits" => render_commits(items_of(Some(body))),
        "view_file" => render_contents(body),
        "search_code" => render_code_hits(body),
        other => format!("(unsupported action: {other})"),
    }
}

fn items_of(value: Option<&Value>) -> &[Value] {
    value
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// `#<number> <title>` plus a `state: <state> | <html_url>` line. Shared by
/// issues and PRs (both carry number/title/state/html_url).
fn render_issue_block(item: &Value) -> String {
    let number = item.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
    let title = field(item, "title");
    let state = field(item, "state");
    let url = field(item, "html_url");
    let mut meta = format!("state: {state}");
    if !url.is_empty() {
        meta.push_str(" | ");
        meta.push_str(url);
    }
    format!("#{number} {title}\n{meta}")
}

/// Renders issue results as `#number title / state: … | url` blocks.
/// `filter_prs` drops pull requests (the issues endpoint mixes them in
/// without the caller asking); search results are rendered unfiltered.
fn render_issue_search(items: &[Value], filter_prs: bool) -> String {
    let mut blocks = Vec::new();
    for item in items {
        if blocks.len() >= LIST_ITEM_CAP {
            break;
        }
        if filter_prs && item.get("pull_request").is_some() {
            continue;
        }
        blocks.push(render_issue_block(item));
    }
    if blocks.is_empty() {
        return "No results".to_string();
    }
    blocks.join("\n---\n")
}

fn render_pr_list(items: &[Value]) -> String {
    let mut blocks = Vec::new();
    for item in items.iter().take(LIST_ITEM_CAP) {
        blocks.push(render_issue_block(item));
    }
    if blocks.is_empty() {
        return "No results".to_string();
    }
    blocks.join("\n---\n")
}

/// Single-issue/PR detail: the block plus the body (bounded by the caller's
/// truncation).
fn render_detail(item: &Value) -> String {
    let mut out = render_issue_block(item);
    let body = field(item, "body");
    if !body.is_empty() {
        out.push_str("\n\n");
        out.push_str(body);
    }
    out
}

fn render_commits(items: &[Value]) -> String {
    let mut lines = Vec::new();
    for item in items.iter().take(LIST_ITEM_CAP) {
        let sha = field(item, "sha");
        let short: String = sha.chars().take(7).collect();
        let message = item
            .get("commit")
            .and_then(|c| c.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let first_line = message.lines().next().unwrap_or("");
        lines.push(format!("{short} {first_line}"));
    }
    if lines.is_empty() {
        return "No results".to_string();
    }
    lines.join("\n")
}

/// `view_file` result: raw file content for files (base64-decoded from the
/// contents endpoint), a directory listing for directories.
fn render_contents(body: &Value) -> String {
    if let Some(items) = body.as_array() {
        let mut lines = Vec::new();
        for item in items.iter().take(LIST_ITEM_CAP) {
            let name = field(item, "name");
            if field(item, "type") == "dir" {
                lines.push(format!("{name}/"));
            } else {
                lines.push(name.to_string());
            }
        }
        if lines.is_empty() {
            return "No results".to_string();
        }
        return lines.join("\n");
    }
    if body.get("encoding").and_then(|e| e.as_str()) == Some("base64") {
        let content = field(body, "content");
        // GitHub wraps the base64 with newlines (and JSON escapes them);
        // strip ASCII whitespace before decoding.
        let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
        return match base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => format!(
                "(could not decode base64 content for {})",
                field(body, "path")
            ),
        };
    }
    if field(body, "type") == "submodule" {
        return format!(
            "submodule {} -> {}",
            field(body, "name"),
            field(body, "sha")
        );
    }
    format!(
        "(unsupported contents response type: {})",
        field(body, "type")
    )
}

/// Code-search hits as `repo:path` headers with best-effort `line: snippet`
/// lines derived from the text-match fragments.
fn render_code_hits(body: &Value) -> String {
    let items = items_of(body.get("items"));
    if items.is_empty() {
        return "No results".to_string();
    }
    let mut blocks = Vec::new();
    for item in items.iter().take(LIST_ITEM_CAP) {
        let path = field(item, "path");
        let repo = item
            .get("repository")
            .and_then(|r| r.get("full_name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let mut block = if repo.is_empty() {
            path.to_string()
        } else {
            format!("{repo}:{path}")
        };
        if let Some(matches) = item.get("text_matches").and_then(|m| m.as_array()) {
            for text_match in matches.iter().take(2) {
                if let Some((line, snippet)) = fragment_line(text_match) {
                    block.push_str(&format!("\n  {line}: {snippet}"));
                }
            }
        }
        blocks.push(block);
    }
    blocks.join("\n")
}

/// Best-effort `(line, snippet)` from a code-search text match fragment. The
/// search API does not return line numbers, so the line is derived from the
/// fragment text before the first match index (character-offset safe).
fn fragment_line(text_match: &Value) -> Option<(usize, String)> {
    let fragment = field(text_match, "fragment");
    if fragment.is_empty() {
        return None;
    }
    let start = text_match
        .get("matches")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.first())
        .and_then(|m| m.get("indices"))
        .and_then(|i| i.as_array())
        .and_then(|i| i.first())
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let prefix: String = fragment.chars().take(start).collect();
    let line = prefix.matches('\n').count() + 1;
    let rest: String = fragment.chars().skip(start).collect();
    let snippet = rest.split('\n').next().unwrap_or("").trim();
    if snippet.is_empty() {
        None
    } else {
        Some((line, hint(snippet, 200)))
    }
}

// ---------------------------------------------------------------------------
// Secret redaction (defense in depth; tokens are never placed in URLs)
// ---------------------------------------------------------------------------
//
// All error hints surfaced to the model pass through
// `crate::redact::redact_secrets` (shared with the memory store and the
// MCP/LSP client error paths) so a token can never leak through stderr or
// API error text.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(args: Value) -> pi_agent::ToolCallContext {
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        pi_agent::ToolCallContext {
            tool_call_id: "github-test".to_string(),
            arguments: args,
            on_update: std::sync::Arc::new(|_r: AgentToolResult| {}),
            abort,
            model: None,
            approval_token: None,
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    struct RecordingExecutor {
        request: std::sync::Mutex<Option<pi_ai::HttpRequestParts>>,
        status: http::StatusCode,
        body: Vec<u8>,
    }

    impl RecordingExecutor {
        fn new(status: http::StatusCode, body: Value) -> Arc<Self> {
            Arc::new(Self {
                request: std::sync::Mutex::new(None),
                status,
                body: serde_json::to_vec(&body).expect("response JSON"),
            })
        }

        fn take_request(&self) -> pi_ai::HttpRequestParts {
            self.request
                .lock()
                .expect("GitHub request mutex poisoned")
                .take()
                .expect("GitHub request was captured")
        }
    }

    impl pi_ai::HttpExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, Result<pi_ai::HttpResponse>> {
            use futures_util::FutureExt as _;

            async move {
                *self.request.lock().expect("GitHub request mutex poisoned") =
                    Some(request.into_parts());
                Ok(pi_ai::HttpResponse::new(
                    self.status,
                    http::HeaderMap::new(),
                    Box::pin(futures_util::stream::iter([Ok(self.body.clone())])),
                ))
            }
            .boxed()
        }
    }

    // ------------------------------------------------------------------
    // Schema / validation
    // ------------------------------------------------------------------

    #[test]
    fn schema_requires_action_and_enumerates_actions() {
        let tool = github_tool();
        assert!(tool.parameters.required.contains(&"action".to_string()));
        let action = tool
            .parameters
            .properties
            .get("action")
            .expect("action prop");
        let values: Vec<&str> = action
            .enum_values
            .iter()
            .map(|v| v.as_str().unwrap_or(""))
            .collect();
        assert_eq!(values, ACTIONS);
        assert!(
            tool.parameters
                .validate(&json!({ "action": "list_issues" }))
                .is_ok()
        );
        assert!(
            tool.parameters
                .validate(&json!({ "action": "nope" }))
                .is_err()
        );
    }

    #[test]
    fn build_request_rejects_bad_params_per_action() {
        // Unknown action.
        assert!(build_request("explode", &json!({})).is_err());
        // Missing repo.
        let err = build_request("get_issue", &json!({ "number": 1 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("owner/name"), "{err}");
        for bad in ["norepo", "owner/", "/name", "a/b/c", "own er/name"] {
            let err = build_request("get_issue", &json!({ "repo": bad, "number": 1 }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("owner/name"), "{bad}: {err}");
        }
        // Missing / non-positive number.
        let err = build_request("get_issue", &json!({ "repo": "o/r" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("number is required"), "{err}");
        let err = build_request("get_issue", &json!({ "repo": "o/r", "number": 0 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("positive"), "{err}");
        // Missing query.
        let err = build_request("search_issues", &json!({ "query": "  " }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("query is required"), "{err}");
        // Bad state.
        let err = build_request("list_issues", &json!({ "repo": "o/r", "state": "merged" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("open, closed, all"), "{err}");
        // Missing title/body/path.
        let err = build_request("create_issue", &json!({ "repo": "o/r" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("title is required"), "{err}");
        let err = build_request("comment_issue", &json!({ "repo": "o/r", "number": 1 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("body is required"), "{err}");
        let err = build_request("view_file", &json!({ "repo": "o/r" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("path is required"), "{err}");
    }

    #[test]
    fn build_request_constructs_each_action() {
        let cases = [
            (
                "search_issues",
                json!({ "query": "repo:octocat/Hello-World is:issue" }),
                "GET /search/issues",
            ),
            (
                "get_issue",
                json!({ "repo": "octocat/Hello-World", "number": 1 }),
                "GET /repos/octocat/Hello-World/issues/1",
            ),
            (
                "list_issues",
                json!({ "repo": "octocat/Hello-World", "state": "closed" }),
                "GET /repos/octocat/Hello-World/issues",
            ),
            (
                "list_prs",
                json!({ "repo": "octocat/Hello-World" }),
                "GET /repos/octocat/Hello-World/pulls",
            ),
            (
                "get_pr",
                json!({ "repo": "octocat/Hello-World", "number": 3 }),
                "GET /repos/octocat/Hello-World/pulls/3",
            ),
            (
                "list_commits",
                json!({ "repo": "octocat/Hello-World", "ref": "master" }),
                "GET /repos/octocat/Hello-World/commits",
            ),
            (
                "view_file",
                json!({ "repo": "octocat/Hello-World", "path": "src/main.rs", "ref": "main" }),
                "GET /repos/octocat/Hello-World/contents/src/main.rs",
            ),
            (
                "search_code",
                json!({ "query": "repo:octocat/Hello-World fn" }),
                "GET /search/code",
            ),
        ];
        for (action, args, expected) in cases {
            let req = build_request(action, &args).unwrap_or_else(|e| panic!("{action}: {e}"));
            let method = if req.method == GhMethod::Post {
                "POST"
            } else {
                "GET"
            };
            assert_eq!(format!("{method} {}", req.endpoint), expected, "{action}");
        }

        // POST bodies carry title/body; missing body is omitted.
        let create = build_request(
            "create_issue",
            &json!({ "repo": "o/r", "title": "hello", "body": "world" }),
        )
        .unwrap();
        assert_eq!(create.method, GhMethod::Post);
        assert_eq!(create.endpoint, "/repos/o/r/issues");
        assert_eq!(
            create.body,
            Some(json!({ "title": "hello", "body": "world" }))
        );

        let create_no_body =
            build_request("create_issue", &json!({ "repo": "o/r", "title": "hello" })).unwrap();
        assert_eq!(create_no_body.body, Some(json!({ "title": "hello" })));

        let comment = build_request(
            "comment_issue",
            &json!({ "repo": "o/r", "number": 5, "body": "thanks" }),
        )
        .unwrap();
        assert_eq!(comment.method, GhMethod::Post);
        assert_eq!(comment.endpoint, "/repos/o/r/issues/5/comments");
        assert_eq!(comment.body, Some(json!({ "body": "thanks" })));

        // Query params: state / ref map to the right keys.
        let list_issues =
            build_request("list_issues", &json!({ "repo": "o/r", "state": "all" })).unwrap();
        assert_eq!(
            list_issues.query,
            vec![("state".to_string(), "all".to_string())]
        );
        let commits = build_request(
            "list_commits",
            &json!({ "repo": "o/r", "ref": "feature/x" }),
        )
        .unwrap();
        assert_eq!(
            commits.query,
            vec![("sha".to_string(), "feature/x".to_string())]
        );
        let view = build_request(
            "view_file",
            &json!({ "repo": "o/r", "path": "a b/c.txt", "ref": "main" }),
        )
        .unwrap();
        assert_eq!(view.endpoint, "/repos/o/r/contents/a%20b/c.txt");
        assert_eq!(view.query, vec![("ref".to_string(), "main".to_string())]);

        // Code search requests the text-match Accept header.
        let code = build_request("search_code", &json!({ "query": "fn main" })).unwrap();
        assert!(
            code.headers
                .iter()
                .any(|(k, v)| k == "Accept" && v.contains("text-match"))
        );
    }

    #[test]
    fn gh_api_args_renders_argv_without_shell_interpolation() {
        let search =
            build_request("search_issues", &json!({ "query": "repo:o/r is:issue" })).unwrap();
        assert_eq!(
            gh_api_args(&search),
            vec![
                "api",
                "--method",
                "GET",
                "/search/issues",
                "-f",
                "q=repo:o/r is:issue"
            ]
        );

        let create = build_request(
            "create_issue",
            &json!({ "repo": "o/r", "title": "t", "body": "multi\nline" }),
        )
        .unwrap();
        // JSON object fields serialize in BTreeMap (alphabetical) order;
        // field order is irrelevant to GitHub.
        assert_eq!(
            gh_api_args(&create),
            vec![
                "api",
                "--method",
                "POST",
                "/repos/o/r/issues",
                "-f",
                "body=multi\nline",
                "-f",
                "title=t"
            ]
        );

        let view = build_request(
            "view_file",
            &json!({ "repo": "o/r", "path": "dir/file.rs", "ref": "main" }),
        )
        .unwrap();
        assert_eq!(
            gh_api_args(&view),
            vec![
                "api",
                "--method",
                "GET",
                "/repos/o/r/contents/dir/file.rs",
                "-f",
                "ref=main"
            ]
        );
    }
    #[tokio::test]
    async fn generation_tool_captures_before_execution() {
        let capture: crate::tools::GenerationCaptureFn =
            Arc::new(|| Err(anyhow!("captured generation marker")));
        let tool = github_tool_with_generation(capture);
        let error = (tool.execute)(ctx(json!({
            "action": "search_issues",
            "query": "q"
        })))
        .await
        .expect_err("generation capture must run before GitHub execution");
        assert_eq!(error.to_string(), "captured generation marker");
    }

    #[test]
    fn governed_github_family_is_exact() {
        assert_eq!(pi_sandbox::SpawnFamily::GithubCli.as_str(), "github-cli");
    }
    #[tokio::test]
    async fn pre_aborted_signal_short_circuits_execution() {
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let err = run_github(json!({ "action": "search_issues", "query": "q" }), abort)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Operation aborted");
    }

    #[tokio::test]
    async fn standalone_execution_fails_closed_after_validation() {
        let error = run_github(
            json!({ "action": "search_issues", "query": "repo:o/r is:issue" }),
            pi_agent::AbortSignal::none(),
        )
        .await
        .expect_err("standalone GitHub execution must not acquire network authority")
        .to_string();
        assert_eq!(error, "GitHub tool requires an application generation");
    }

    #[tokio::test]
    async fn injected_executor_receives_exact_bounded_github_request() {
        let executor = RecordingExecutor::new(http::StatusCode::OK, json!({ "items": [] }));
        let request =
            build_request("search_code", &json!({ "query": "fn main repo:o/r" })).unwrap();
        let body = run_http_executor(&request, &pi_agent::AbortSignal::none(), executor.clone())
            .await
            .expect("injected GitHub executor succeeds");
        assert_eq!(body, json!({ "items": [] }));

        let captured = executor.take_request();
        assert_eq!(captured.method, pi_ai::HttpMethod::Get);
        assert_eq!(
            captured.authority.scheme(),
            pi_ai::ResolvedHttpScheme::Https
        );
        assert_eq!(
            captured.authority.host(),
            &pi_ai::ResolvedHttpHost::Dns("api.github.com".to_owned())
        );
        assert_eq!(captured.authority.port(), 443);
        assert_eq!(
            captured.path_and_query,
            "/search/code?q=fn+main+repo%3Ao%2Fr"
        );
        assert_eq!(captured.timeout_ms, REQUEST_TIMEOUT.as_millis() as u64);
        assert_eq!(
            captured.max_response_bytes.get(),
            GH_STDOUT_MAX_BYTES as u64
        );
        assert!(captured.abort_signal.is_some());
        assert!(captured.body.is_empty());
        let headers = captured.headers.into_iter().collect::<http::HeaderMap>();
        assert_eq!(
            headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(USER_AGENT)
        );
        assert!(headers.get(http::header::ACCEPT).is_some_and(|value| {
            value
                .to_str()
                .is_ok_and(|value| value.contains("text-match"))
        }));
        assert!(!headers.contains_key(http::header::AUTHORIZATION));
    }

    #[tokio::test]
    async fn injected_executor_receives_validated_json_body() {
        let executor = RecordingExecutor::new(http::StatusCode::CREATED, json!({ "number": 7 }));
        let request = build_request(
            "create_issue",
            &json!({ "repo": "o/r", "title": "t", "body": "b" }),
        )
        .unwrap();
        run_http_executor(&request, &pi_agent::AbortSignal::none(), executor.clone())
            .await
            .expect("injected GitHub executor succeeds");

        let captured = executor.take_request();
        assert_eq!(captured.method, pi_ai::HttpMethod::Post);
        assert_eq!(captured.path_and_query, "/repos/o/r/issues");
        assert_eq!(
            serde_json::from_slice::<Value>(&captured.body).expect("request JSON"),
            json!({ "title": "t", "body": "b" })
        );
        let headers = captured.headers.into_iter().collect::<http::HeaderMap>();
        assert_eq!(
            headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert!(!headers.contains_key(http::header::AUTHORIZATION));
    }

    #[tokio::test]
    async fn injected_executor_redacts_github_error_body() {
        let secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let executor = RecordingExecutor::new(
            http::StatusCode::UNAUTHORIZED,
            json!({ "message": format!("rejected {secret}") }),
        );
        let request = build_request("get_issue", &json!({ "repo": "o/r", "number": 1 })).unwrap();
        let error = run_http_executor(&request, &pi_agent::AbortSignal::none(), executor)
            .await
            .expect_err("GitHub error response must fail")
            .to_string();
        assert!(error.contains("HTTP 401 Unauthorized"), "{error}");
        assert!(!error.contains(&secret), "{error}");
        assert!(error.contains("[REDACTED]"), "{error}");
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    #[test]
    fn render_issue_list_skips_pull_requests() {
        let items = json!([
            { "number": 1, "title": "Bug", "state": "open", "html_url": "https://github.com/o/r/issues/1" },
            { "number": 2, "title": "A PR", "state": "open", "html_url": "https://github.com/o/r/pull/2", "pull_request": {} },
            { "number": 3, "title": "Closed bug", "state": "closed", "html_url": "https://github.com/o/r/issues/3" }
        ]);
        let out = render("list_issues", &items);
        assert_eq!(
            out,
            "#1 Bug\nstate: open | https://github.com/o/r/issues/1\n---\n#3 Closed bug\nstate: closed | https://github.com/o/r/issues/3"
        );
    }

    #[test]
    fn render_search_issues_uses_items_and_empty_results() {
        let body = json!({ "total_count": 2, "items": [
            { "number": 4, "title": "Found", "state": "open", "html_url": "https://github.com/o/r/issues/4" }
        ]});
        let out = render("search_issues", &body);
        assert!(out.contains("#4 Found"), "{out}");
        assert!(
            out.contains("state: open | https://github.com/o/r/issues/4"),
            "{out}"
        );
        assert_eq!(
            render("search_issues", &json!({ "total_count": 0, "items": [] })),
            "No results"
        );
    }

    #[test]
    fn render_detail_includes_body() {
        let item = json!({
            "number": 9,
            "title": "Detail",
            "state": "open",
            "html_url": "https://github.com/o/r/issues/9",
            "body": "first line\nsecond line"
        });
        let out = render("get_issue", &item);
        assert!(
            out.starts_with("#9 Detail\nstate: open | https://github.com/o/r/issues/9"),
            "{out}"
        );
        assert!(out.ends_with("first line\nsecond line"), "{out}");
    }

    #[test]
    fn render_commits_shortens_sha_and_takes_first_line() {
        let items = json!([
            { "sha": "0123456789abcdef", "commit": { "message": "Fix everything\n\nlong body" } },
            { "sha": "abcdef0123456789", "commit": { "message": "Add feature" } }
        ]);
        let out = render("list_commits", &items);
        assert_eq!(out, "0123456 Fix everything\nabcdef0 Add feature");
    }

    #[test]
    fn render_contents_decodes_base64_and_lists_directories() {
        // GitHub wraps the base64 content with a trailing newline; the
        // decoder must strip whitespace (regression for the real API shape).
        let encoded = base64::engine::general_purpose::STANDARD.encode("Hello World\nline two");
        let file = json!({
            "type": "file",
            "path": "README",
            "encoding": "base64",
            "content": format!("{encoded}\n")
        });
        assert_eq!(render("view_file", &file), "Hello World\nline two");

        let dir = json!([
            { "name": "src", "type": "dir" },
            { "name": "README", "type": "file" }
        ]);
        assert_eq!(render("view_file", &dir), "src/\nREADME");
    }

    #[test]
    fn render_code_hits_produces_file_line_snippet() {
        let body = json!({ "items": [
            {
                "path": "src/lib.rs",
                "repository": { "full_name": "octocat/Hello-World" },
                "text_matches": [
                    { "fragment": "pub fn main() {\n    println!(\"hi\");\n}\n", "matches": [{ "text": "main", "indices": [7, 11] }] }
                ]
            }
        ]});
        let out = render("search_code", &body);
        assert!(out.starts_with("octocat/Hello-World:src/lib.rs"), "{out}");
        assert!(out.contains("1: main() {"), "{out}");
    }

    // ------------------------------------------------------------------
    // Redaction
    // ------------------------------------------------------------------

    #[test]
    fn redact_secrets_hides_token_patterns() {
        let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let github_pat = ["github_", "pat_", "ABC_12345678901234567890"].concat();
        let text = format!(
            "auth token=abc123 and {ghp} and {github_pat} and Authorization: Bearer xyz789"
        );
        let redacted = redact_secrets(&text);
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains(ghp.as_str()));
        assert!(!redacted.contains(github_pat.as_str()));
        assert!(!redacted.contains("xyz789"));
        assert!(redacted.contains("[REDACTED]"));
        assert_eq!(
            redact_secrets("plain text, no secrets"),
            "plain text, no secrets"
        );
    }
}
