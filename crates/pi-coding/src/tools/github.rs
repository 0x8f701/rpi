//! `github` tool: GitHub API access via the `gh` CLI with a GH_TOKEN/reqwest
//! fallback.
//!
//! Auth decision: prefer the `gh` CLI. Credentials live in the user's own gh
//! config (`~/.config/gh/hosts.yml`), read by gh itself — rpi never touches,
//! stores, or prints a token. Requests are spawned as `gh api` with a fully
//! argv-built request (no shell interpolation), mirroring the gist-share
//! module. When `gh` is not installed, fall back to the `GH_TOKEN`
//! environment variable via reqwest against `https://api.github.com`. If
//! neither is available the error says exactly what to install or set, and
//! all surfaced error text is passed through [`redact_secrets`] so a token
//! can never leak into tool output.
//!
//! Actions (read-only plus issue create/comment): `search_issues`,
//! `get_issue`, `list_issues`, `create_issue`, `comment_issue`, `list_prs`,
//! `get_pr`, `list_commits`, `view_file`, `search_code`. GitHub JSON is
//! parsed into bounded plain text (`title+number+state+url` blocks for
//! issues/PRs, `file:line:snippet` lines for code search, raw content for
//! `view_file`), truncated to [`OUTPUT_MAX_BYTES`].

use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use serde_json::{Map, Value};
use tokio::process::Command;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};
use pi_ai::Schema;

use crate::redact::redact_secrets;
use crate::truncate::truncate_head;

use super::{
    arg_int, arg_str, check_aborted, copy_capped, s_number, s_object, s_string,
    spawn_with_etxtbsy_retry, text_result,
};

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

/// A fully-validated GitHub API request, shared by both backends (gh CLI and
/// reqwest). `query` holds GET query parameters; `body` holds the JSON body
/// for POST actions; `headers` holds extra headers (code search needs the
/// text-match Accept header).
#[derive(Debug, Clone, PartialEq)]
struct GitHubRequest {
    method: GhMethod,
    endpoint: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

/// Builds the `github` tool. No workspace binding — it reaches the network.
pub(crate) fn github_tool() -> AgentTool {
    let action_schema = schema_with_enum(
        "GitHub API action to perform",
        ACTIONS.iter().map(|a| (*a).to_string()).collect(),
    );
    let state_schema = schema_with_enum(
        "Filter by state (list_issues, list_prs)",
        ["open", "closed", "all"].iter().map(|s| (*s).to_string()).collect(),
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
                s_number("Issue or pull request number (required for get_issue, comment_issue, get_pr)"),
            ),
            ("title", s_string("Issue title (required for create_issue)")),
            (
                "body",
                s_string("Issue body / comment body (required for comment_issue; optional for create_issue)"),
            ),
            ("path", s_string("File path within the repository (required for view_file)")),
            ("state", state_schema),
            (
                "ref",
                s_string("Git ref: branch, tag, or SHA (optional for list_commits and view_file)"),
            ),
        ],
        vec!["action"],
    );
    let description = format!(
        "GitHub API access via the gh CLI (preferred — uses your existing gh auth) with a \
         GH_TOKEN/reqwest fallback. Actions: search_issues, get_issue, list_issues, create_issue, \
         comment_issue, list_prs, get_pr, list_commits, view_file, search_code. Output is bounded \
         to {}KB of plain text.",
        OUTPUT_MAX_BYTES / 1024
    );
    AgentTool::new("github", description, params, move |ctx| {
        async move { run_github(ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Write)
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

/// Tool entry point: validates the action and its parameters, runs the
/// request through the preferred backend, and renders bounded plain text.
pub(crate) async fn run_github(args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action");
    let request = build_request(&action, &args)?;
    let body = execute(&request, &abort).await?;
    check_aborted(&abort)?;
    let output = render(&action, &body);
    let tr = truncate_head(&output, usize::MAX, OUTPUT_MAX_BYTES);
    Ok(text_result(tr.content))
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
                format!("/repos/{}/contents/{}", repo()?, encode_contents_path(&path)),
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
    path.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
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
    /// The `gh` binary is not installed (spawn returned NotFound).
    MissingGh,
    /// `gh` ran but exited nonzero; carries a redacted, bounded hint.
    Failed(String),
}

/// Runs `gh api` with the request, honoring the abort signal and a per-call
/// timeout. `gh` is injectable so tests can point at a fake executable.
async fn run_gh_api_with(gh: &str, request: &GitHubRequest, abort: &AbortSignal) -> Result<GhOutcome> {
    let args = gh_api_args(request);
    let mut cmd = Command::new(gh);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group: a timeout/abort kill reaps gh and any descendants
    // sharing the output pipes (same pattern as `run_bash_core`).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    let mut child = match spawn_with_etxtbsy_retry(&mut cmd, gh).await {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GhOutcome::MissingGh),
        Err(e) => return Err(anyhow!("failed to start {gh}: {e}")),
    };

    // Drain stdout/stderr on separate tasks, both byte-capped. `child` stays
    // available for the timeout/abort kill (same structure as
    // `extract_pdf_text_with`). `biased` with the abort branch first makes an
    // already-cancelled signal deterministically win over a fast child exit.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let _ = copy_capped(&mut stdout, &mut out, GH_STDOUT_MAX_BYTES).await;
        out
    });
    let err_task = tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = copy_capped(&mut stderr, &mut err, GH_STDERR_MAX_BYTES).await;
        err
    });

    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Aborted,
    }
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        let sleep = tokio::time::sleep(REQUEST_TIMEOUT);
        tokio::pin!(sleep);
        let abort_fut = abort.cancelled();
        tokio::pin!(abort_fut);
        tokio::select! {
            biased;
            _ = &mut abort_fut => Outcome::Aborted,
            _ = &mut sleep => Outcome::TimedOut,
            res = &mut wait => Outcome::Exited(res),
        }
    };

    // On timeout/abort kill the process group so the piped output closes and
    // the readers finish.
    if matches!(outcome, Outcome::TimedOut | Outcome::Aborted) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let stdout = out_task.await.map_err(|e| anyhow!("{gh} output task failed: {e}"))?;
    let stderr = err_task.await.map_err(|e| anyhow!("{gh} error task failed: {e}"))?;

    match outcome {
        Outcome::Exited(Ok(status)) if status.success() => {
            let text = String::from_utf8_lossy(&stdout);
            let trimmed = text.trim();
            let value = serde_json::from_str(trimmed).map_err(|e| {
                anyhow!(
                    "{gh} api returned non-JSON output: {e}: {}",
                    redact_secrets(&hint(trimmed, HINT_MAX_CHARS))
                )
            })?;
            Ok(GhOutcome::Ok(value))
        }
        Outcome::Exited(Ok(status)) => {
            // gh api exits nonzero on HTTP errors; prefer the API error
            // message embedded in the stdout JSON, else stderr.
            let stdout_text = String::from_utf8_lossy(&stdout);
            let stderr_text = String::from_utf8_lossy(&stderr);
            let api_message = serde_json::from_str::<Value>(stdout_text.trim())
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from));
            let hint_text = api_message
                .or_else(|| {
                    let t = stderr_text.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                })
                .unwrap_or_default();
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
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
        Outcome::Exited(Err(e)) => Err(anyhow!("{gh} api failed to run: {e}")),
        Outcome::TimedOut => Err(anyhow!(
            "{gh} api timed out after {}s",
            REQUEST_TIMEOUT.as_secs()
        )),
        Outcome::Aborted => Err(anyhow!("Operation aborted")),
    }
}

/// Executes the request through the preferred backend: `gh` CLI first, with
/// a GH_TOKEN/reqwest fallback when gh is missing.
async fn execute(request: &GitHubRequest, abort: &AbortSignal) -> Result<Value> {
    execute_with(request, "gh", std::env::var("GH_TOKEN").ok(), abort).await
}

/// Core entry with injectable gh program and token so the fallback trigger is
/// unit-testable without mutating the process environment (the workspace
/// forbids `unsafe`, which `std::env::set_var` requires in edition 2024).
async fn execute_with(
    request: &GitHubRequest,
    gh: &str,
    token: Option<String>,
    abort: &AbortSignal,
) -> Result<Value> {
    match run_gh_api_with(gh, request, abort).await? {
        GhOutcome::Ok(value) => Ok(value),
        GhOutcome::MissingGh => match token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(token) => run_reqwest(request, token, abort).await,
            None => Err(anyhow!(
                "GitHub tool: `gh` CLI not found and GH_TOKEN is not set. \
                 Install gh (https://cli.github.com/) or set GH_TOKEN to use the GitHub API."
            )),
        },
        GhOutcome::Failed(hint) => Err(anyhow!("gh api failed: {hint}")),
    }
}

/// Builds a reqwest request builder for the given API request and token
/// without sending it. Shared by [`run_reqwest`] (which sends) and the unit
/// tests (which build and assert on the URL/headers/body with no network).
fn reqwest_builder(request: &GitHubRequest, token: &str) -> Result<reqwest::RequestBuilder> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("github HTTP client error: {e}"))?;
    let url = format!("https://api.github.com{}", request.endpoint);
    let mut builder = match request.method {
        GhMethod::Get => client.get(&url),
        GhMethod::Post => client.post(&url),
    };
    if !request.query.is_empty() {
        builder = builder.query(&request.query);
    }
    if let Some(body) = &request.body {
        builder = builder.json(body);
    }
    builder = builder.bearer_auth(token);
    for (key, value) in &request.headers {
        builder = builder.header(key, value);
    }
    Ok(builder)
}

/// Fallback backend: direct reqwest call against api.github.com using the
/// `GH_TOKEN` from the environment. The token only ever appears in the
/// Authorization header; errors never include it.
async fn run_reqwest(request: &GitHubRequest, token: &str, abort: &AbortSignal) -> Result<Value> {
    let send = async {
        let resp = reqwest_builder(request, token)?
            .send()
            .await
            .map_err(|e| anyhow!("github request failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("github response read failed: {e}"))?;
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(text.trim())
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| text.trim().to_string());
            return Err(anyhow!(
                "GitHub API error (HTTP {status}): {}",
                redact_secrets(&hint(&message, HINT_MAX_CHARS))
            ));
        }
        serde_json::from_str(text.trim())
            .map_err(|e| anyhow!("github response parse failed: {e}"))
    };
    let body = match tokio::time::timeout(REQUEST_TIMEOUT, send).await {
        Ok(inner) => inner?,
        Err(_) => {
            return Err(anyhow!(
                "github request timed out after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))
        }
    };
    check_aborted(abort)?;
    Ok(body)
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
        return format!("submodule {} -> {}", field(body, "name"), field(body, "sha"));
    }
    format!("(unsupported contents response type: {})", field(body, "type"))
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
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    // ------------------------------------------------------------------
    // Schema / validation
    // ------------------------------------------------------------------

    #[test]
    fn schema_requires_action_and_enumerates_actions() {
        let tool = github_tool();
        assert!(tool.parameters.required.contains(&"action".to_string()));
        let action = tool.parameters.properties.get("action").expect("action prop");
        let values: Vec<&str> = action
            .enum_values
            .iter()
            .map(|v| v.as_str().unwrap_or(""))
            .collect();
        assert_eq!(values, ACTIONS);
        assert!(tool.parameters.validate(&json!({ "action": "list_issues" })).is_ok());
        assert!(tool.parameters.validate(&json!({ "action": "nope" })).is_err());
    }

    #[test]
    fn build_request_rejects_bad_params_per_action() {
        // Unknown action.
        assert!(build_request("explode", &json!({})).is_err());
        // Missing repo.
        let err = build_request("get_issue", &json!({ "number": 1 })).unwrap_err().to_string();
        assert!(err.contains("owner/name"), "{err}");
        for bad in ["norepo", "owner/", "/name", "a/b/c", "own er/name"] {
            let err = build_request("get_issue", &json!({ "repo": bad, "number": 1 }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("owner/name"), "{bad}: {err}");
        }
        // Missing / non-positive number.
        let err = build_request("get_issue", &json!({ "repo": "o/r" })).unwrap_err().to_string();
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
        let err = build_request("create_issue", &json!({ "repo": "o/r" })).unwrap_err().to_string();
        assert!(err.contains("title is required"), "{err}");
        let err = build_request("comment_issue", &json!({ "repo": "o/r", "number": 1 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("body is required"), "{err}");
        let err = build_request("view_file", &json!({ "repo": "o/r" })).unwrap_err().to_string();
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
            ("get_issue", json!({ "repo": "octocat/Hello-World", "number": 1 }), "GET /repos/octocat/Hello-World/issues/1"),
            ("list_issues", json!({ "repo": "octocat/Hello-World", "state": "closed" }), "GET /repos/octocat/Hello-World/issues"),
            ("list_prs", json!({ "repo": "octocat/Hello-World" }), "GET /repos/octocat/Hello-World/pulls"),
            ("get_pr", json!({ "repo": "octocat/Hello-World", "number": 3 }), "GET /repos/octocat/Hello-World/pulls/3"),
            ("list_commits", json!({ "repo": "octocat/Hello-World", "ref": "master" }), "GET /repos/octocat/Hello-World/commits"),
            ("view_file", json!({ "repo": "octocat/Hello-World", "path": "src/main.rs", "ref": "main" }), "GET /repos/octocat/Hello-World/contents/src/main.rs"),
            ("search_code", json!({ "query": "repo:octocat/Hello-World fn" }), "GET /search/code"),
        ];
        for (action, args, expected) in cases {
            let req = build_request(action, &args).unwrap_or_else(|e| panic!("{action}: {e}"));
            let method = if req.method == GhMethod::Post { "POST" } else { "GET" };
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
        assert_eq!(create.body, Some(json!({ "title": "hello", "body": "world" })));

        let create_no_body = build_request(
            "create_issue",
            &json!({ "repo": "o/r", "title": "hello" }),
        )
        .unwrap();
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
        let list_issues = build_request("list_issues", &json!({ "repo": "o/r", "state": "all" })).unwrap();
        assert_eq!(list_issues.query, vec![("state".to_string(), "all".to_string())]);
        let commits = build_request("list_commits", &json!({ "repo": "o/r", "ref": "feature/x" })).unwrap();
        assert_eq!(commits.query, vec![("sha".to_string(), "feature/x".to_string())]);
        let view = build_request("view_file", &json!({ "repo": "o/r", "path": "a b/c.txt", "ref": "main" })).unwrap();
        assert_eq!(view.endpoint, "/repos/o/r/contents/a%20b/c.txt");
        assert_eq!(view.query, vec![("ref".to_string(), "main".to_string())]);

        // Code search requests the text-match Accept header.
        let code = build_request("search_code", &json!({ "query": "fn main" })).unwrap();
        assert!(code
            .headers
            .iter()
            .any(|(k, v)| k == "Accept" && v.contains("text-match")));
    }

    #[test]
    fn gh_api_args_renders_argv_without_shell_interpolation() {
        let search = build_request("search_issues", &json!({ "query": "repo:o/r is:issue" })).unwrap();
        assert_eq!(
            gh_api_args(&search),
            vec!["api", "--method", "GET", "/search/issues", "-f", "q=repo:o/r is:issue"]
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

    // ------------------------------------------------------------------
    // Backends (fake gh executable, no network)
    // ------------------------------------------------------------------

    fn fake_gh_script(script: &str) -> std::path::PathBuf {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("gh");
        let mut file = std::fs::File::create(&path).expect("create fake gh");
        file.write_all(script.as_bytes()).expect("write fake gh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = file.metadata().expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod fake gh");
        }
        // Keep the tempdir alive by leaking it (the path stays valid for the
        // test process lifetime).
        std::mem::forget(dir);
        path
    }

    #[tokio::test]
    async fn run_gh_api_detects_missing_binary() {
        let req = build_request("search_issues", &json!({ "query": "q" })).unwrap();
        let outcome = run_gh_api_with("/nonexistent/gh-xyz", &req, &AbortSignal::none())
            .await
            .expect("spawn outcome");
        assert!(matches!(outcome, GhOutcome::MissingGh));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_gh_api_parses_fake_gh_json_output() {
        let gh = fake_gh_script(
            "#!/bin/sh\nprintf '%s' '{\"items\":[{\"number\":7,\"title\":\"Fix the bug\",\"state\":\"open\",\"html_url\":\"https://github.com/octocat/Hello-World/issues/7\"}]}'\n",
        );
        let req = build_request("search_issues", &json!({ "query": "q" })).unwrap();
        let outcome = run_gh_api_with(&gh.to_string_lossy(), &req, &AbortSignal::none())
            .await
            .expect("spawn outcome");
        let GhOutcome::Ok(value) = outcome else {
            panic!("expected Ok, got {outcome:?}");
        };
        assert_eq!(value["items"][0]["number"], 7);
        assert_eq!(value["items"][0]["title"], "Fix the bug");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_gh_api_surfaces_nonzero_exit_with_stderr() {
        let gh = fake_gh_script("#!/bin/sh\necho 'not authenticated; run gh auth login' >&2\nexit 3\n");
        let req = build_request("search_issues", &json!({ "query": "q" })).unwrap();
        let outcome = run_gh_api_with(&gh.to_string_lossy(), &req, &AbortSignal::none())
            .await
            .expect("spawn outcome");
        let GhOutcome::Failed(hint) = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert!(hint.contains("exit code 3"), "{hint}");
        assert!(hint.contains("gh auth login"), "{hint}");
    }

    #[tokio::test]
    async fn execute_reports_actionable_error_when_gh_missing_and_no_token() {
        let req = build_request("search_issues", &json!({ "query": "q" })).unwrap();
        let err = execute_with(&req, "/nonexistent/gh-xyz", None, &AbortSignal::none())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh` CLI not found"), "{err}");
        assert!(err.contains("GH_TOKEN"), "{err}");
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

    // ------------------------------------------------------------------
    // reqwest request construction (no network)
    // ------------------------------------------------------------------

    #[test]
    fn reqwest_builder_builds_url_headers_and_body_without_sending() {
        let search = build_request("search_code", &json!({ "query": "fn main repo:o/r" })).unwrap();
        let request = reqwest_builder(&search, "sekrit-token")
            .expect("builder")
            .build()
            .expect("build");
        assert_eq!(
            request.url().as_str(),
            "https://api.github.com/search/code?q=fn+main+repo%3Ao%2Fr"
        );
        assert_eq!(
            request.headers().get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sekrit-token")
        );
        assert!(request
            .headers()
            .get("accept")
            .is_some_and(|v| v.to_str().is_ok_and(|s| s.contains("text-match"))));
        assert!(!request.url().as_str().contains("sekrit-token"));

        let create = build_request(
            "create_issue",
            &json!({ "repo": "o/r", "title": "t", "body": "b" }),
        )
        .unwrap();
        let request = reqwest_builder(&create, "sekrit-token")
            .expect("builder")
            .build()
            .expect("build");
        assert_eq!(request.method(), reqwest::Method::POST);
        let body_bytes = request
            .body()
            .and_then(|b| b.as_bytes())
            .expect("json body");
        let body: Value = serde_json::from_slice(body_bytes).expect("body json");
        assert_eq!(body, json!({ "title": "t", "body": "b" }));
        assert!(!String::from_utf8_lossy(body_bytes).contains("sekrit-token"));
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
        assert!(out.contains("state: open | https://github.com/o/r/issues/4"), "{out}");
        assert_eq!(render("search_issues", &json!({ "total_count": 0, "items": [] })), "No results");
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
        assert!(out.starts_with("#9 Detail\nstate: open | https://github.com/o/r/issues/9"), "{out}");
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
        assert_eq!(redact_secrets("plain text, no secrets"), "plain text, no secrets");
    }

    // ------------------------------------------------------------------
    // Real gh smoke tests (skip-guarded: run only when gh is installed and
    // authenticated on the host). Read-only actions only.
    // ------------------------------------------------------------------

    fn gh_smoke_available() -> bool {
        let version = std::process::Command::new("gh").arg("--version").output();
        if !version.is_ok_and(|o| o.status.success()) {
            return false;
        }
        std::process::Command::new("gh")
            .args(["auth", "status"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Runs a real-gh smoke call with a short bounded retry: GitHub's search
    /// API is occasionally rate-limited or transiently erroring in parallel
    /// full-suite runs, which must not make the shared suite flaky.
    async fn run_smoke_with_retry(tool: &AgentTool, args: Value) -> AgentToolResult {
        let mut last_error = None;
        for attempt in 0..3 {
            match (tool.execute)(ctx(args.clone())).await {
                Ok(result) => return result,
                Err(e) => {
                    last_error = Some(e);
                    tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
                }
            }
        }
        panic!(
            "real gh smoke failed after 3 attempts: {}",
            last_error.unwrap()
        );
    }

    #[tokio::test]
    async fn real_gh_search_issues_smoke() {
        if !gh_smoke_available() {
            eprintln!("skipping real_gh_search_issues_smoke: gh CLI not installed or not authenticated");
            return;
        }
        let tool = github_tool();
        let result = run_smoke_with_retry(&tool, json!({
            "action": "search_issues",
            "query": "repo:octocat/Hello-World"
        }))
        .await;
        let text = text_of(&result);
        assert!(
            text.contains("state:") || text == "No results",
            "unexpected search_issues output: {text}"
        );
    }

    #[tokio::test]
    async fn real_gh_view_file_smoke() {
        if !gh_smoke_available() {
            eprintln!("skipping real_gh_view_file_smoke: gh CLI not installed or not authenticated");
            return;
        }
        let tool = github_tool();
        let result = run_smoke_with_retry(&tool, json!({
            "action": "view_file",
            "repo": "octocat/Hello-World",
            "path": "README",
            "ref": "master"
        }))
        .await;
        let text = text_of(&result);
        assert!(!text.is_empty(), "expected README content, got empty output");
        assert!(
            text.contains("Hello World"),
            "unexpected README content: {text}"
        );
    }

    #[tokio::test]
    async fn real_gh_search_code_smoke() {
        if !gh_smoke_available() {
            eprintln!("skipping real_gh_search_code_smoke: gh CLI not installed or not authenticated");
            return;
        }
        let tool = github_tool();
        let result = run_smoke_with_retry(&tool, json!({
            "action": "search_code",
            "query": "repo:octocat/Hello-World Hello"
        }))
        .await;
        let text = text_of(&result);
        assert!(
            text.contains("octocat/Hello-World:README"),
            "unexpected search_code output: {text}"
        );
    }
}
