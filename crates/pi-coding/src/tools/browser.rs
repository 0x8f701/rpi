//! `browser` tool: headless Chromium/Chrome automation over the Chrome
//! DevTools Protocol (CDP).
//!
//! Each invocation spawns a fresh headless browser process with a temporary
//! profile, performs exactly one action (navigate / click / fill / screenshot
//! / extract / list_tabs / close), then tears the browser down. State does
//! not persist between calls.
//!
//! The CDP client is deliberately minimal and hand-rolled over
//! `tokio-tungstenite` (already in the workspace dependency tree; see
//! `Cargo.lock`) instead of pulling in `chromiumoxide` (which would add a
//! large transitive subtree for an API surface this tool does not need).
//! Commands are strictly sequential: one request is in flight at a time and
//! its response is matched by `id`, so stale responses (e.g. from a
//! timed-out earlier command) are skipped harmlessly.
//!
//! ## Sandbox trade-off
//!
//! The browser is launched with `--no-sandbox` because Chromium's sandbox
//! routinely fails in containers and when running as root, which would make
//! the tool unusable in those environments. Disabling it removes Chromium's
//! process-level isolation, so the tool bounds its own blast radius instead:
//! navigation is agent-controlled and restricted to validated
//! `http(s)`/`about:blank` URLs; each call spawns a short-lived browser with
//! a fresh temporary profile that performs exactly one action and is torn
//! down afterwards; and the process is hard-killed on drop (`kill_on_drop`),
//! as a process group (`process_group(0)` on unix), and by the action
//! timeout ([`ACTION_TIMEOUT`]). A compromised page can therefore only act
//! within a single action's window, never outlive the call, and never
//! outlive the tool process.
//!
//! Requires a Chrome/Chromium binary on the host. Discovery order:
//! `CHROME_PATH`, well-known install locations, then `PATH`. A missing
//! binary is rejected with an actionable message (like the `pdftotext` path
//! in `tools.rs`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};
use pi_ai::ContentBlock;

use super::{arg_str, check_aborted, imageresize::process_image, paths::resolve_scoped_path, s_object, s_string, text_result};
use crate::truncate::{format_size, truncate_head};
use crate::WorkspaceRoots;

/// Wall-clock budget for a single action, including browser startup wait.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a single CDP command round trip (read until the matching id).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for waiting out a navigation (polling `document.readyState`).
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the browser process to print its DevTools endpoint.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(15);
/// Output byte budget for `extract` text — keeps agent context lean.
const EXTRACT_MAX_BYTES: usize = 16 * 1024;
/// Cap on the number of tabs rendered by `list_tabs`.
const LIST_TABS_MAX: usize = 20;
/// Raw screenshot cap before image decoding/resizing.
const SCREENSHOT_MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
/// Inline base64 budget leaves headroom inside the 4 MiB listener frame cap.
const SCREENSHOT_MAX_BASE64_BYTES: usize = 3 * 1024 * 1024;
/// Error shown when no Chrome/Chromium binary can be found.
const MISSING_CHROME_MESSAGE: &str = "\
browser tool requires Chrome/Chromium, but none was found on this machine. \
Set the CHROME_PATH environment variable to a Chrome or Chromium executable, \
or install one on PATH (google-chrome, chromium, ...). Well-known locations \
are checked automatically (/usr/bin/google-chrome, /usr/bin/chromium, \
/snap/bin/chromium, /opt/google/chrome/chrome, /usr/lib/chromium/chromium, \
and the macOS .app bundles). The browser always runs headless.";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

/// Builds the `browser` tool. `cwd` anchors workspace-scoped screenshot paths.
pub(crate) fn browser_tool_for_workspace(workspace: WorkspaceRoots) -> AgentTool {
    let cwd = workspace.cwd().to_string_lossy().into_owned();
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
            ("selector", s_string("CSS selector for click / fill / extract")),
            ("text", s_string("Text to fill into the matched input (fill only)")),
            (
                "path",
                s_string("Workspace-relative output path for the screenshot PNG (screenshot only)"),
            ),
        ],
        vec!["action"],
    );
    let description = format!(
        "Automate a headless Chromium/Chrome browser over the Chrome DevTools Protocol. \
Actions: navigate (url), click (selector), fill (selector, text), screenshot (path), \
extract (selector? — page or element text), list_tabs, close. \
Each call spawns a fresh headless browser starting at about:blank and state does not persist \
between calls, so click/fill/screenshot/extract accept an optional url to navigate first \
(one call can then act on a page end to end). \
Requires a Chrome/Chromium binary on this machine (CHROME_PATH, PATH, or standard install \
locations); missing binaries are rejected with an actionable error. \
Screenshots are saved only within the workspace and returned as bounded inline image content. \
Outputs are bounded to {}KB.",
        EXTRACT_MAX_BYTES / 1024
    );
    AgentTool::new("browser", description, params, move |ctx| {
        let workspace = workspace.clone();
        let cwd = cwd.clone();
        async move { run_browser_for_workspace(&cwd, &workspace, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Exec)
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

/// Candidate browser binaries in priority order: `CHROME_PATH` first, then
/// well-known Linux and macOS install locations.
fn chrome_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(path) = std::env::var("CHROME_PATH") {
        let path = path.trim();
        if !path.is_empty() {
            out.push(PathBuf::from(path));
        }
    }
    out.extend(
        [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/opt/google/chrome/chrome",
            "/usr/lib/chromium/chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    out
}

/// The first candidate that exists as a file. Injectable seam for tests.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

/// Finds a usable browser binary: `CHROME_PATH` / common locations, then PATH.
fn discover_chrome() -> Option<PathBuf> {
    if let Some(bin) = first_existing(&chrome_candidates()) {
        return Some(bin);
    }
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"] {
        if let Some(path) = super::look_path(name) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Browser lifecycle
// ---------------------------------------------------------------------------

/// A spawned headless browser plus its CDP page connection. Dropping it kills
/// the process (`kill_on_drop`) and removes the temporary profile.
struct BrowserSession {
    child: Child,
    user_data_dir: PathBuf,
    ws: Option<WsStream>,
    next_id: u64,
    port: u16,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        // kill_on_drop(true) on `child` SIGKILLs the browser when it drops;
        // its renderer/zygote children exit with it. Profile cleanup is
        // best-effort (the shutdown path removes it eagerly).
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }
}

impl BrowserSession {
    /// Kills the browser, reaps it, and removes the temporary profile.
    async fn shutdown(mut self) {
        let _ = self.ws.take();
        let _ = self.child.kill().await;
        // `wait` reaps the pid and nulls tokio's inner handle, so the
        // `kill_on_drop` drop guard becomes a no-op afterwards (a reaped pid
        // is never signaled again).
        let _ = self.child.wait().await;
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }

    /// Sends one CDP command and waits for the response with a matching `id`.
    /// Events and stale responses (from an earlier timed-out command) are
    /// skipped, which keeps the client correct across timeouts.
    async fn send_command(&mut self, method: &str, params: Value) -> Result<Value> {
        let ws = self.ws.as_mut().expect("websocket open");
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        ws.send(WsMessage::Text(message.into()))
            .await
            .map_err(|e| anyhow!("browser: CDP send failed: {e}"))?;
        loop {
            match tokio::time::timeout(COMMAND_TIMEOUT, ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    let response: Value = serde_json::from_str(&text)
                        .map_err(|e| anyhow!("browser: invalid CDP response: {e}"))?;
                    if response.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = response.get("error") {
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown CDP error");
                            return Err(anyhow!("browser: CDP {method} failed: {message}"));
                        }
                        return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Event or another command's reply — skip.
                }
                Ok(Some(Ok(_))) => continue, // binary / ping / pong
                Ok(Some(Err(e))) => return Err(anyhow!("browser: CDP connection error: {e}")),
                Ok(None) => return Err(anyhow!("browser: CDP connection closed")),
                Err(_) => return Err(anyhow!("browser: CDP command {method} timed out")),
            }
        }
    }

    /// Runs `expression` in the page's main world, returning the JSON value.
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
        if let Some(exc) = response.get("exceptionDetails") {
            let text = exc.get("text").and_then(Value::as_str).unwrap_or("page script threw");
            return Err(anyhow!("browser: page script error: {text}"));
        }
        Ok(response
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Convenience wrapper for `eval_value` when the value is a string.
    async fn eval_string(&mut self, expression: &str) -> Result<String> {
        match self.eval_value(expression).await? {
            Value::String(s) => Ok(s),
            Value::Null => Ok(String::new()),
            other => Ok(other.to_string()),
        }
    }
}

/// Spawns the browser and connects to the initial page's DevTools endpoint.
async fn spawn_browser(bin: &Path) -> Result<BrowserSession> {
    let user_data_dir =
        std::env::temp_dir().join(format!("pi-browser-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&user_data_dir).with_context(|| {
        format!("browser: failed to create profile dir {}", user_data_dir.display())
    })?;

    let args = vec![
        "--headless=new".to_string(),
        "--remote-debugging-port=0".to_string(),
        "--remote-allow-origins=*".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-extensions".to_string(),
        "--disable-sync".to_string(),
        "--disable-component-update".to_string(),
        "--disable-gpu".to_string(),
        "--disable-dev-shm-usage".to_string(),
        // --no-sandbox: Chromium's sandbox fails in containers/root; see the
        // module header for the trade-off and the lifecycle bounds that
        // contain it (short-lived process, kill_on_drop, process group,
        // ACTION_TIMEOUT).
        "--no-sandbox".to_string(),
        "--window-size=1280,900".to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
        "about:blank".to_string(),
    ];
    let mut command = Command::new(bin);
    command.args(&args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = super::spawn_with_etxtbsy_retry(&mut command, "chrome")
        .await
        .with_context(|| format!("browser: failed to launch {} (is it executable?)", bin.display()))?;

    // Chrome prints `DevTools listening on ws://127.0.0.1:PORT/devtools/browser/...`
    // on stderr. Read until it appears (or the process exits).
    let stderr = child.stderr.take().expect("piped stderr");
    let mut lines = BufReader::new(stderr).lines();
    let mut tail: Vec<String> = Vec::new();
    let mut ws_url: Option<String> = None;
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if tail.len() >= 8 {
                    tail.remove(0);
                }
                tail.push(line.clone());
                if let Some(pos) = line.find("DevTools listening on ws://") {
                    ws_url =
                        Some(line[pos + "DevTools listening on ".len()..].trim().to_string());
                    break;
                }
            }
            Ok(Ok(None)) => break, // browser exited
            Ok(Err(e)) => bail!("browser: failed reading chrome output: {e}"),
            Err(_) => continue,
        }
    }
    let Some(ws_url) = ws_url else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = std::fs::remove_dir_all(&user_data_dir);
        bail!(
            "browser: {} did not expose a DevTools endpoint (it exited or is too old \
             for --headless=new). Last stderr:\n{}",
            bin.display(),
            tail.join("\n")
        );
    };
    // Keep draining stderr in the background so the pipe never fills.
    tokio::spawn(async move {
        while lines.next_line().await.ok().flatten().is_some() {}
    });

    let port = ws_url
        .split('/')
        .nth(2)
        .and_then(|host| host.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("browser: could not parse DevTools port from {ws_url}"))?;
    let page_ws = fetch_page_websocket(port).await?;
    let (ws, _) = connect_async(page_ws.as_str())
        .await
        .map_err(|e| anyhow!("browser: failed to connect to DevTools websocket: {e}"))?;

    Ok(BrowserSession {
        child,
        user_data_dir,
        ws: Some(ws),
        next_id: 0,
        port,
    })
}

/// GETs `/json/list` from the DevTools HTTP endpoint (retrying briefly while
/// the HTTP server catches up to the stderr announcement).
async fn fetch_json_list(port: u16) -> Result<Vec<Value>> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = reqwest::get(&url).await;
        if let Ok(response) = response {
            if response.status().is_success() {
                return response
                    .json::<Vec<Value>>()
                    .await
                    .context("browser: invalid /json/list response");
            }
        }
        if Instant::now() >= deadline {
            bail!("browser: DevTools HTTP endpoint did not come up on port {port}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Finds the initial page target's `webSocketDebuggerUrl` on `/json/list`.
async fn fetch_page_websocket(port: u16) -> Result<String> {
    let targets = fetch_json_list(port).await?;
    for target in &targets {
        if target.get("type").and_then(Value::as_str) == Some("page") {
            let ws = target
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !ws.is_empty() {
                return Ok(ws.to_string());
            }
        }
    }
    bail!("browser: no page target found on the DevTools endpoint")
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

/// Navigates the page to `url`, waits for the load to complete, and returns
/// the resulting title and final href.
async fn navigate_to(session: &mut BrowserSession, url: &str) -> Result<(String, String)> {
    let response = session.send_command("Page.navigate", json!({ "url": url })).await?;
    if let Some(error_text) = response.get("errorText").and_then(Value::as_str) {
        if !error_text.is_empty() {
            bail!("browser: navigation failed: {error_text}");
        }
    }
    wait_for_load(session).await?;
    let title = session.eval_string("document.title").await.unwrap_or_default();
    let href = session.eval_string("location.href").await.unwrap_or_default();
    Ok((title, href))
}

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
    let output = PathBuf::from(resolve_scoped_path(path, workspace)?);
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("browser: screenshot path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!("browser: failed to create screenshot directory {}", parent.display())
    })?;
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("browser: failed to resolve screenshot directory {}", parent.display()))?;
    if !workspace.roots().iter().any(|root| canonical_parent.starts_with(root)) {
        bail!("browser: screenshot path must stay within the workspace");
    }
    if output.exists() {
        let canonical_output = std::fs::canonicalize(&output)
            .with_context(|| format!("browser: failed to resolve screenshot path {}", output.display()))?;
        if !workspace.roots().iter().any(|root| canonical_output.starts_with(root)) {
            bail!("browser: screenshot path must stay within the workspace");
        }
    }
    Ok(output)
}

fn save_screenshot_bytes(workspace: &WorkspaceRoots, output: &Path, bytes: &[u8]) -> Result<()> {
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("browser: screenshot path has no parent"))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("browser: failed to resolve screenshot directory {}", parent.display()))?;
    if !workspace.roots().iter().any(|root| canonical_parent.starts_with(root)) {
        bail!("browser: screenshot directory now resolves outside the workspace");
    }
    let name = output
        .file_name()
        .ok_or_else(|| anyhow!("browser: screenshot path must name a file"))?;
    let final_path = canonical_parent.join(name);
    if std::fs::symlink_metadata(&final_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("browser: screenshot target must not be a symlink");
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&final_path)
        .with_context(|| format!("browser: failed to write screenshot to {}", output.display()))?;
    use std::io::Write as _;
    file.write_all(bytes)
        .with_context(|| format!("browser: failed to write screenshot to {}", output.display()))
}
fn screenshot_result(
    workspace: &WorkspaceRoots,
    output: &Path,
    bytes: Vec<u8>,
) -> Result<AgentToolResult> {
    if bytes.len() > SCREENSHOT_MAX_INPUT_BYTES {
        bail!("browser: screenshot exceeds the {} MiB input limit", SCREENSHOT_MAX_INPUT_BYTES / 1024 / 1024);
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
    save_screenshot_bytes(workspace, output, &bytes)?;
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

async fn run_action(
    session: &mut BrowserSession,
    action: &Action,
    args: &Value,
    workspace: &WorkspaceRoots,
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
            Ok(text_result(format!("Navigated to {url}\nTitle: {title}\nURL: {href}")))
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
            Ok(text_result(format!("Filled {tag} ({selector}) — value now {value:?}")))
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
            screenshot_result(workspace, &output, bytes)
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
            let targets = fetch_json_list(session.port).await?;
            let pages: Vec<&Value> = targets
                .iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .take(LIST_TABS_MAX)
                .collect();
            if pages.is_empty() {
                return Ok(text_result("No tabs"));
            }
            let mut lines = vec![format!("{} tab(s):", pages.len())];
            for (index, target) in pages.iter().enumerate() {
                let title = target.get("title").and_then(Value::as_str).unwrap_or("");
                let mut url = target.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                if url.chars().count() > 200 {
                    let truncated: String = url.chars().take(200).collect();
                    url = format!("{truncated}...");
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

/// Executes one `browser` action: validates, spawns a fresh headless browser,
/// runs the action, and tears the browser down (also on errors/timeouts/abort).
pub(crate) async fn run_browser(cwd: &str, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    let workspace = WorkspaceRoots::for_tool_factory(cwd);
    run_browser_for_workspace(cwd, &workspace, args, abort).await
}

async fn run_browser_for_workspace(
    cwd: &str,
    workspace: &WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    run_browser_with(cwd, workspace, args, abort, None).await
}

/// `run_browser` with an injectable binary path so tests can exercise the
/// missing-binary rejection deterministically and skip-guard the real smoke.
async fn run_browser_with(
    cwd: &str,
    workspace: &WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
    chrome_bin: Option<PathBuf>,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action_input = arg_str(&args, "action");
    let action = parse_action(action_input.trim())?;
    validate_args(&action, &args)?;

    // Calls are stateless: every non-close action owns a fresh browser and
    // tears it down before returning. `close` therefore has nothing to spawn
    // or discover; treating it as an idempotent acknowledgement keeps the
    // advertised action valid even when Chrome is unavailable.
    if action == Action::Close {
        return Ok(text_result("browser closed"));
    }

    let bin = match chrome_bin {
        Some(path) if path.is_file() => path,
        Some(path) => bail!("browser: Chrome/Chromium binary not found at {}", path.display()),
        None => discover_chrome().ok_or_else(|| anyhow!(MISSING_CHROME_MESSAGE))?,
    };

    let mut session = spawn_browser(&bin).await?;
    let outcome = tokio::select! {
        res = tokio::time::timeout(ACTION_TIMEOUT, run_action(&mut session, &action, &args, workspace)) => {
            match res {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "browser: action {action_input:?} timed out after {}s",
                    ACTION_TIMEOUT.as_secs()
                )),
            }
        }
        _ = abort.cancelled() => Err(anyhow!("Operation aborted")),
    };
    session.shutdown().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    fn image_of(result: &AgentToolResult) -> Option<(&str, &str)> {
        result.content.iter().find_map(|block| match block {
            ContentBlock::Image { data, mime_type } => Some((data.as_str(), mime_type.as_str())),
            _ => None,
        })
    }

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
        assert!(parse_action(" NAVIGATE").is_err());
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
        for url in ["", "   ", "javascript:alert(1)", "ftp://example.com", "not a url", "example.com"] {
            assert!(validate_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn validate_args_requires_action_specific_fields() {
        assert!(validate_args(&Action::Navigate, &json!({})).is_err());
        assert!(validate_args(&Action::Navigate, &json!({ "url": "data:text/html,hi" })).is_ok());

        assert!(validate_args(&Action::Click, &json!({})).is_err());
        assert!(validate_args(&Action::Click, &json!({ "selector": "#a" })).is_ok());

        assert!(validate_args(&Action::Fill, &json!({ "selector": "#a" })).is_err());
        assert!(validate_args(&Action::Fill, &json!({ "selector": "#a", "text": "x" })).is_ok());
        // Empty text is legal (clears an input), but the key must be present.
        assert!(validate_args(&Action::Fill, &json!({ "selector": "#a", "text": "" })).is_ok());

        assert!(validate_args(&Action::Screenshot, &json!({})).is_err());
        assert!(validate_args(&Action::Screenshot, &json!({ "path": "shot.png" })).is_ok());

        for action in [Action::Extract, Action::ListTabs, Action::Close] {
            assert!(validate_args(&action, &json!({})).is_ok());
        }

        // Non-navigate actions accept an optional `url` (navigate first) that
        // must itself be valid when present.
        assert!(validate_args(&Action::Extract, &json!({ "url": "data:text/html,hi" })).is_ok());
        assert!(validate_args(&Action::Click, &json!({ "selector": "#a", "url": "javascript:1" })).is_err());
        assert!(validate_args(&Action::Screenshot, &json!({ "path": "s.png", "url": "not a url" })).is_err());
    }

    #[test]
    fn chrome_discovery_prefers_first_existing_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("chrome-real");
        fs::write(&real, b"#!").unwrap();
        let missing = dir.path().join("chrome-missing");
        let candidates = vec![missing.clone(), real.clone()];
        assert_eq!(first_existing(&candidates), Some(real));
        assert_eq!(first_existing(&[missing]), None);
        assert_eq!(first_existing(&[]), None);
    }

    #[test]
    fn screenshot_path_resolution_is_workspace_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).unwrap();
        let output = resolve_screenshot_path(&workspace, "shots/a.png").unwrap();
        assert_eq!(output, cwd.join("shots/a.png"));
        assert!(output.parent().unwrap().is_dir());
        assert!(resolve_screenshot_path(&workspace, "../outside.png").is_err());
        let absolute = Path::new(std::path::MAIN_SEPARATOR_STR).join("outside.png");
        assert!(resolve_screenshot_path(&workspace, absolute.to_str().unwrap()).is_err());
    }

    #[test]
    fn screenshot_result_rejects_hostile_and_oversize_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("shot.png");
        let workspace = WorkspaceRoots::new(dir.path(), Vec::<PathBuf>::new()).unwrap();
        assert!(screenshot_result(&workspace, &output, b"not an image".to_vec()).is_err());
        assert!(screenshot_result(&workspace, &output, vec![0; SCREENSHOT_MAX_INPUT_BYTES + 1]).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn screenshot_path_accepts_nested_additional_workspace_root() {
        let cwd = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        let workspace = WorkspaceRoots::new(cwd.path(), [additional.path()]).unwrap();
        let output = additional.path().join("captures").join("shot.png");
        let resolved = resolve_screenshot_path(&workspace, output.to_str().unwrap()).unwrap();
        assert_eq!(resolved, output);
        assert!(output.parent().unwrap().is_dir());
        let image = image::DynamicImage::new_rgb8(2, 2);
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let result = screenshot_result(&workspace, &resolved, bytes).unwrap();
        assert!(!text_of(&result).contains(additional.path().to_string_lossy().as_ref()));
        assert!(text_of(&result).contains("captures/shot.png"));
    }

    #[test]
    fn screenshot_result_returns_bounded_png_content() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("shot.png");
        let image = image::DynamicImage::new_rgb8(2, 2);
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let workspace = WorkspaceRoots::new(dir.path(), Vec::<PathBuf>::new()).unwrap();
        let result = screenshot_result(&workspace, &output, bytes).unwrap();
        let (data, mime_type) = image_of(&result).expect("inline screenshot image");
        assert_eq!(mime_type, "image/png");
        assert!(data.len() <= SCREENSHOT_MAX_BASE64_BYTES);
        assert!(output.is_file());
        assert!(!text_of(&result).contains(output.to_string_lossy().as_ref()));
    }

    #[cfg(unix)]
    #[test]
    fn screenshot_write_rejects_final_symlink() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let output = workspace_dir.path().join("shot.png");
        std::os::unix::fs::symlink(outside.path(), &output).unwrap();
        let workspace = WorkspaceRoots::new(workspace_dir.path(), Vec::<PathBuf>::new()).unwrap();
        let error = save_screenshot_bytes(&workspace, &output, b"png").unwrap_err().to_string();
        assert!(error.contains("must not be a symlink"), "{error}");
    }

    #[tokio::test]
    async fn missing_chrome_binary_yields_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = WorkspaceRoots::new(dir.path(), Vec::<PathBuf>::new()).unwrap();
        let result = run_browser_with(
            dir.path().to_str().unwrap(),
            &workspace,
            json!({ "action": "list_tabs" }),
            AbortSignal::none(),
            Some(PathBuf::from("/nonexistent/pi-chrome")),
        )
        .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Chrome/Chromium binary not found"), "{err}");
    }

    #[tokio::test]
    async fn close_is_idempotent_without_chrome() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = WorkspaceRoots::new(dir.path(), Vec::<PathBuf>::new()).unwrap();
        let result = run_browser_with(
            dir.path().to_str().unwrap(),
            &workspace,
            json!({ "action": "close" }),
            AbortSignal::none(),
            Some(PathBuf::from("/nonexistent/pi-chrome")),
        )
        .await
        .expect("stateless close");
        assert!(text_of(&result).contains("browser closed"));
    }

    /// Skip-guarded real-browser smoke: runs every action against a local
    /// `data:` URL page. Skipped when no Chrome/Chromium is installed.
    #[tokio::test]
    async fn real_browser_smoke_navigate_extract_click_fill_screenshot() {
        let Some(bin) = discover_chrome() else {
            eprintln!("browser smoke: skipping (no Chrome/Chromium on host)");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let workspace = WorkspaceRoots::new(dir.path(), Vec::<PathBuf>::new()).unwrap();
        let html = r#"<!DOCTYPE html><html><body>
            <p id="marker">hello-browser</p>
            <input id="name" value="initial">
            <button id="btn" onclick="this.textContent='clicked-ok'">go</button>
            <div id="out"></div>
        </body></html>"#;
        let url = format!(
            "data:text/html;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(html)
        );

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "navigate", "url": url.clone() }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("navigate");
        assert!(
            text_of(&result).starts_with("Navigated to data:text/html;base64,"),
            "{}",
            text_of(&result)
        );
        assert!(text_of(&result).contains("URL: data:"), "{}", text_of(&result));

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "extract", "url": url.clone(), "selector": "#marker" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("extract selector");
        assert_eq!(text_of(&result).trim(), "hello-browser");

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "extract", "url": url.clone() }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("extract page");
        assert!(text_of(&result).contains("hello-browser"));

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "fill", "url": url.clone(), "selector": "#name", "text": "world" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("fill");
        // The fill action reports the input's value after filling, so the
        // effect is observable within the same (fresh-browser) call.
        assert!(text_of(&result).contains("value now \"world\""), "{}", text_of(&result));

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "click", "url": url.clone(), "selector": "#btn" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("click");
        // A real mouse press/release was dispatched: the button's own text
        // changed via its onclick handler, reported back in the same call.
        assert!(text_of(&result).contains("Clicked button"), "{}", text_of(&result));
        assert!(text_of(&result).contains("→ \"clicked-ok\""), "{}", text_of(&result));

        let shot = dir.path().join("shot.png");
        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "screenshot", "url": url.clone(), "path": "shot.png" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("screenshot");
        let (image_data, mime_type) = image_of(&result).expect("inline screenshot image");
        assert_eq!(mime_type, "image/png");
        assert!(image_data.len() <= SCREENSHOT_MAX_BASE64_BYTES);
        assert!(text_of(&result).contains("Screenshot saved to shot.png"), "{}", text_of(&result));
        let bytes = fs::read(&shot).expect("screenshot file exists");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG header");
        assert!(bytes.len() > 100, "non-trivial PNG size");

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "list_tabs" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("list_tabs");
        assert!(text_of(&result).contains("tab(s)"), "{}", text_of(&result));

        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "close" }),
            AbortSignal::none(),
            Some(bin.clone()),
        )
        .await
        .expect("close");
        assert!(text_of(&result).contains("browser closed"));

        // Unsupported scheme is rejected before any browser is spawned.
        let result = run_browser_with(
            cwd,
            &workspace,
            json!({ "action": "navigate", "url": "javascript:alert(1)" }),
            AbortSignal::none(),
            Some(bin),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsupported URL scheme"), "{err}");
    }
}
