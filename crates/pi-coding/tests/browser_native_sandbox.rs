#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{ContentBlock, Model};
use pi_coding::{
    Application, BrowserSandboxError, NativeBrowserSandboxFailure, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::{Value, json};

fn application_session(cwd: &Path) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("browser-native-sandbox-api-{suffix}");
    let provider = format!("browser-native-sandbox-provider-{suffix}");
    let model = Model {
        id: format!("browser-native-sandbox-model-{suffix}"),
        name: "Browser native sandbox model".to_owned(),
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
    let session = Session::new(SessionOptions {
        model: Some(model),
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    })
    .expect("session");
    (session, registration)
}

async fn browser_application(
    cwd: &Path,
) -> (Application, FauxProviderRegistration, tempfile::TempDir) {
    // Agent root lives in its own tempdir outside the workspace: an agent dir
    // inside the project boundary changes project-resource discovery. The
    // returned TempDir keeps it alive for the application's lifetime.
    let agent_dir = tempfile::tempdir().expect("agent directory");
    // Danger mode is the hermetic, launcher-free baseline (it already implies
    // full network access; an explicit `network` would conflict with the
    // mode's policy contract).
    std::fs::write(
        agent_dir.path().join("settings.json"),
        serde_json::to_vec(&json!({
            "sandbox": {
                "mode": "danger-full-access"
            }
        }))
        .expect("settings json"),
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir.path().to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = application_session(cwd);
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    (Application::new(session).await, registration, agent_dir)
}

fn browser_tool(application: &Application) -> AgentTool {
    application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "browser")
        .expect("generation-bound browser tool")
}

fn prepared_arguments(tool: &AgentTool, arguments: Value) -> Value {
    tool.prepare_arguments
        .as_ref()
        .expect("browser argument preparation")(arguments)
    .expect("prepared arguments")
}

async fn execute_approved(tool: &AgentTool, arguments: Value) -> anyhow::Result<AgentToolResult> {
    let arguments = prepared_arguments(tool, arguments);
    let approval = tool
        .prepare_approval
        .as_ref()
        .expect("browser approval preparation")(arguments.clone())
    .await?
    .expect("browser launch approval");
    assert_eq!(approval.summary.target_fds.len(), 2);
    assert_eq!(approval.summary.target_fds[0].target_fd, 3);
    assert_eq!(approval.summary.target_fds[1].target_fd, 4);
    let token = approval.issue().await?;
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    (tool.execute)(ToolCallContext {
        tool_call_id: "browser-native-sandbox".to_owned(),
        arguments,
        on_update,
        abort,
        model: None,
        approval_token: Some(token),
    })
    .await
}

fn result_text(result: &AgentToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

/// The only live governed-browser proof: it launches a real Chrome via the
/// governed debugging pipe. Chrome is not available on every host (notably
/// the static-gates runner), so the proof is `#[ignore]`d by default — the
/// runner reports it as ignored, never as a green pass. An explicit
/// `--include-ignored` run without `CHROME_PATH` fails loudly instead of
/// silently passing.
#[cfg(unix)]
#[ignore = "live browser proof: requires CHROME_PATH pointing at a real Chrome/Chromium binary (run with --include-ignored)"]
#[tokio::test]
async fn governed_debugging_pipe_returns_browser_version() {
    let chrome = match std::env::var_os("CHROME_PATH") {
        Some(chrome) if !chrome.is_empty() => chrome,
        _ => panic!(
            "governed_debugging_pipe_returns_browser_version requires CHROME_PATH to run; it is #[ignore]d by default"
        ),
    };
    assert!(
        Path::new(&chrome).is_absolute(),
        "CHROME_PATH must be absolute"
    );
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration, _agent_dir) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let result = execute_approved(&tool, json!({ "action": "list_tabs" }))
        .await
        .expect("governed browser debugging pipe");
    assert!(result_text(&result).contains("tab(s)"), "{result:?}");
    application.cleanup().await;
    registration.unregister();
}

/// Stoppable loopback fixture server: `stop` ends the serve loop promptly
/// and joins the thread; a thread panic is re-raised, never swallowed.
struct LoopbackServer {
    accepts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    request_bytes: Arc<AtomicUsize>,
    read_error: Arc<Mutex<Option<String>>>,
    write_error: Arc<Mutex<Option<String>>>,
    stamps: Arc<Mutex<(u128, u128, u128)>>,
    accept_error: Arc<Mutex<Option<String>>>,
}

impl LoopbackServer {
    fn accept_count(&self) -> usize {
        self.accepts.load(Ordering::Relaxed)
    }
    fn request_bytes(&self) -> usize {
        self.request_bytes.load(Ordering::Relaxed)
    }

    fn read_error(&self) -> Option<String> {
        self.read_error.lock().unwrap().clone()
    }

    fn write_error(&self) -> Option<String> {
        self.write_error.lock().unwrap().clone()
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            if let Err(payload) = handle.join() {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

/// Serves `body` for every HTTP request on an ephemeral loopback port.
/// Terminates after no new connection arrives for one second past the first
/// serve (hard-capped at 15s), so favicon retries cannot leak the thread.
/// Returns the base URL and the stoppable server (live accept counter).
fn loopback_page(body: &str) -> (String, LoopbackServer) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("loopback address").port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let accept_count = accepts.clone();
    let stop_flag = stop.clone();
    let request_bytes = Arc::new(AtomicUsize::new(0));
    let request_bytes_writer = request_bytes.clone();
    let read_error = Arc::new(Mutex::new(None));
    let read_error_writer = read_error.clone();
    let write_error = Arc::new(Mutex::new(None));
    let write_error_writer = write_error.clone();
    let stamps = Arc::new(Mutex::new((0_u128, 0_u128, 0_u128)));
    let accept_error = Arc::new(Mutex::new(None));
    let (stamps_w, accept_error_w) = (stamps.clone(), accept_error.clone());
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let start = std::time::Instant::now();
        let mut served = false;
        let mut last_serve = start;
        // Pending until request headers are complete; only WouldBlock and
        // EOF are normal, and the header cap is 4 KiB.
        let mut pending: Vec<(std::net::TcpStream, Vec<u8>)> = Vec::new();
        while start.elapsed() < std::time::Duration::from_secs(15)
            && !stop_flag.load(Ordering::Relaxed)
            && !(served && last_serve.elapsed() > std::time::Duration::from_secs(1))
        {
            match listener.accept() {
                Ok((socket, _)) => {
                    accept_count.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut stamps = stamps_w.lock().unwrap();
                        if stamps.0 == 0 {
                            stamps.0 = start.elapsed().as_millis();
                        }
                    }
                    match socket.set_nonblocking(true) {
                        Ok(()) => pending.push((socket, Vec::new())),
                        Err(error) => {
                            let mut slot = read_error_writer.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(format!("set_nonblocking {:?}", error.kind()));
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    *accept_error_w.lock().unwrap() = Some(format!("{:?}", error.kind()));
                    break;
                }
            }
            let mut still_pending = Vec::with_capacity(pending.len());
            for (mut socket, mut received) in pending.drain(..) {
                let mut buf = [0u8; 4096];
                let remaining = 4096 - received.len();
                match socket.read(&mut buf[..remaining]) {
                    Ok(0) => {}
                    Ok(n) => {
                        request_bytes_writer.fetch_add(n, Ordering::Relaxed);
                        received.extend_from_slice(&buf[..n]);
                        {
                            let mut stamps = stamps_w.lock().unwrap();
                            if stamps.1 == 0 {
                                stamps.1 = start.elapsed().as_millis();
                            }
                        }
                        if !received.windows(4).any(|window| window == b"\r\n\r\n") {
                            if received.len() >= 4096 {
                                let mut slot = read_error_writer.lock().unwrap();
                                if slot.is_none() {
                                    *slot = Some("incomplete request headers".to_owned());
                                }
                            } else {
                                still_pending.push((socket, received));
                            }
                            continue;
                        }
                        if let Err(error) = socket.set_nonblocking(false) {
                            *write_error_writer.lock().unwrap() =
                                Some(format!("set_nonblocking {:?}", error.kind()));
                            continue;
                        }
                        if let Err(error) =
                            socket.set_write_timeout(Some(std::time::Duration::from_secs(1)))
                        {
                            *write_error_writer.lock().unwrap() =
                                Some(format!("set_write_timeout {:?}", error.kind()));
                            continue;
                        }
                        match socket.write_all(response.as_bytes()) {
                            Ok(()) => {
                                served = true;
                                last_serve = std::time::Instant::now();
                            }
                            Err(error) => {
                                let mut slot = write_error_writer.lock().unwrap();
                                if slot.is_none() {
                                    *slot = Some(format!("{:?}", error.kind()));
                                }
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        still_pending.push((socket, received));
                    }
                    Err(error) => {
                        let mut slot = read_error_writer.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(format!("{:?}", error.kind()));
                        }
                    }
                }
            }
            pending = still_pending;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stamps_w.lock().unwrap().2 = start.elapsed().as_millis();
    });
    (
        format!("http://127.0.0.1:{port}/"),
        LoopbackServer {
            accepts,
            stop,
            handle: Some(handle),
            request_bytes,
            read_error,
            write_error,
            stamps,
            accept_error,
        },
    )
}

/// RED fixture contract: a browser preconnects (TCP established before the
/// request) and only then sends the real GET. The accepted socket must keep
/// being polled for the whole 15s window rather than dropped after one 200ms
/// read. The unfixed `loopback_page` read once, saw `WouldBlock`, and dropped
/// the preconnected socket before the GET existed, so the late GET could
/// never get an HTTP response. The failure names the stage and every fixture
/// metric instead of masking the cause behind a `write_all` BrokenPipe panic.
#[cfg(unix)]
#[test]
fn loopback_page_serves_preconnected_request_after_read_window() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    let marker = format!("LATE-GET-MARKER-{}", uuid::Uuid::now_v7());
    let body = format!("<html><body><pre>{marker}</pre></body></html>");
    let (url, server) = loopback_page(&body);
    let port: u16 = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.strip_suffix('/'))
        .and_then(|port| port.parse().ok())
        .expect("loopback fixture url");
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect("preconnect tcp to loopback fixture");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("preconnect read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("preconnect write timeout");
    let accept_deadline = Instant::now() + Duration::from_secs(2);
    while server.accept_count() == 0 && Instant::now() < accept_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if server.accept_count() == 0 {
        let trace = fixture_trace(&server);
        server.stop();
        panic!("stage preconnect-not-accepted within 2s (fixture {trace})");
    }
    // The fixture's request-read window is 200ms; a silent preconnect through
    // it is real client behaviour (Chrome establishes the socket before the
    // GET). Stay on the same socket and wait well past the window so the
    // old fixture has certainly dropped it (no scheduling-race pass).
    std::thread::sleep(Duration::from_millis(1000));
    let request = format!(
        "GET / HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\naccept: text/html\r\n\
         connection: close\r\n\r\n"
    );
    let request_len = request.len();
    let write_error = stream
        .write_all(request.as_bytes())
        .err()
        .map(|error| format!("{:?}: {error}", error.kind()));
    let mut response = Vec::new();
    let read_error = stream
        .read_to_end(&mut response)
        .err()
        .map(|error| format!("{:?}: {error}", error.kind()));
    let trace = fixture_trace(&server);
    let accepts = server.accept_count();
    let request_bytes = server.request_bytes();
    server.stop();
    let text = String::from_utf8_lossy(&response);
    let status_ok = text.starts_with("HTTP/1.1 200 OK\r\n");
    let length_header = format!("content-length: {}", body.len());
    let length_ok = text.contains(length_header.as_str());
    let payload = text
        .split_once("\r\n\r\n")
        .map(|(_, payload)| payload)
        .unwrap_or_default();
    assert!(
        status_ok
            && length_ok
            && write_error.is_none()
            && read_error.is_none()
            && payload == body.as_str(),
        "stage late-GET-after-read-window: the fixture must keep polling the \
         preconnected socket in the 15s window instead of dropping it at the \
         200ms read window (accept_count {accepts}, request_bytes \
         {request_bytes}, request_len {request_len}, response_bytes {}, \
         status_ok {status_ok}, length_ok {length_ok}, payload_bytes {}, \
         write_error {write_error:?}, read_error {read_error:?}, fixture \
         {trace})",
        response.len(),
        payload.len()
    );
}

/// Regression for the `extract` byte ceiling: a single governed `extract`
/// call navigating a real loopback page whose visible text exceeds
/// `EXTRACT_MAX_BYTES` must return the whole text, including the tail.
/// RED while `extract` truncates at 16 KiB with no paging — the marker past
/// the ceiling is unrecoverable through the tool today.
#[cfg(unix)]
#[ignore = "live browser proof: requires CHROME_PATH pointing at a real Chrome/Chromium binary (run with --include-ignored)"]
#[tokio::test]
async fn extract_over_loopback_returns_full_text_including_tail_marker() {
    let chrome = match std::env::var_os("CHROME_PATH") {
        Some(chrome) if !chrome.is_empty() => chrome,
        _ => panic!(
            "extract_over_loopback_returns_full_text_including_tail_marker requires CHROME_PATH to run; it is #[ignore]d by default"
        ),
    };
    assert!(
        Path::new(&chrome).is_absolute(),
        "CHROME_PATH must be absolute"
    );
    let marker = format!("TAIL-MARKER-{}", uuid::Uuid::now_v7());
    let filler = "loopback extract regression filler\n".repeat(700);
    let body = format!("<html><body><pre>{filler}{marker}</pre></body></html>");
    let (url, server) = loopback_page(&body);
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration, _agent_dir) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let text = match execute_approved(&tool, json!({ "action": "extract", "url": url })).await {
        Ok(result) => result_text(&result).to_owned(),
        Err(error) => {
            let message = format!(
                "extract failed (loopback_accepts {}, request_bytes {}, \
                 read_error {:?}, write_error {:?}): {error:#}",
                server.accept_count(),
                server.request_bytes(),
                server.read_error(),
                server.write_error()
            );
            server.stop();
            panic!("{message}");
        }
    };
    // Documented non-fatal socket error: WouldBlock is the 200ms request
    // poll window (a dial with no request — the preconnect signature).
    // Every other read/write error kind must fail the test, never be
    // swallowed.
    if let Some(kind) = server.read_error() {
        assert!(
            kind == "WouldBlock",
            "loopback fixture request read failed: {kind}"
        );
    }
    if let Some(kind) = server.write_error() {
        panic!("loopback fixture response write failed: {kind}");
    }
    let accepts = server.accept_count();
    let request_bytes = server.request_bytes();
    server.stop();
    assert!(
        text.contains(&marker),
        "extract dropped the page tail ({} byte(s) returned, loopback_accepts {}, \
         request_bytes {}): {text:?}",
        text.len(),
        accepts,
        request_bytes
    );
    application.cleanup().await;
    registration.unregister();
}

/// Discriminates a session-scoped CDP hang from a network stall: blank-page
/// navigation exercises the same attach → `Page.navigate` → `wait_for_load`
/// path as any real URL but requires zero renderer network access.
#[cfg(unix)]
#[ignore = "live browser proof: requires CHROME_PATH pointing at a real Chrome/Chromium binary (run with --include-ignored)"]
#[tokio::test]
async fn governed_session_scoped_navigation_returns_on_blank_page() {
    let chrome = match std::env::var_os("CHROME_PATH") {
        Some(chrome) if !chrome.is_empty() => chrome,
        _ => panic!(
            "governed_session_scoped_navigation_returns_on_blank_page requires CHROME_PATH to run; it is #[ignore]d by default"
        ),
    };
    assert!(
        Path::new(&chrome).is_absolute(),
        "CHROME_PATH must be absolute"
    );
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration, _agent_dir) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let result = execute_approved(&tool, json!({ "action": "navigate", "url": "about:blank" }))
        .await
        .expect("governed blank navigation");
    assert!(
        result_text(&result).contains("Navigated to about:blank"),
        "{result:?}"
    );
    application.cleanup().await;
    registration.unregister();
}

#[test]
fn browser_errors_are_complete_and_redacted() {
    let secret = ["sk-", "browser-native-sandbox-secret"].concat();
    let native = [
        NativeBrowserSandboxFailure::Timeout { timeout_ms: 15_000 },
        NativeBrowserSandboxFailure::ProcessExited {
            code: Some(9),
            stderr_bytes: secret.len(),
        },
        NativeBrowserSandboxFailure::DebuggingPipeClosed {
            stderr_bytes: secret.len(),
        },
        NativeBrowserSandboxFailure::FrameTooLarge { limit: 1024 * 1024 },
        NativeBrowserSandboxFailure::InvalidFrame,
        NativeBrowserSandboxFailure::CommandRejected,
    ];
    assert_eq!(
        native
            .iter()
            .map(NativeBrowserSandboxFailure::variant_name)
            .collect::<Vec<_>>(),
        [
            "timeout",
            "process-exited",
            "debugging-pipe-closed",
            "frame-too-large",
            "invalid-frame",
            "command-rejected",
        ]
    );
    for failure in native {
        let error = BrowserSandboxError::Native(failure);
        assert_eq!(error.variant_name(), "native");
        assert!(!error.to_string().contains(&secret));
        assert!(!format!("{error:?}").contains(&secret));
    }
    let filesystem_probe = BrowserSandboxError::Native(NativeBrowserSandboxFailure::InvalidFrame);
    assert!(!filesystem_probe.is_filesystem());
    assert!(!format!("{filesystem_probe:?}").contains(&secret));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_browser_is_rejected_before_approval() {
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration, _agent_dir) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let arguments = prepared_arguments(&tool, json!({ "action": "list_tabs" }));
    let error = tool
        .prepare_approval
        .as_ref()
        .expect("browser approval preparation")(arguments)
    .await
    .expect_err("Windows browser approval must fail closed");
    assert!(error.downcast_ref::<BrowserSandboxError>().is_some());
    assert!(error.to_string().contains("unsupported"));
    application.cleanup().await;
    registration.unregister();
}

#[test]
fn browser_filesystem_errors_are_typed() {
    let error = BrowserSandboxError::filesystem_denied_for_test();
    assert!(error.is_filesystem());
    assert_eq!(error.variant_name(), "filesystem");
    assert!(error.to_string().contains("filesystem"));
}

/// RED acceptance for `extract` full-text artifact: `path` must write the
/// complete untruncated page text (exact bytes) and the real `read` tool
/// windows must recover head/middle/tail. RED today: `extract` ignores
/// `path` and returns the inline 16 KiB body.
#[cfg(unix)]
#[ignore = "live browser proof: requires CHROME_PATH pointing at a real Chrome/Chromium binary (run with --include-ignored)"]
#[tokio::test]
async fn extract_artifact_returns_full_text_with_readable_middle_and_tail() {
    let chrome = match std::env::var_os("CHROME_PATH") {
        Some(chrome) if !chrome.is_empty() => chrome,
        _ => panic!(
            "extract_artifact_returns_full_text_with_readable_middle_and_tail requires CHROME_PATH to run; it is #[ignore]d by default"
        ),
    };
    assert!(
        Path::new(&chrome).is_absolute(),
        "CHROME_PATH must be absolute"
    );
    let head_marker = format!("HEAD-MARKER-{}", uuid::Uuid::now_v7());
    let middle_marker = format!("MIDDLE-MARKER-{}", uuid::Uuid::now_v7());
    let tail_marker = format!("TAIL-MARKER-{}", uuid::Uuid::now_v7());
    let filler_line = "loopback extract regression filler";
    let segment = || vec![filler_line; 800].join("\n");
    let text = format!(
        "{head_marker}\n{}\n{middle_marker}\n{}\n{tail_marker}",
        segment(),
        segment()
    );
    let expected_lines = 2 * 800 + 3_usize;
    let body = format!("<html><body><pre>{text}</pre></body></html>");
    let (url, server) = loopback_page(&body);
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration, _agent_dir) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let result = match execute_approved(
        &tool,
        json!({ "action": "extract", "url": url, "path": "extract-artifact.txt" }),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let trace = fixture_trace(&server);
            server.stop();
            panic!("extract failed (fixture {trace}): {error:#}");
        }
    };
    server.stop();
    let summary = result_text(&result);
    assert!(
        summary.len() < 4096
            && summary.contains("extract-artifact.txt")
            && summary.contains(&text.len().to_string()),
        "extract summary must state bounded bytes and the readable artifact path \
         (RED: extract ignores path and returns the inline body): {} byte(s)",
        summary.len()
    );
    let file_path = cwd.path().join("extract-artifact.txt");
    let bytes = std::fs::read(&file_path).unwrap_or_else(|error| {
        panic!("extract artifact file must exist (RED: extract ignores path): {error}")
    });
    assert!(
        bytes == text.as_bytes() || bytes.strip_suffix(b"\n") == Some(text.as_bytes()),
        "artifact bytes diverge from the page text ({} vs {} byte(s))",
        bytes.len(),
        text.len()
    );
    let read = application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "read")
        .expect("session read tool");
    let head = read_window(&read, "extract-artifact.txt", 1).await;
    assert!(
        head.contains(&head_marker)
            && !head.contains(&middle_marker)
            && !head.contains(&tail_marker),
        "head window wrong: {head:?}"
    );
    let middle = read_window(&read, "extract-artifact.txt", 802).await;
    assert!(
        middle.contains(&middle_marker),
        "read window cannot reach the middle of the artifact: {middle:?}"
    );
    let tail = read_window(&read, "extract-artifact.txt", expected_lines).await;
    assert!(
        tail.contains(&tail_marker) && !tail.contains(&middle_marker),
        "read window cannot reach the tail of the artifact: {tail:?}"
    );
    application.cleanup().await;
    registration.unregister();
}

#[cfg(unix)]
async fn read_window(read: &AgentTool, path: &str, offset: usize) -> String {
    let (_controller, abort) = AbortController::new();
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    let result = (read.execute)(ToolCallContext {
        tool_call_id: "extract-artifact-read".to_owned(),
        arguments: json!({ "path": path, "offset": offset, "limit": 1 }),
        on_update,
        abort,
        model: None,
        approval_token: None,
    })
    .await
    .expect("read tool window on extract artifact");
    result_text(&result).to_owned()
}

/// Category-only fixture metadata: ms stamps measured from server-thread
/// spawn (0 = event never seen) plus the exact accept error kind.
#[cfg(unix)]
fn fixture_trace(server: &LoopbackServer) -> String {
    let stamps = server.stamps.lock().unwrap();
    let accept_error = server.accept_error.lock().unwrap().clone();
    format!(
        "first_accept_ms {:?} first_request_ms {:?} window_closed_ms {:?} accept_error {:?} \
         accept_count {} request_bytes {} read_error {:?} write_error {:?}",
        (stamps.0 != 0).then_some(stamps.0),
        (stamps.1 != 0).then_some(stamps.1),
        (stamps.2 != 0).then_some(stamps.2),
        accept_error.clone(),
        server.accepts.load(Ordering::Relaxed),
        server.request_bytes.load(Ordering::Relaxed),
        server.read_error.lock().unwrap().clone(),
        server.write_error.lock().unwrap().clone()
    )
}
