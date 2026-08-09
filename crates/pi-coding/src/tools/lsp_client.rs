//! Minimal JSON-RPC 2.0 client for LSP language servers over child-process
//! stdio (Content-Length framing, `lsp-types` message shapes).
//!
//! The `lsp` tool runs one server per invocation (see [`super::lsp`]), so this
//! client is deliberately request/response synchronous: a request is written
//! to the child's stdin and the matching response is read back on stdout with
//! no background reader task. Notifications pushed while waiting (notably
//! `textDocument/publishDiagnostics`) are collected in arrival order so the
//! `diagnostics` action can observe them, and a dedicated
//! [`LspClient::wait_for_diagnostics`] waits for the push targeted at a
//! specific document URI.
//!
//! Server stderr is captured (bounded) and surfaced in error messages, so a
//! server that fails to initialize reports why instead of failing silently.

use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::Uri;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use url::Url;

/// Default timeout for a single JSON-RPC request (matches OMP's
/// `DEFAULT_REQUEST_TIMEOUT_MS`).
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for `textDocument/publishDiagnostics` after a didOpen
/// before reporting the wait as failed.
pub(crate) const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(15);
/// Grace allowed for the server to exit after the `exit` notification before
/// it is killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Shutdown-request grace before the client moves on to `exit`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on captured server stderr so a chatty server cannot balloon memory.
const STDERR_CAP: usize = 64 * 1024;
/// LSP error code `ContentModified` (-32801): the request raced a document
/// change and must be retried against the fresh snapshot.
const CONTENT_MODIFIED_CODE: i64 = -32_801;
/// Max retries for transient `ContentModified` errors.
const CONTENT_MODIFIED_RETRIES: usize = 3;

/// True when `error` is a transient LSP `ContentModified` failure.
fn is_content_modified(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.contains(&format!("\"code\":{CONTENT_MODIFIED_CODE}"))
        || text.contains("content modified")
        || text.contains("ContentModified")
}

/// Re-export of the shared `Content-Length` framing writer (see `framing`).
pub(crate) use super::framing::encode_message;
/// Cap on non-header junk lines scanned before the Content-Length header
/// (mirrors OMP's framing resync: embedded tooling noise must not wedge the
/// client).
pub(crate) use super::framing::MAX_JUNK_HEADER_LINES;

/// Reads one `Content-Length` framed JSON-RPC message from `reader` with
/// LSP-flavored error wording. The framing logic itself is shared with the
/// MCP and DAP clients via [`super::framing`].
pub(crate) async fn read_message(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    super::framing::read_message("LSP", reader, super::framing::DEFAULT_MAX_MESSAGE_BYTES).await
}

/// Converts a local filesystem path to an LSP `file://` URI.
pub(crate) fn path_to_uri(path: &str) -> Result<Uri> {
    let url = Url::from_file_path(path).map_err(|_| anyhow!("invalid file path for URI: {path}"))?;
    Uri::from_str(url.as_str()).map_err(|e| anyhow!("invalid file URI for {path}: {e}"))
}

/// Converts an LSP `file://` URI back to a local filesystem path.
///
/// The scheme is checked explicitly: the `url` crate's `to_file_path` does not
/// verify the scheme (see its docs), so without this check URIs like
/// `untitled:Untitled-1` or `vscode-userdata:///x` would be treated as file
/// paths. Only `file://` URIs with an empty or `localhost` host are accepted.
pub(crate) fn uri_to_path(uri: &str) -> Result<PathBuf> {
    let url = Url::parse(uri).with_context(|| format!("invalid LSP uri: {uri}"))?;
    if url.scheme() != "file" {
        return Err(anyhow!("not a file:// uri: {uri}"));
    }
    url.to_file_path().map_err(|_| anyhow!("not a file:// uri: {uri}"))
}

/// A per-invocation LSP server session: spawned child + framed stdin/stdout.
pub(crate) struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// `textDocument/publishDiagnostics` notifications observed while waiting
    /// for responses, in arrival order.
    pub(crate) diagnostics: Vec<Value>,
    /// Server capabilities returned by `initialize`.
    pub(crate) capabilities: Value,
    /// Bounded tail of the server's stderr, surfaced in error messages.
    stderr_tail: std::sync::Arc<Mutex<String>>,
}

impl LspClient {
    /// Spawns an already-fully-configured command as an LSP server (used by
    /// `tools/lsp.rs` with the resolved binary, and by tests to point at a
    /// fake server process).
    pub(crate) async fn spawn_command(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        let mut child = command.spawn().context("spawning LSP server process")?;
        let stdin = child.stdin.take().context("LSP server stdin unavailable")?;
        let stdout = child.stdout.take().context("LSP server stdout unavailable")?;
        let stderr = child.stderr.take().context("LSP server stderr unavailable")?;
        let stderr_tail = std::sync::Arc::new(Mutex::new(String::new()));
        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut reader = BufReader::new(stderr);
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut guard = tail.lock();
                            guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if guard.len() > STDERR_CAP {
                                let overflow = guard.len() - STDERR_CAP;
                                guard.drain(..overflow);
                            }
                        }
                    }
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            diagnostics: Vec::new(),
            capabilities: Value::Null,
            stderr_tail,
        })
    }

    /// Sends a JSON-RPC notification (no response expected).
    pub(crate) async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.stdin.write_all(&encode_message(&message)?).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Sends a JSON-RPC request and waits for the matching response.
    ///
    /// Notifications received while waiting are collected: publishDiagnostics
    /// into [`Self::diagnostics`], everything else dropped.
    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.stdin.write_all(&encode_message(&message)?).await?;
        self.stdin.flush().await?;
        self.read_response(id).await
    }

    /// [`Self::request`] bounded by `timeout`.
    pub(crate) async fn request_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        match tokio::time::timeout(timeout, self.request(method, params)).await {
            Ok(result) => result,
            Err(_) => bail!(
                "LSP request `{method}` timed out after {}s",
                timeout.as_secs()
            ),
        }
    }

    /// [`Self::request_timeout`] that retries transient `ContentModified`
    /// errors (`-32801`).
    ///
    /// Servers with a file watcher (rust-analyzer) bump the in-memory document
    /// version when their watcher reloads the file from disk, failing requests
    /// that raced the reload. Retrying is the standard remedy — the probe
    /// request carries no version, so the retry sees the fresh snapshot.
    pub(crate) async fn request_with_retry(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let mut attempt = 0usize;
        loop {
            match self.request_timeout(method, params.clone(), timeout).await {
                Ok(result) => return Ok(result),
                Err(error) if attempt < CONTENT_MODIFIED_RETRIES && is_content_modified(&error) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * u64::try_from(attempt).unwrap_or(1)))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Reads messages until the response with `id` arrives.
    async fn read_response(&mut self, id: i64) -> Result<Value> {
        loop {
            let message = read_message(&mut self.stdout).await?;
            if message.get("id").is_none() {
                // A notification: keep what the diagnostics action needs.
                if message.get("method").and_then(Value::as_str)
                    == Some("textDocument/publishDiagnostics")
                {
                    self.diagnostics.push(
                        message.get("params").cloned().unwrap_or(Value::Null),
                    );
                }
                continue;
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // response to a request this client never sent
            }
            if let Some(error) = message.get("error") {
                bail!("LSP request failed: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Runs the LSP `initialize` handshake for `root_path` and returns the
    /// server capabilities. Sends the `initialized` notification on success.
    pub(crate) async fn initialize(&mut self, root_path: &str) -> Result<Value> {
        let root_uri = path_to_uri(root_path)?;
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "rpi", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "rootPath": root_path,
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "willSave": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "codeAction": { "dynamicRegistration": false },
                    "rename": { "prepareSupport": false },
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": { "workspaceFolders": true, "symbol": {} }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": "rpi-workspace" }]
        });
        let result = self
            .request_timeout(lsp_types::request::Initialize::METHOD, params, REQUEST_TIMEOUT)
            .await?;
        self.capabilities = result.clone();
        self.notify(lsp_types::notification::Initialized::METHOD, json!({}))
            .await?;
        Ok(result)
    }

    /// Waits for the next `textDocument/publishDiagnostics` notification for
    /// `uri` and returns its params (which may carry an empty diagnostics
    /// array). Diagnostics pushed for other documents are kept in
    /// [`Self::diagnostics`].
    pub(crate) async fn wait_for_diagnostics(&mut self, uri: &str) -> Result<Value> {
        if let Some(params) = self
            .diagnostics
            .iter()
            .find(|p| p.get("uri").and_then(Value::as_str) == Some(uri))
        {
            return Ok(params.clone());
        }
        let deadline = tokio::time::Instant::now() + DIAGNOSTICS_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out after {}s waiting for diagnostics for {uri}",
                    DIAGNOSTICS_TIMEOUT.as_secs()
                );
            }
            let message = match tokio::time::timeout(remaining, read_message(&mut self.stdout))
                .await
            {
                Ok(result) => result?,
                Err(_) => bail!(
                    "timed out after {}s waiting for diagnostics for {uri}",
                    DIAGNOSTICS_TIMEOUT.as_secs()
                ),
            };
            if message.get("id").is_some() {
                continue; // stray response — not expected while waiting
            }
            if message.get("method").and_then(Value::as_str)
                != Some("textDocument/publishDiagnostics")
            {
                continue;
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            if params.get("uri").and_then(Value::as_str) == Some(uri) {
                return Ok(params);
            }
            self.diagnostics.push(params);
        }
    }

    /// Best-effort LSP shutdown handshake, then reaps (and if necessary
    /// kills) the child. Never fails the caller: a wedged server must not
    /// hang the tool.
    pub(crate) async fn shutdown(&mut self) {
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            self.request(lsp_types::request::Shutdown::METHOD, json!(null)),
        )
        .await;
        let _ = self.notify(lsp_types::notification::Exit::METHOD, json!(null)).await;
        match tokio::time::timeout(EXIT_TIMEOUT, self.child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
    }

    /// Bounded tail of the server's stderr (for diagnostics in error paths).
    pub(crate) fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().clone()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Never leak a server process, even on panic/abort paths.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};

    #[test]
    fn uri_to_path_requires_file_scheme() {
        // Round trip for a real file path.
        let path = std::env::temp_dir().join("pi-lsp-uri-test.rs");
        let uri = path_to_uri(&path.display().to_string()).unwrap();
        assert_eq!(uri_to_path(uri.as_str()).unwrap(), path);

        // Non-file schemes must be rejected even though the url crate's
        // to_file_path does not check the scheme itself.
        for bad in [
            "http://evil.example/x.rs",
            "untitled:Untitled-1",
            "vscode-userdata:///x",
        ] {
            let err = uri_to_path(bad).unwrap_err().to_string();
            assert!(err.contains("not a file:// uri"), "{bad}: {err}");
        }
    }

    #[test]
    fn encode_message_uses_content_length_framing() {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(
            bytes.starts_with(header.as_bytes()),
            "expected header prefix {header:?} in {bytes:?}"
        );
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[test]
    fn encode_message_counts_bytes_not_chars() {
        let body = json!({ "text": "héllo 世界 — ünïcode" });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(bytes.starts_with(header.as_bytes()));
        // The body is multibyte, so the byte length must exceed the character
        // count — proving Content-Length counts bytes, not characters.
        let text_chars = body["text"].as_str().unwrap().chars().count();
        assert!(serialized.len() > text_chars, "bytes {} vs chars {text_chars}", serialized.len());
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[tokio::test]
    async fn read_message_decodes_multiple_framed_messages() {
        let mut payload = Vec::new();
        payload.extend(encode_message(&json!({"jsonrpc":"2.0","id":1,"result":true})).unwrap());
        payload.extend(encode_message(&json!({"jsonrpc":"2.0","id":2,"result":"two"})).unwrap());
        let mut reader = BufReader::new(&payload[..]);
        let first = read_message(&mut reader).await.unwrap();
        let second = read_message(&mut reader).await.unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"], true);
        assert_eq!(second["id"], 2);
        assert_eq!(second["result"], "two");
    }

    #[tokio::test]
    async fn read_message_accepts_any_header_case_and_order() {
        let body = r#"{"jsonrpc":"2.0","id":9,"result":"ok"}"#;
        let payload = format!(
            "x-custom: ignored\r\ncontent-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message(&mut reader).await.unwrap();
        assert_eq!(message["id"], 9);
        assert_eq!(message["result"], "ok");
    }

    #[tokio::test]
    async fn read_message_rejects_missing_content_length() {
        // No Content-Length anywhere: the junk budget is exhausted and the
        // message is rejected.
        let payload = format!("{}\r\n\r\n{{}}", "\n".repeat(MAX_JUNK_HEADER_LINES + 1));
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("Content-Length"), "{err}");
    }

    #[tokio::test]
    async fn read_message_skips_junk_lines_before_headers() {
        // Noise like an embedded test harness's progress output must not
        // wedge the client: blank lines and non-header lines before the
        // Content-Length header are skipped.
        let body = r#"{"jsonrpc":"2.0","id":7,"result":true}"#;
        let payload = format!(
            "\n\nrunning 1 test\n\ntest tools::x ... ok\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message(&mut reader).await.unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(message["result"], true);
    }

    #[tokio::test]
    async fn read_message_reports_eof() {
        let mut reader = BufReader::new(&b""[..]);
        let err = read_message(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("closed"), "{err}");
    }

    #[tokio::test]
    async fn read_message_handles_unicode_bodies() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":"héllo 世界"}"#;
        let payload = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message(&mut reader).await.unwrap();
        assert_eq!(message["result"], "héllo 世界");
    }

    // -----------------------------------------------------------------------
    // Fake LSP server
    // -----------------------------------------------------------------------
    //
    // The client tests below spawn a fake LSP server by re-executing this test
    // binary with `--exact tools::lsp_client::tests::fake_lsp_server_process`
    // and `PI_FAKE_LSP_SERVER=1`. The test then acts as a minimal server
    // speaking initialize/hover/definition/didOpen→publishDiagnostics/shutdown
    // over Content-Length framing (implemented independently of the client's
    // framing so a framing asymmetry fails the test).

    /// Runs the fake server loop when invoked via the env-var re-exec trick;
    /// a silent no-op when the test suite runs it directly.
    #[test]
    fn fake_lsp_server_process() {
        if std::env::var_os("PI_FAKE_LSP_SERVER").is_none() {
            return;
        }
        // Boom mode (PI_FAKE_LSP_BOOM=1): log credential-shaped text to
        // stderr, then refuse initialize — exercises the stderr-tail
        // redaction in the LSP initialize-failure error path.
        let boom = std::env::var_os("PI_FAKE_LSP_BOOM").is_some();
        if boom {
            let github_token =
                ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghij0123456789"].concat();
            let api_token = ["s", "k-", "abcdefghijklmnop", "1234"].concat();
            eprintln!("fake lsp server: token={github_token} {api_token}");
            // Let the client's stderr reader drain the line before the
            // initialize error is answered, so the tail is captured.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut writer = std::io::BufWriter::new(stdout.lock());

        fn send(writer: &mut std::io::BufWriter<std::io::StdoutLock<'_>>, body: &Value) {
            let bytes = sync_encode(body).expect("fake server encode");
            writer.write_all(&bytes).expect("fake server write");
            writer.flush().expect("fake server flush");
        }

        loop {
            let message = sync_read_message(&mut reader).expect("fake server read");
            let method = message.get("method").and_then(Value::as_str);
            let id = message.get("id").cloned();
            match (method, id) {
                (Some("initialize"), Some(id)) if boom => send(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32002, "message": "initialize refused by fake server" }
                    }),
                ),
                (Some("initialize"), Some(id)) => send(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": {
                                "hoverProvider": true,
                                "definitionProvider": true,
                                "referencesProvider": true,
                                "documentSymbolProvider": true
                            }
                        }
                    }),
                ),
                (Some("initialized"), _) => {}
                (Some("textDocument/didOpen"), _) => {
                    let uri = message
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                        .unwrap_or("file:///fake");
                    send(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": uri,
                                "version": 1,
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 5 }
                                    },
                                    "severity": 1,
                                    "source": "fake",
                                    "message": "fake diagnostic message"
                                }]
                            }
                        }),
                    );
                }
                (Some("textDocument/hover"), Some(id)) => send(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "contents": { "kind": "markdown", "value": "**Fake hover**\n\nproduced by fake server" }
                        }
                    }),
                ),
                (Some("textDocument/definition"), Some(id)) => send(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "uri": "file:///tmp/fake_definition.rs",
                            "range": {
                                "start": { "line": 2, "character": 4 },
                                "end": { "line": 2, "character": 9 }
                            }
                        }]
                    }),
                ),
                (Some("textDocument/rename"), Some(id)) => {
                    // Resource-op mode: answer with a create-file
                    // documentChange plus a text edit, exercising the lsp
                    // tool's explicit resource-operation rejection end to end.
                    if std::env::var_os("PI_FAKE_LSP_RENAME_RESOURCE_OP").is_some() {
                        send(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "documentChanges": [
                                        { "kind": "create", "uri": "file:///tmp/pi-fake-created.rs" },
                                        {
                                            "textDocument": { "uri": "file:///tmp/pi-fake-created.rs", "version": 1 },
                                            "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "newText": "x" }]
                                        }
                                    ]
                                }
                            }),
                        );
                    } else {
                        // Target mode: `PI_FAKE_LSP_RENAME_TARGETS` is a
                        // semicolon-separated list of URIs; each gets one text
                        // edit replacing the first 3 characters of line 0 with
                        // "NEW".
                        let targets =
                            std::env::var("PI_FAKE_LSP_RENAME_TARGETS").unwrap_or_default();
                        let mut changes = serde_json::Map::new();
                        for uri in targets.split(';').filter(|s| !s.is_empty()) {
                            changes.insert(
                                uri.to_owned(),
                                json!([{
                                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
                                    "newText": "NEW"
                                }]),
                            );
                        }
                        send(
                            &mut writer,
                            &json!({ "jsonrpc": "2.0", "id": id, "result": { "changes": changes } }),
                        );
                    }
                }
                (Some("shutdown"), Some(id)) => send(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                ),
                (Some("exit"), _) => return,
                _ => {}
            }
        }
    }

    /// Independent sync framing writer for the fake server, kept separate from
    /// the async client writer so a framing asymmetry fails the tests.
    fn sync_encode(body: &Value) -> std::io::Result<Vec<u8>> {
        let json = serde_json::to_vec(body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
        out.extend_from_slice(&json);
        Ok(out)
    }

    /// Independent sync framing reader for the fake server, kept separate from
    /// the async client reader so a framing asymmetry fails the tests.
    fn sync_read_message(reader: &mut impl std::io::BufRead) -> std::io::Result<Value> {
        let mut header = String::new();
        let mut content_length = None;
        loop {
            header.clear();
            if reader.read_line(&mut header)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "fake server stdin closed",
                ));
            }
            if header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.trim_end().split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    content_length = Some(value.trim().parse().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    })?);
                }
            }
        }
        let length = content_length.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
        })?;
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Spawns the fake server (this test binary in fake-server mode).
    async fn spawn_fake_server() -> LspClient {
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = Command::new(exe);
        // Substring filter (no --exact) so only the fake-server test runs in
        // the child; --nocapture lets the server write framed messages to
        // stdout. The client's header parser tolerates libtest's own progress
        // lines on the same stream.
        command
            .arg("tools::lsp_client::tests::fake_lsp_server_process")
            .arg("--nocapture")
            .env("PI_FAKE_LSP_SERVER", "1");
        LspClient::spawn_command(command).await.expect("fake server spawn")
    }

    #[test]
    fn client_initializes_and_queries_fake_server() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let mut client = spawn_fake_server().await;
            // The client must send `initialize` and parse the response.
            let caps = client
                .initialize("/tmp")
                .await
                .expect("initialize handshake");

            assert_eq!(
                caps.pointer("/capabilities/hoverProvider").and_then(Value::as_bool),
                Some(true)
            );

            // didOpen triggers a pushed publishDiagnostics notification.
            client
                .notify(
                    lsp_types::notification::DidOpenTextDocument::METHOD,
                    json!({
                        "textDocument": {
                            "uri": "file:///tmp/fake.rs",
                            "languageId": "rust",
                            "version": 1,
                            "text": "fn main() {}"
                        }
                    }),
                )
                .await
                .expect("didOpen");
            let diagnostics = client
                .wait_for_diagnostics("file:///tmp/fake.rs")
                .await
                .expect("pushed diagnostics");
            assert_eq!(diagnostics["diagnostics"][0]["message"], "fake diagnostic message");

            // hover parses the markdown result.
            let hover = client
                .request_timeout(
                    lsp_types::request::HoverRequest::METHOD,
                    json!({
                        "textDocument": { "uri": "file:///tmp/fake.rs" },
                        "position": { "line": 0, "character": 0 }
                    }),
                    REQUEST_TIMEOUT,
                )
                .await
                .expect("hover request");
            assert_eq!(hover["contents"]["value"], "**Fake hover**\n\nproduced by fake server");

            // definition returns locations.
            let definitions = client
                .request_timeout(
                    lsp_types::request::GotoDefinition::METHOD,
                    json!({
                        "textDocument": { "uri": "file:///tmp/fake.rs" },
                        "position": { "line": 0, "character": 4 }
                    }),
                    REQUEST_TIMEOUT,
                )
                .await
                .expect("definition request");
            assert_eq!(definitions[0]["uri"], "file:///tmp/fake_definition.rs");

            // Graceful shutdown handshake must complete.
            client.shutdown().await;
        });
    }
}
