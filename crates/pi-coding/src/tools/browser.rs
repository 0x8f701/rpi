//! `browser` tool: headless Chromium/Chrome automation over the Chrome
//! DevTools Protocol (CDP).
//!
//! Every invocation captures one immutable application generation, uses its
//! filesystem and process authority, launches Chromium through the governed
//! Unix backend, and controls it exclusively through the NUL-framed native
//! remote-debugging pipe. A session is exposed only after `Browser.getVersion`
//! proves that Chromium started with its native sandbox enabled.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pi_agent::{
    AbortSignal, AgentTool, AgentToolResult, ToolApprovalRequest, ToolApprovalSummary,
    ToolCallApprovalToken, ToolCapability,
};
use pi_ai::ContentBlock;
use pi_sandbox::{
    AbsolutePath, CanonicalTargetFdMapping, HostPlatform, SandboxError, SpawnFamily, StdioMode,
    TargetFdDirection,
};

use super::{
    arg_str, check_aborted, imageresize::process_image, paths::resolve_to_cwd, s_object, s_string,
    text_result,
};
use crate::WorkspaceRoots;
use crate::application::generation::{ApplicationGeneration, GenerationCaptureFn};
use crate::fs::{
    FileMode, FsError, PolicyFs, PolicyReplacement, PolicyScratch, SandboxVisiblePath,
};
#[cfg(unix)]
use crate::process::{
    AuthorizedSpawn, GovernedPipeProcess, browser_debugging_pipes, launch_authorized_browser_pipe,
};
use crate::process::{TrustedToolProbe, authorize_spawn, run_trusted_tool_probe};
use crate::sandbox::{
    CanonicalSpawnCall, CredentialAudience, RawSpawnRequest, SpawnAuthorization, target_fd_summary,
};
use crate::truncate::{format_size, truncate_head};

/// Wall-clock budget for a single action after the native startup proof.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for one CDP command round trip after startup.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for waiting out a navigation (polling `document.readyState`).
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// One absolute deadline covering preparation, launch, and native proof.
const BROWSER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_PIPE_FRAME_LIMIT: usize = 1024 * 1024;
const BROWSER_STDERR_TAIL_LIMIT: usize = 8 * 1024;
const BROWSER_PROFILE_ID_KEY: &str = "_browserProfileId";
/// Output byte budget for `extract` text — keeps agent context lean.
const EXTRACT_MAX_BYTES: usize = 16 * 1024;
/// Cap on the number of tabs rendered by `list_tabs`.
const LIST_TABS_MAX: usize = 20;
/// Raw screenshot cap before image decoding/resizing.
const SCREENSHOT_MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
/// Inline base64 budget leaves headroom inside the 4 MiB listener frame cap.
const SCREENSHOT_MAX_BASE64_BYTES: usize = 3 * 1024 * 1024;

/// The supported `browser` actions (schema `action` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Navigate,
    Click,
    Fill,
    Screenshot,
    Extract,
    ListTabs,
    Close,
}

const ACTIONS_HINT: &str = "navigate, click, fill, screenshot, extract, list_tabs, close";

/// Builds the generation-bound `browser` tool.
pub(crate) fn browser_tool_for_workspace(
    workspace: WorkspaceRoots,
    capture_generation: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("action", s_string("Action to perform")),
            (
                "url",
                s_string(
                    "URL to navigate to (http:, https:, data:, file:, about:blank). Required for \
                     navigate; optional on click/fill/screenshot/extract to navigate the fresh \
                     browser to this page before acting",
                ),
            ),
            (
                "selector",
                s_string("CSS selector for click / fill / extract"),
            ),
            (
                "text",
                s_string("Text to fill into the matched input (fill only)"),
            ),
            (
                "path",
                s_string("Workspace-relative output path for the screenshot PNG (screenshot only)"),
            ),
            (
                BROWSER_PROFILE_ID_KEY,
                s_string("Internal browser profile identifier"),
            ),
        ],
        vec!["action"],
    );
    let description = format!(
        "Automate a governed headless Chromium/Chrome browser over its native debugging pipe. \
Actions: navigate (url), click (selector), fill (selector, text), screenshot (path), \
extract (selector? — page or element text), list_tabs, close. \
Each call uses a fresh revision-owned profile and proves Chromium's native sandbox before CDP \
commands are accepted. Screenshots stay within captured workspace authority. Outputs are \
bounded to {}KB.",
        EXTRACT_MAX_BYTES / 1024
    );
    let execute_capture = capture_generation.clone();
    let approval_capture = capture_generation;
    let approval_workspace = workspace.clone();
    AgentTool::new("browser", description, params, move |ctx| {
        let capture_generation = execute_capture.clone();
        let workspace = workspace.clone();
        async move {
            let generation = capture_generation()?;
            run_browser_for_workspace(
                generation,
                &workspace,
                ctx.arguments,
                ctx.approval_token,
                ctx.abort,
            )
            .await
        }
    })
    .with_prepare_arguments(inject_browser_profile_id)
    .with_prepare_approval(move |arguments| {
        let capture_generation = approval_capture.clone();
        let workspace = approval_workspace.clone();
        Box::pin(async move {
            let generation = capture_generation()?;
            prepare_browser_approval(generation, &workspace, arguments).await
        })
    })
    .with_capability(ToolCapability::Exec)
}

fn inject_browser_profile_id(mut arguments: Value) -> Result<Value> {
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| anyhow!("browser: arguments must be an object"))?;
    let mut random = uuid::Uuid::now_v7().simple().to_string();
    random.replace_range(..1, "b");
    object.insert(BROWSER_PROFILE_ID_KEY.to_owned(), Value::String(random));
    Ok(arguments)
}

// ---------------------------------------------------------------------------
// Argument validation (pure, unit-testable)
// ---------------------------------------------------------------------------

fn parse_action(input: &str) -> Result<Action> {
    if input.is_empty() {
        bail!("browser: action is required (one of: {ACTIONS_HINT})");
    }
    match input {
        "navigate" => Ok(Action::Navigate),
        "click" => Ok(Action::Click),
        "fill" => Ok(Action::Fill),
        "screenshot" => Ok(Action::Screenshot),
        "extract" => Ok(Action::Extract),
        "list_tabs" => Ok(Action::ListTabs),
        "close" => Ok(Action::Close),
        other => bail!("browser: unknown action {other:?} (expected one of: {ACTIONS_HINT})"),
    }
}

/// Rejects URLs with unsupported schemes up front so a typo like
/// `javascript:...` or `ftp://...` fails fast instead of confusingly in the
/// browser. `about:blank` is accepted as the empty starting page.
fn validate_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        bail!("browser: url is required for navigate");
    }
    if url == "about:blank" {
        return Ok(());
    }
    let parsed = url::Url::parse(url).map_err(|_| anyhow!("browser: invalid URL: {url}"))?;
    match parsed.scheme() {
        "http" | "https" | "data" | "file" => Ok(()),
        other => bail!(
            "browser: unsupported URL scheme {other:?} \
             (allowed: http, https, data, file, about:blank)"
        ),
    }
}

/// Per-action required-argument checks. `extract`/`list_tabs`/`close` take no
/// required extras. `text` may be an empty string (clearing an input) but the
/// key must be present for `fill`.
fn validate_args(action: &Action, args: &Value) -> Result<()> {
    let selector = arg_str(args, "selector");
    let path = arg_str(args, "path");
    match action {
        Action::Navigate => validate_url(&arg_str(args, "url")),
        Action::Click => {
            if selector.trim().is_empty() {
                bail!("browser: selector is required for click");
            }
            Ok(())
        }
        Action::Fill => {
            if selector.trim().is_empty() {
                bail!("browser: selector is required for fill");
            }
            if args.get("text").and_then(Value::as_str).is_none() {
                bail!("browser: text is required for fill (may be an empty string)");
            }
            Ok(())
        }
        Action::Screenshot => {
            if path.trim().is_empty() {
                bail!("browser: path is required for screenshot");
            }
            Ok(())
        }
        Action::Extract | Action::ListTabs | Action::Close => Ok(()),
    }?;
    // Non-navigate actions may carry an optional `url` (navigate first); it
    // must be valid when present.
    if !matches!(action, Action::Navigate) {
        let url = arg_str(args, "url");
        if !url.trim().is_empty() {
            validate_url(&url)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Chrome/Chromium discovery
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Browser lifecycle
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowserVersion {
    product: String,
    protocol_version: String,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeBrowserSandboxFailure {
    #[error("browser sandbox handshake timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },
    #[error("browser exited before sandbox confirmation")]
    ProcessExited {
        code: Option<i32>,
        stderr_bytes: usize,
    },
    #[error("browser debugging pipe closed before sandbox confirmation")]
    DebuggingPipeClosed { stderr_bytes: usize },
    #[error("browser debugging-pipe frame exceeds {limit} bytes")]
    FrameTooLarge { limit: usize },
    #[error("browser debugging-pipe frame is invalid")]
    InvalidFrame,
    #[error("browser rejected the sandbox proof command")]
    CommandRejected,
}

impl NativeBrowserSandboxFailure {
    #[must_use]
    pub const fn variant_name(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "timeout",
            Self::ProcessExited { .. } => "process-exited",
            Self::DebuggingPipeClosed { .. } => "debugging-pipe-closed",
            Self::FrameTooLarge { .. } => "frame-too-large",
            Self::InvalidFrame => "invalid-frame",
            Self::CommandRejected => "command-rejected",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserSandboxError {
    #[error("browser launch sandbox failed: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("browser filesystem staging or cleanup failed: {0}")]
    Filesystem(#[from] FsError),
    #[error("browser native sandbox proof failed: {0}")]
    Native(#[from] NativeBrowserSandboxFailure),
    #[error("browser launch is unsupported on {platform:?}")]
    UnsupportedPlatform { platform: HostPlatform },
}

impl BrowserSandboxError {
    #[must_use]
    pub const fn is_filesystem(&self) -> bool {
        matches!(self, Self::Filesystem(_))
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn filesystem_denied_for_test() -> Self {
        Self::Filesystem(FsError::Denied)
    }

    #[must_use]
    pub const fn variant_name(&self) -> &'static str {
        match self {
            Self::Sandbox(_) => "sandbox",
            Self::Filesystem(_) => "filesystem",
            Self::Native(_) => "native",
            Self::UnsupportedPlatform { .. } => "unsupported-platform",
        }
    }
}

struct StderrTail {
    bytes: Arc<AtomicUsize>,
}

impl StderrTail {
    fn spawn(mut stderr: tokio::fs::File) -> Self {
        let bytes = Arc::new(AtomicUsize::new(0));
        let observed = bytes.clone();
        tokio::spawn(async move {
            let mut tail = Vec::with_capacity(BROWSER_STDERR_TAIL_LIMIT);
            let mut buffer = [0_u8; 4096];
            loop {
                let read = match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let _ = observed.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(read))
                });
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > BROWSER_STDERR_TAIL_LIMIT {
                    let excess = tail.len() - BROWSER_STDERR_TAIL_LIMIT;
                    tail.drain(..excess);
                }
            }
            let _ = crate::redact::redact_secrets(&String::from_utf8_lossy(&tail));
        });
        Self { bytes }
    }

    fn byte_count(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

#[cfg(unix)]
struct BrowserSession {
    _generation: Arc<ApplicationGeneration>,
    process: GovernedPipeProcess,
    profile: Option<PolicyScratch>,
    command_writer: tokio::fs::File,
    response_reader: tokio::fs::File,
    stderr: StderrTail,
    next_id: u64,
    page_session_id: Option<String>,
}

#[cfg(unix)]
impl BrowserSession {
    async fn shutdown(mut self) -> std::result::Result<(), BrowserSandboxError> {
        let process_result = self
            .process
            .shutdown(Duration::from_secs(1))
            .await
            .map(|_| ())
            .map_err(BrowserSandboxError::from);
        let profile_result = self
            .profile
            .take()
            .map(PolicyScratch::destroy)
            .transpose()
            .map_err(BrowserSandboxError::from);
        process_result?;
        profile_result.map(|_| ())
    }

    async fn send_command(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut payload = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = &self.page_session_id {
            payload["sessionId"] = Value::String(session_id.clone());
        }
        let frame = payload.to_string();
        self.command_writer
            .write_all(frame.as_bytes())
            .await
            .map_err(|_| anyhow!("browser: CDP send failed"))?;
        self.command_writer
            .write_all(&[0])
            .await
            .map_err(|_| anyhow!("browser: CDP send failed"))?;
        self.command_writer
            .flush()
            .await
            .map_err(|_| anyhow!("browser: CDP send failed"))?;
        let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
        loop {
            let response = read_cdp_frame(&mut self.response_reader, deadline)
                .await
                .map_err(|error| anyhow!("browser: CDP receive failed: {error}"))?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown CDP error");
                bail!("browser: CDP {method} failed: {message}");
            }
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn send_browser_command(&mut self, method: &str, params: Value) -> Result<Value> {
        let page_session_id = self.page_session_id.take();
        let result = self.send_command(method, params).await;
        self.page_session_id = page_session_id;
        result
    }

    async fn attach_page_target(&mut self) -> Result<()> {
        let targets = self
            .send_browser_command("Target.getTargets", json!({}))
            .await?;
        let page_target = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .and_then(|targets| {
                targets.iter().find(|target| {
                    target.get("type").and_then(Value::as_str) == Some("page")
                        && target.get("attached").and_then(Value::as_bool) != Some(true)
                })
            });
        let target_id = match page_target
            .and_then(|target| target.get("targetId"))
            .and_then(Value::as_str)
        {
            Some(target_id) => target_id.to_owned(),
            None => self
                .send_browser_command("Target.createTarget", json!({ "url": "about:blank" }))
                .await?
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("browser: CDP did not return a page target"))?
                .to_owned(),
        };
        let attached = self
            .send_browser_command(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        self.page_session_id = Some(
            attached
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("browser: CDP did not attach to the page target"))?
                .to_owned(),
        );
        Ok(())
    }

    async fn eval_value(&mut self, expression: &str) -> Result<Value> {
        let response = self
            .send_command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(exception) = response.get("exceptionDetails") {
            let text = exception
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("page script threw");
            bail!("browser: page script error: {text}");
        }
        Ok(response
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn eval_string(&mut self, expression: &str) -> Result<String> {
        match self.eval_value(expression).await? {
            Value::String(value) => Ok(value),
            Value::Null => Ok(String::new()),
            other => Ok(other.to_string()),
        }
    }
}

#[cfg(unix)]
fn build_browser_argv(profile: &SandboxVisiblePath) -> Vec<String> {
    vec![
        "--headless=new".to_owned(),
        "--remote-debugging-pipe".to_owned(),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        "--disable-background-networking".to_owned(),
        "--disable-extensions".to_owned(),
        "--disable-sync".to_owned(),
        "--disable-component-update".to_owned(),
        "--disable-gpu".to_owned(),
        "--disable-dev-shm-usage".to_owned(),
        "--window-size=1280,900".to_owned(),
        format!("--user-data-dir={}", profile.as_path().display()),
        "about:blank".to_owned(),
    ]
}

fn browser_target_fd_mappings() -> std::result::Result<Vec<CanonicalTargetFdMapping>, SandboxError>
{
    Ok(vec![
        CanonicalTargetFdMapping::new(3, TargetFdDirection::ParentWrites)?,
        CanonicalTargetFdMapping::new(4, TargetFdDirection::ParentReads)?,
    ])
}

fn browser_audience() -> Result<CredentialAudience> {
    Ok(CredentialAudience::new_spawn_family(SpawnFamily::Browser)?)
}

fn browser_profile_id(arguments: &Value) -> Result<&str> {
    let id = arguments
        .get(BROWSER_PROFILE_ID_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("browser: internal profile identifier is missing"))?;
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || !id.as_bytes()[0].is_ascii_lowercase()
    {
        bail!("browser: internal profile identifier is invalid");
    }
    Ok(id)
}

fn explicit_browser_path() -> Result<Option<AbsolutePath>> {
    std::env::var_os("CHROME_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(AbsolutePath::parse)
        .transpose()
        .map_err(Into::into)
}

async fn prepare_browser_approval(
    generation: Arc<ApplicationGeneration>,
    workspace: &WorkspaceRoots,
    arguments: Value,
) -> Result<Option<ToolApprovalRequest>> {
    reject_unsupported_browser_platform()?;
    #[cfg(not(unix))]
    {
        let _ = (generation, workspace, arguments);
        return Err(BrowserSandboxError::UnsupportedPlatform {
            platform: HostPlatform::Windows,
        }
        .into());
    }
    #[cfg(unix)]
    {
        let action = parse_action(arg_str(&arguments, "action").trim())?;
        validate_args(&action, &arguments)?;
        if action == Action::Close {
            return Ok(None);
        }
        let profile = generation
            .sandbox()
            .scratch()
            .browser_profile_path(browser_profile_id(&arguments)?)?;
        let binary = run_trusted_tool_probe(
            &generation,
            TrustedToolProbe::Browser {
                explicit: explicit_browser_path()?,
            },
        )?;
        let argv = browser_argv(&binary, &profile);
        let target_fd_mappings = browser_target_fd_mappings()?;
        let call = CanonicalSpawnCall {
            argv: argv.clone(),
            cwd: AbsolutePath::parse(workspace.cwd().to_path_buf())?,
            requested_env: BTreeMap::new(),
            requested_grants: BTreeSet::new(),
            stdio: StdioMode::Piped,
            terminal_size: None,
            timeout_ms: Some(BROWSER_STARTUP_TIMEOUT.as_millis() as u64),
            target_fd_mappings: target_fd_mappings.clone(),
            mount_sources: Vec::new(),
            retained_executable: None,
            retained_working_directory: None,
        };
        let audience = browser_audience()?;
        let summary = ToolApprovalSummary {
            family: SpawnFamily::Browser.as_str().to_owned(),
            executable: "governed browser executable".to_owned(),
            argument_count: u32::try_from(argv.len().saturating_sub(1)).unwrap_or(u32::MAX),
            working_directory: "captured workspace".to_owned(),
            credential_audience: Some(SpawnFamily::Browser.as_str().to_owned()),
            requested_credential_count: 0,
            target_fds: target_fd_summary(&target_fd_mappings),
        };
        let approvals = generation.security().approvals().clone();
        Ok(Some(ToolApprovalRequest::new(
            summary,
            Box::new(move || {
                Box::pin(async move {
                    approvals
                        .issue(
                            generation.sandbox().clone(),
                            SpawnFamily::Browser,
                            audience,
                            &call,
                            current_unix_ms(),
                        )
                        .map_err(Into::into)
                })
            }),
        )))
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(unix)]
fn browser_argv(binary: &AbsolutePath, profile: &SandboxVisiblePath) -> Vec<String> {
    let mut argv = vec![binary.as_path().display().to_string()];
    argv.extend(build_browser_argv(profile));
    argv
}
#[cfg(unix)]
async fn launch_browser_pipe(
    generation: Arc<ApplicationGeneration>,
    authorized: AuthorizedSpawn,
    profile: PolicyScratch,
    debugger_writer: std::fs::File,
    debugger_reader: std::fs::File,
    deadline: tokio::time::Instant,
) -> std::result::Result<BrowserSession, BrowserSandboxError> {
    let mut process = match launch_authorized_browser_pipe(
        authorized,
        debugger_writer,
        debugger_reader,
        deadline,
    )
    .await
    {
        Ok(process) => process,
        Err(error) => {
            profile.destroy()?;
            return Err(error.into());
        }
    };
    let command_writer = match process.take_debugger_writer() {
        Some(writer) => tokio::fs::File::from_std(writer),
        None => {
            let _ = process.shutdown(Duration::from_secs(1)).await;
            profile.destroy()?;
            return Err(
                NativeBrowserSandboxFailure::DebuggingPipeClosed { stderr_bytes: 0 }.into(),
            );
        }
    };
    let response_reader = match process.take_debugger_reader() {
        Some(reader) => tokio::fs::File::from_std(reader),
        None => {
            let _ = process.shutdown(Duration::from_secs(1)).await;
            profile.destroy()?;
            return Err(
                NativeBrowserSandboxFailure::DebuggingPipeClosed { stderr_bytes: 0 }.into(),
            );
        }
    };
    let stderr = match process.take_stderr() {
        Some(stderr) => StderrTail::spawn(tokio::fs::File::from_std(stderr)),
        None => {
            let _ = process.shutdown(Duration::from_secs(1)).await;
            profile.destroy()?;
            return Err(
                NativeBrowserSandboxFailure::DebuggingPipeClosed { stderr_bytes: 0 }.into(),
            );
        }
    };
    let mut session = BrowserSession {
        _generation: generation,
        process,
        profile: Some(profile),
        command_writer,
        response_reader,
        stderr,
        next_id: 1,
        page_session_id: None,
    };
    match await_native_browser_sandbox(&mut session, deadline).await {
        Ok(_) => match session.attach_page_target().await {
            Ok(()) => Ok(session),
            Err(_) => {
                let failure =
                    BrowserSandboxError::Native(NativeBrowserSandboxFailure::InvalidFrame);
                let _ = session.shutdown().await;
                Err(failure)
            }
        },
        Err(proof_error) => {
            let _ = session.shutdown().await;
            Err(proof_error)
        }
    }
}

#[cfg(unix)]
async fn await_native_browser_sandbox(
    session: &mut BrowserSession,
    deadline: tokio::time::Instant,
) -> std::result::Result<BrowserVersion, BrowserSandboxError> {
    let mut command = br#"{"id":1,"method":"Browser.getVersion","params":{}}"#.to_vec();
    command.push(0);
    tokio::time::timeout_at(deadline, async {
        session
            .command_writer
            .write_all(&command)
            .await
            .map_err(|_| NativeBrowserSandboxFailure::DebuggingPipeClosed {
                stderr_bytes: session.stderr.byte_count(),
            })?;
        session.command_writer.flush().await.map_err(|_| {
            NativeBrowserSandboxFailure::DebuggingPipeClosed {
                stderr_bytes: session.stderr.byte_count(),
            }
        })?;
        loop {
            let response = tokio::select! {
                status = session.process.wait() => {
                    return Err(NativeBrowserSandboxFailure::ProcessExited {
                        code: status.ok(),
                        stderr_bytes: session.stderr.byte_count(),
                    });
                }
                response = read_cdp_frame(&mut session.response_reader, deadline) => response?,
            };
            if response.get("id").and_then(Value::as_u64) != Some(1) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(NativeBrowserSandboxFailure::CommandRejected);
            }
            let result = response
                .get("result")
                .ok_or(NativeBrowserSandboxFailure::InvalidFrame)?;
            let product = result
                .get("product")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(NativeBrowserSandboxFailure::InvalidFrame)?;
            let protocol_version = result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(NativeBrowserSandboxFailure::InvalidFrame)?;
            return Ok(BrowserVersion {
                product: product.to_owned(),
                protocol_version: protocol_version.to_owned(),
            });
        }
    })
    .await
    .map_err(|_| NativeBrowserSandboxFailure::Timeout {
        timeout_ms: BROWSER_STARTUP_TIMEOUT.as_millis() as u64,
    })?
    .map_err(Into::into)
}

async fn read_cdp_frame(
    reader: &mut tokio::fs::File,
    deadline: tokio::time::Instant,
) -> std::result::Result<Value, NativeBrowserSandboxFailure> {
    let mut frame = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout_at(deadline, reader.read(&mut byte))
            .await
            .map_err(|_| NativeBrowserSandboxFailure::Timeout {
                timeout_ms: BROWSER_STARTUP_TIMEOUT.as_millis() as u64,
            })?
            .map_err(|_| NativeBrowserSandboxFailure::InvalidFrame)?;
        if read == 0 {
            return Err(NativeBrowserSandboxFailure::DebuggingPipeClosed { stderr_bytes: 0 });
        }
        if byte[0] == 0 {
            break;
        }
        if frame.len() == BROWSER_PIPE_FRAME_LIMIT {
            return Err(NativeBrowserSandboxFailure::FrameTooLarge {
                limit: BROWSER_PIPE_FRAME_LIMIT,
            });
        }
        frame.push(byte[0]);
    }
    let text =
        std::str::from_utf8(&frame).map_err(|_| NativeBrowserSandboxFailure::InvalidFrame)?;
    serde_json::from_str(text).map_err(|_| NativeBrowserSandboxFailure::InvalidFrame)
}

fn reject_unsupported_browser_platform() -> std::result::Result<(), BrowserSandboxError> {
    if cfg!(target_os = "windows") {
        return Err(BrowserSandboxError::UnsupportedPlatform {
            platform: HostPlatform::Windows,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Page-script builders (selectors/text embedded as JSON string literals)
// ---------------------------------------------------------------------------

/// Returns the click target's viewport center (plus tag/text) or a `{ok:
/// false, error}` descriptor. Coordinates come from `getBoundingClientRect`,
/// which matches `Input.dispatchMouseEvent`'s viewport pixel space.
fn js_click(selector: &str) -> String {
    let selector = serde_json::to_string(selector).expect("selector serializes");
    format!(
        r#"( () => {{
  const sel = {selector};
  const el = document.querySelector(sel);
  if (!el) return {{ ok: false, error: "no element matches selector: " + sel }};
  el.scrollIntoView({{ block: "center", inline: "center" }});
  const r = el.getBoundingClientRect();
  if (r.width === 0 && r.height === 0) return {{ ok: false, error: "element is not visible" }};
  return {{
    ok: true,
    x: r.left + r.width / 2,
    y: r.top + r.height / 2,
    tag: el.tagName.toLowerCase(),
    text: String(el.innerText || el.value || "").slice(0, 200),
  }};
}})()"#
    )
}

/// Sets an input/textarea/contenteditable's value using the native prototype
/// setter (so React-style controlled inputs observe the change) and dispatches
/// `input`/`change` events.
fn js_fill(selector: &str, text: &str) -> String {
    let selector = serde_json::to_string(selector).expect("selector serializes");
    let text = serde_json::to_string(text).expect("text serializes");
    format!(
        r#"( () => {{
  const sel = {selector};
  const el = document.querySelector(sel);
  if (!el) return {{ ok: false, error: "no element matches selector: " + sel }};
  const tag = el.tagName.toLowerCase();
  if (tag !== "input" && tag !== "textarea" && !el.isContentEditable) {{
    return {{ ok: false, error: "element is not a text input" }};
  }}
  el.focus();
  if (el.isContentEditable) {{
    el.textContent = {text};
  }} else {{
    const proto = tag === "textarea" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(el, {text});
  }}
  el.dispatchEvent(new Event("input", {{ bubbles: true }}));
  el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  return {{ ok: true, tag: tag, value: el.isContentEditable ? el.textContent : el.value }};
}})()"#
    )
}

/// Extracts an element's value (input/textarea), else its inner text. `null`
/// when the selector matches nothing (caller turns that into an actionable
/// error). Tag-based so buttons/divs fall back to their text (buttons expose
/// an empty `.value` that would otherwise shadow innerText).
fn js_element_text(selector: &str) -> String {
    let selector = serde_json::to_string(selector).expect("selector serializes");
    format!(
        r#"( () => {{
  const sel = {selector};
  const el = document.querySelector(sel);
  if (!el) return null;
  const tag = el.tagName;
  const v = tag === "INPUT" || tag === "TEXTAREA" ? el.value : (el.innerText ?? el.textContent);
  return typeof v === "string" ? v : "";
}})()"#
    )
}

/// The whole page's visible text (`document.body.innerText`).
const JS_PAGE_TEXT: &str = r#"( () => document.body ? document.body.innerText : "" )()"#;

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[cfg(unix)]
/// Navigates the page to `url`, waits for the load to complete, and returns
/// the resulting title and final href.
async fn navigate_to(session: &mut BrowserSession, url: &str) -> Result<(String, String)> {
    let response = session
        .send_command("Page.navigate", json!({ "url": url }))
        .await?;
    if let Some(error_text) = response.get("errorText").and_then(Value::as_str) {
        if !error_text.is_empty() {
            bail!("browser: navigation failed: {error_text}");
        }
    }
    wait_for_load(session).await?;
    let title = session
        .eval_string("document.title")
        .await
        .unwrap_or_default();
    let href = session
        .eval_string("location.href")
        .await
        .unwrap_or_default();
    Ok((title, href))
}

#[cfg(unix)]
/// Waits for `document.readyState == "complete"`, tolerating the brief window
/// after navigation where the old execution context is being torn down.
async fn wait_for_load(session: &mut BrowserSession) -> Result<()> {
    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("browser: timed out waiting for the page to load");
        }
        match session.eval_string("document.readyState").await {
            Ok(state) if state == "complete" => return Ok(()),
            Ok(_) => {}
            Err(e) => {
                let message = e.to_string();
                if !(message.contains("Cannot find context") || message.contains("was destroyed")) {
                    return Err(e);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
/// Dispatches a real mouse press/release pair at viewport coordinates.
async fn mouse_click_at(session: &mut BrowserSession, x: f64, y: f64) -> Result<()> {
    for event_type in ["mousePressed", "mouseReleased"] {
        session
            .send_command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": event_type,
                    "x": x,
                    "y": y,
                    "button": "left",
                    "clickCount": 1,
                    "pointerType": "mouse",
                }),
            )
            .await?;
    }
    Ok(())
}

fn resolve_screenshot_path(workspace: &WorkspaceRoots, path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() || path.contains('\0') {
        bail!("browser: screenshot path is invalid");
    }
    let output = PathBuf::from(resolve_to_cwd(path, &workspace.cwd().to_string_lossy()));
    if !workspace
        .roots()
        .iter()
        .any(|root| output.starts_with(root))
    {
        bail!("browser: screenshot path must stay within the workspace");
    }
    if output.file_name().is_none() {
        bail!("browser: screenshot path must name a file");
    }
    Ok(output)
}

fn save_screenshot_bytes(
    fs: &Arc<PolicyFs>,
    output: &Path,
    bytes: Vec<u8>,
) -> std::result::Result<(), BrowserSandboxError> {
    let parent = output.parent().ok_or(FsError::InvalidPath)?;
    fs.create_dir_all(parent, FileMode::Shared)?;
    let expected = match fs.metadata(output) {
        Ok(metadata) => Some(metadata.identity),
        Err(FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }) => None,
        Err(error) => return Err(error.into()),
    };
    fs.replace_file(PolicyReplacement {
        path: output.to_path_buf(),
        expected,
        bytes,
        mode: FileMode::Shared,
    })?;
    Ok(())
}

fn screenshot_result(
    fs: &Arc<PolicyFs>,
    workspace: &WorkspaceRoots,
    output: &Path,
    bytes: Vec<u8>,
) -> Result<AgentToolResult> {
    if bytes.len() > SCREENSHOT_MAX_INPUT_BYTES {
        bail!(
            "browser: screenshot exceeds the {} MiB input limit",
            SCREENSHOT_MAX_INPUT_BYTES / 1024 / 1024
        );
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("browser: screenshot did not produce a PNG image");
    }
    let processed = process_image(&bytes, "image/png", true);
    if !processed.ok {
        bail!("browser: {}", processed.message.trim_matches(['[', ']']));
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&processed.data);
    if data.len() > SCREENSHOT_MAX_BASE64_BYTES {
        bail!("browser: screenshot exceeds the inline transport limit");
    }
    save_screenshot_bytes(fs, output, bytes).map_err(anyhow::Error::from)?;
    let display_path = workspace
        .roots()
        .iter()
        .find_map(|root| output.strip_prefix(root).ok())
        .ok_or_else(|| anyhow!("browser: screenshot output is outside the workspace"))?
        .to_string_lossy();
    let mime_type = processed.mime_type;
    Ok(AgentToolResult {
        content: vec![
            ContentBlock::text(format!(
                "Screenshot saved to {} ({} {})",
                display_path,
                format_size(processed.data.len()),
                mime_type
            )),
            ContentBlock::Image { data, mime_type },
        ],
        ..Default::default()
    })
}

#[cfg(unix)]
async fn run_action(
    session: &mut BrowserSession,
    action: &Action,
    args: &Value,
    workspace: &WorkspaceRoots,
    fs: &Arc<PolicyFs>,
) -> Result<AgentToolResult> {
    // Each call spawns a fresh browser starting at about:blank, so non-
    // navigate actions accept an optional `url` to navigate first — a single
    // call can then operate on a real page end to end.
    if !matches!(action, Action::Navigate) {
        let url = arg_str(args, "url");
        if !url.trim().is_empty() {
            navigate_to(session, &url).await?;
        }
    }
    match action {
        Action::Navigate => {
            let url = arg_str(args, "url");
            let (title, href) = navigate_to(session, &url).await?;
            Ok(text_result(format!(
                "Navigated to {url}\nTitle: {title}\nURL: {href}"
            )))
        }
        Action::Click => {
            let selector = arg_str(args, "selector");
            let info = session.eval_value(&js_click(&selector)).await?;
            if info.get("ok").and_then(Value::as_bool) != Some(true) {
                let message = info
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("click failed");
                bail!("browser: {message}");
            }
            let x = info.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = info.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let tag = info.get("tag").and_then(Value::as_str).unwrap_or("element");
            let text = info.get("text").and_then(Value::as_str).unwrap_or("");
            // A freshly connected headless target may not hold input focus;
            // without it the mouse press/release never synthesizes a click.
            // `bringToFront` first activates the target so Input events land.
            session.send_command("Page.bringToFront", json!({})).await?;
            mouse_click_at(session, x, y).await?;
            let mut parts = vec![format!("Clicked {tag} at ({x:.0}, {y:.0})")];
            if !text.is_empty() {
                parts.push(format!("— \"{text}\""));
            }
            // The click handler runs asynchronously in the renderer, so poll
            // briefly for the element's text to change before reporting.
            let mut new_text = String::new();
            let deadline = Instant::now() + Duration::from_millis(2000);
            while Instant::now() < deadline {
                if let Ok(after) = session.eval_value(&js_element_text(&selector)).await {
                    if let Some(t) = after.as_str() {
                        new_text = t.to_string();
                        if !new_text.is_empty() && new_text != text {
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !new_text.is_empty() && new_text != text {
                parts.push(format!("→ \"{new_text}\""));
            }
            Ok(text_result(parts.join(" ")))
        }
        Action::Fill => {
            let selector = arg_str(args, "selector");
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let info = session.eval_value(&js_fill(&selector, text)).await?;
            if info.get("ok").and_then(Value::as_bool) != Some(true) {
                let message = info
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("fill failed");
                bail!("browser: {message}");
            }
            let tag = info.get("tag").and_then(Value::as_str).unwrap_or("element");
            let value = info.get("value").and_then(Value::as_str).unwrap_or("");
            Ok(text_result(format!(
                "Filled {tag} ({selector}) — value now {value:?}"
            )))
        }
        Action::Screenshot => {
            let path = arg_str(args, "path");
            let response = session
                .send_command("Page.captureScreenshot", json!({ "format": "png" }))
                .await?;
            let b64 = response
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("browser: screenshot returned no image data"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("browser: screenshot data was not valid base64")?;
            let output = resolve_screenshot_path(workspace, &path)?;
            screenshot_result(fs, workspace, &output, bytes)
        }
        Action::Extract => {
            let selector = arg_str(args, "selector");
            let expression = if selector.is_empty() {
                JS_PAGE_TEXT.to_string()
            } else {
                js_element_text(&selector)
            };
            let value = session.eval_value(&expression).await?;
            let Some(text) = value.as_str() else {
                if !selector.is_empty() {
                    bail!("browser: no element matches selector: {selector}");
                }
                bail!("browser: page produced no text");
            };
            let truncated = truncate_head(text, usize::MAX, EXTRACT_MAX_BYTES);
            let mut out = truncated.content;
            if truncated.truncated {
                out.push_str(&format!(
                    "\n[truncated: showing {} of {} bytes]",
                    truncated.output_bytes, truncated.total_bytes
                ));
            }
            Ok(text_result(out))
        }
        Action::ListTabs => {
            let targets = session
                .send_browser_command("Target.getTargets", json!({}))
                .await?;
            let pages: Vec<&Value> = targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|target| target.get("type").and_then(Value::as_str) == Some("page"))
                .take(LIST_TABS_MAX)
                .collect();
            if pages.is_empty() {
                return Ok(text_result("No tabs"));
            }
            let mut lines = vec![format!("{} tab(s):", pages.len())];
            for (index, target) in pages.iter().enumerate() {
                let title = target.get("title").and_then(Value::as_str).unwrap_or("");
                let mut url = target
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if url.chars().count() > 200 {
                    url = format!("{}...", url.chars().take(200).collect::<String>());
                }
                lines.push(format!("{}. {title} — {url}", index + 1));
            }
            Ok(text_result(lines.join("\n")))
        }
        Action::Close => Ok(text_result("browser closed")),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Executes one generation-bound browser action.
async fn run_browser_for_workspace(
    generation: Arc<ApplicationGeneration>,
    workspace: &WorkspaceRoots,
    args: Value,
    approval_token: Option<ToolCallApprovalToken>,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    let deadline = tokio::time::Instant::now() + BROWSER_STARTUP_TIMEOUT;
    reject_unsupported_browser_platform()?;
    check_aborted(&abort)?;
    let action_input = arg_str(&args, "action");
    let action = parse_action(action_input.trim())?;
    validate_args(&action, &args)?;
    if action == Action::Close {
        return Ok(text_result("browser closed"));
    }
    #[cfg(not(unix))]
    {
        let _ = (generation, workspace, args, approval_token, abort, deadline);
        return Err(BrowserSandboxError::UnsupportedPlatform {
            platform: HostPlatform::Windows,
        }
        .into());
    }
    #[cfg(unix)]
    {
        let profile_id = browser_profile_id(&args)?;
        let profile_path = generation
            .sandbox()
            .scratch()
            .browser_profile_path(profile_id)?;
        let binary = run_trusted_tool_probe(
            &generation,
            TrustedToolProbe::Browser {
                explicit: explicit_browser_path()?,
            },
        )?;
        let argv = browser_argv(&binary, &profile_path);
        let pipes = browser_debugging_pipes()?;
        let target_fd_mappings = pipes.canonical_mappings().to_vec();
        let (owned_target_fd_mappings, debugger_writer, debugger_reader) = pipes.into_parts();
        let token =
            approval_token.ok_or_else(|| anyhow!("browser: launch approval is required"))?;
        let request = RawSpawnRequest {
            authorization: SpawnAuthorization::Approved(token),
            family: SpawnFamily::Browser,
            audience: browser_audience()?,
            argv: argv.into_iter().map(OsString::from).collect(),
            cwd: workspace.cwd().to_path_buf(),
            requested_env: BTreeMap::new(),
            requested_grants: BTreeSet::new(),
            stdio: StdioMode::Piped,
            terminal_size: None,
            timeout_ms: Some(BROWSER_STARTUP_TIMEOUT.as_millis() as u64),
            target_fd_mappings,
            owned_target_fd_mappings,
            #[cfg(unix)]
            mount_sources: Vec::new(),
            #[cfg(unix)]
            retained_executable: None,
            #[cfg(unix)]
            retained_working_directory: None,
        };
        let authorized = authorize_spawn(generation.clone(), request).await?;
        let profile = generation
            .sandbox()
            .scratch()
            .create_browser_profile(profile_id)?;
        let fs = generation.sandbox().fs().confined()?.clone();
        let mut session = launch_browser_pipe(
            generation,
            authorized,
            profile,
            debugger_writer,
            debugger_reader,
            deadline,
        )
        .await?;
        let outcome = tokio::select! {
            result = tokio::time::timeout(
                ACTION_TIMEOUT,
                run_action(&mut session, &action, &args, workspace, &fs),
            ) => match result {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "browser: action {action_input:?} timed out after {}s",
                    ACTION_TIMEOUT.as_secs()
                )),
            },
            _ = abort.cancelled() => Err(anyhow!("Operation aborted")),
        };
        let cleanup = session.shutdown().await;
        match (outcome, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Ok(result), Ok(())) => Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_accepts_all_documented_actions() {
        for (input, expected) in [
            ("navigate", Action::Navigate),
            ("click", Action::Click),
            ("fill", Action::Fill),
            ("screenshot", Action::Screenshot),
            ("extract", Action::Extract),
            ("list_tabs", Action::ListTabs),
            ("close", Action::Close),
        ] {
            assert_eq!(parse_action(input).unwrap(), expected, "{input}");
        }
        assert!(parse_action("").is_err());
        assert!(parse_action("hover").is_err());
    }

    #[test]
    fn url_validation_accepts_supported_schemes_and_rejects_others() {
        for url in [
            "http://example.com/",
            "https://example.com/path?q=1",
            "data:text/html,<h1>hi</h1>",
            "file:///tmp/page.html",
            "about:blank",
        ] {
            assert!(validate_url(url).is_ok(), "{url}");
        }
        for url in [
            "",
            "javascript:alert(1)",
            "ftp://example.com",
            "example.com",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn prepare_arguments_replaces_caller_profile_identifier() {
        let arguments = inject_browser_profile_id(json!({
            "action": "list_tabs",
            BROWSER_PROFILE_ID_KEY: "caller-controlled"
        }))
        .unwrap();
        let id = browser_profile_id(&arguments).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.as_bytes()[0].is_ascii_lowercase());
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );
        assert_ne!(id, "caller-controlled");
    }

    #[test]
    fn browser_target_mapping_contract_is_exact() {
        let mappings = browser_target_fd_mappings().unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].target(), 3);
        assert_eq!(mappings[0].direction(), TargetFdDirection::ParentWrites);
        assert_eq!(mappings[1].target(), 4);
        assert_eq!(mappings[1].direction(), TargetFdDirection::ParentReads);
    }

    #[cfg(unix)]
    #[test]
    fn browser_argv_uses_pipe_and_forbids_sandbox_bypasses() {
        let profile = SandboxVisiblePath::for_browser_test("/run/pi/scratch/browser-profile/test");
        let argv = build_browser_argv(&profile);
        assert!(
            argv.iter()
                .any(|argument| argument == "--remote-debugging-pipe")
        );
        assert!(argv.iter().any(|argument| argument == "about:blank"));
        for forbidden in [
            "--no-sandbox",
            "--disable-sandbox",
            "--disable-setuid-sandbox",
            "--disable-seccomp-filter-sandbox",
            "--disable-namespace-sandbox",
            "--single-process",
            "--no-zygote",
            "--remote-debugging-port",
            "--remote-allow-origins",
        ] {
            assert!(
                argv.iter().all(|argument| argument != forbidden
                    && !argument.starts_with(&format!("{forbidden}="))),
                "forbidden browser switch present: {forbidden}"
            );
        }
    }

    #[test]
    fn browser_errors_are_redacted() {
        let secret = ["s", "k-", "abcdefghijklmnop1234567890"].concat();
        let errors = [
            NativeBrowserSandboxFailure::Timeout { timeout_ms: 15_000 },
            NativeBrowserSandboxFailure::ProcessExited {
                code: Some(1),
                stderr_bytes: secret.len(),
            },
            NativeBrowserSandboxFailure::DebuggingPipeClosed {
                stderr_bytes: secret.len(),
            },
            NativeBrowserSandboxFailure::FrameTooLarge {
                limit: BROWSER_PIPE_FRAME_LIMIT,
            },
            NativeBrowserSandboxFailure::InvalidFrame,
            NativeBrowserSandboxFailure::CommandRejected,
        ];
        for error in errors {
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(!display.contains(&secret));
            assert!(!debug.contains(&secret));
        }
    }

    #[test]
    fn browser_tool_routes_launch_through_prepare_approval_token_authority() {
        // Native sandbox contract: the browser tool binds `prepare_approval`
        // so the host approval hook (wired as the final `approve_tool_call`)
        // is the SOLE minting path for the sandbox authorization token —
        // `prepare_browser_approval` builds the `ToolApprovalRequest` whose
        // `issue()` closure calls `approvals.issue(...)` against the native
        // sandbox, and `run_browser_for_workspace` then hard-requires the
        // resulting token before any spawn. There is no parallel boolean
        // authority: a tool without `prepare_approval` would never reach the
        // final approval hook and could not mint a token.
        let workspace = WorkspaceRoots::for_tool_factory("/tmp/browser-approval-contract");
        let capture: GenerationCaptureFn = Arc::new(|| bail!("capture must not run here"));
        let tool = browser_tool_for_workspace(workspace, capture);
        assert_eq!(tool.capability, ToolCapability::Exec);
        assert!(
            tool.prepare_approval.is_some(),
            "browser must prepare an approval request so the host approval hook \
             mints the final native sandbox token"
        );
    }
}
