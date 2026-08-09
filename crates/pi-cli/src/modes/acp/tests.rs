//! In-process tests for the ACP agent mode: a fake ACP client drives
//! [`serve_connection`] over channels, exercising initialize/authenticate/
//! session/prompt, the reverse-request permission round trip, cancellation,
//! error paths, and the stdio Content-Length framing.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser as _;
use pi_ai::{
    ContentBlock, Model, ToolCall,
    providers::{FauxProviderOptions, FauxResponse, register_faux_provider},
};
use serde_json::{Value, json};

use super::*;

// ---------------------------------------------------------------------------
// Fake ACP client + hermetic server harness
// ---------------------------------------------------------------------------

struct TestClient {
    tx: mpsc::Sender<Value>,
    rx: mpsc::Receiver<Value>,
    next_id: i64,
}

impl TestClient {
    /// Send a request and collect notifications until the matching response.
    async fn request(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.tx
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
            .expect("send request");
        let mut notifications = Vec::new();
        loop {
            let message = self.rx.recv().await.expect("server response");
            if message.get("id") == Some(&id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.tx
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
            .expect("send notification");
    }

    /// Respond to a reverse request (e.g. `session/request_permission`).
    async fn respond(&mut self, id: &Value, result: Value) {
        self.tx
            .send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await
            .expect("send reverse response");
    }

    async fn next(&mut self) -> Value {
        self.rx.recv().await.expect("server message")
    }

    fn result(response: &Value) -> &Value {
        &response["result"]
    }

    fn error_code(response: &Value) -> i64 {
        response["error"]["code"].as_i64().expect("error code")
    }
}

/// Build the hermetic blueprint: temp agent dir, headless, Json extension
/// mode, no interactive UI.
fn test_blueprint(cli: &Cli, agent_dir: &Path) -> RunSessionBlueprint {
    let mut resource_options = ResourceManagerOptions::new(agent_dir);
    resource_options.agent_dir = agent_dir.to_path_buf();
    resource_options.headless = true;
    let mut blueprint = RunSessionBlueprint::from_cli(cli, resource_options, None);
    blueprint.set_extension_mode(ExtensionMode::Json);
    blueprint
}

/// Register a faux provider with a unique api and model id so concurrent
/// tests never share a response queue or model-registry entry. Returns the
/// registration (kept alive by the model registry for the test) and the
/// resolvable model id (`faux-<tag>`).
fn register_faux(tag: &str) -> (pi_ai::providers::FauxProviderRegistration, String) {
    let api = format!("acp-{tag}-api-{}", Uuid::now_v7());
    let model_id = format!("faux-{tag}");
    let mut model = Model::default();
    model.id = model_id.clone();
    model.name = "Faux Model".into();
    model.api = api.clone();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();
    (
        register_faux_provider(FauxProviderOptions {
            api,
            provider: "faux".into(),
            models: vec![model],
            chunk_size: 4,
        }),
        model_id,
    )
}

/// Spawn the in-process server and return the fake client plus the tempdir
/// that holds the agent dir (must outlive the server).
async fn spawn_server(cli: &Cli, agent_dir: &Path) -> (TestClient, tokio::task::JoinHandle<Result<()>>) {
    let (client_tx, server_rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
    let (server_tx, client_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
    let blueprint = test_blueprint(cli, agent_dir);
    let server = tokio::spawn(serve_connection(blueprint, cli.clone(), server_rx, server_tx));
    (TestClient { tx: client_tx, rx: client_rx, next_id: 0 }, server)
}

/// CLI flags shared by the round-trip tests: an explicit faux model (unique
/// per test), an API key so model auth resolves, `ask` approval mode to
/// exercise permission reverse requests, and a temp session dir so nothing
/// touches the user's real session store.
fn test_cli(session_dir: &Path, model_id: &str) -> Cli {
    Cli::try_parse_from([
        "rpi",
        "--model",
        &format!("faux/{model_id}"),
        "--api-key",
        "acp-test-key",
        "--approval-mode",
        "ask",
        "--session-dir",
        session_dir.to_str().expect("session dir"),
    ])
    .expect("parse test cli")
}

/// Full handshake helper: initialize + authenticate + session/new.
async fn connect(client: &mut TestClient, cwd: &Path) -> String {
    let (response, _) = client
        .request("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }))
        .await;
    assert!(response.get("result").is_some(), "initialize must succeed: {response}");
    assert_eq!(TestClient::result(&response)["protocolVersion"], 1);

    let (response, _) = client
        .request("authenticate", json!({ "methodId": AUTH_METHOD_ID }))
        .await;
    assert!(response.get("result").is_some(), "authenticate must succeed: {response}");

    let (response, _) = client
        .request("session/new", json!({ "cwd": cwd.to_str().expect("cwd"), "mcpServers": [] }))
        .await;
    let session_id = TestClient::result(&response)["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(session_id.starts_with("sess_"), "session id prefix: {session_id}");
    session_id
}

/// Drain `session/update` notifications, returning the joined text of all
/// `agent_message_chunk` deltas in order.
fn joined_agent_text(notifications: &[Value]) -> String {
    notifications
        .iter()
        .filter(|message| {
            message["method"] == "session/update"
                && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        })
        .filter_map(|message| {
            message["params"]["update"]["content"]["text"].as_str().map(ToOwned::to_owned)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// initialize / version negotiation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_negotiates_version_capabilities_and_auth_methods() {
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let model_id = format!("faux-init");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    let (response, notifications) = client
        .request("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }))
        .await;
    assert!(notifications.is_empty(), "initialize must not stream notifications");
    let result = TestClient::result(&response);
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], false);
    assert_eq!(result["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(result["agentCapabilities"]["promptCapabilities"]["audio"], false);
    assert_eq!(result["agentCapabilities"]["sessionCapabilities"]["close"], json!({}));
    assert_eq!(result["agentInfo"]["name"], "rpi");
    assert!(result["authMethods"].as_array().is_some_and(|methods| !methods.is_empty()));

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn initialize_responds_with_our_version_when_client_version_is_unsupported() {
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let model_id = format!("faux-version");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    // Older draft clients send 0; hypothetical future clients send 2. Both
    // get our latest supported version (1) back and decide whether to stay.
    for requested in [0, 2] {
        let (response, _) = client
            .request("initialize", json!({ "protocolVersion": requested }))
            .await;
        let negotiated = TestClient::result(&response)["protocolVersion"].as_i64();
        assert_eq!(negotiated, Some(PROTOCOL_VERSION), "requested {requested}");
    }
    // Missing protocolVersion is also answered with our version.
    let (response, _) = client.request("initialize", json!({})).await;
    assert_eq!(TestClient::result(&response)["protocolVersion"], PROTOCOL_VERSION);

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn authenticate_rejects_unknown_methods_and_logout_acknowledges() {
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let model_id = format!("faux-auth");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    let (response, _) = client.request("authenticate", json!({ "methodId": "nope" })).await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);
    let (response, _) = client.request("authenticate", json!({})).await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);
    let (response, _) = client.request("logout", json!({})).await;
    assert!(response.get("result").is_some(), "logout must succeed: {response}");

    drop(client);
    server.await.expect("server join").expect("server ok");
}

// ---------------------------------------------------------------------------
// session/new error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_new_validates_cwd_and_rejects_unknown_sessions_in_prompt() {
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let model_id = format!("faux-cwd");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    let (response, _) = client.request("initialize", json!({ "protocolVersion": 1 })).await;
    assert!(response.get("result").is_some(), "{response}");

    // Relative cwd is rejected.
    let (response, _) = client.request("session/new", json!({ "cwd": "relative/path" })).await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);
    // Missing directory is a resource-not-found error.
    let missing = agent.path().join("does-not-exist");
    let (response, _) = client
        .request("session/new", json!({ "cwd": missing.to_str().expect("path") }))
        .await;
    assert_eq!(TestClient::error_code(&response), RESOURCE_NOT_FOUND);
    // mcpServers must be an array when present.
    let (response, _) = client
        .request(
            "session/new",
            json!({ "cwd": agent.path().to_str().expect("cwd"), "mcpServers": "nope" }),
        )
        .await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);

    // Prompting an unknown session is a resource-not-found error.
    let (response, _) = client
        .request("session/prompt", json!({ "sessionId": "sess_missing", "prompt": [] }))
        .await;
    assert_eq!(TestClient::error_code(&response), RESOURCE_NOT_FOUND);

    // Unknown methods are method-not-found errors.
    let (response, _) = client.request("session/load", json!({})).await;
    assert_eq!(TestClient::error_code(&response), METHOD_NOT_FOUND);

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn session_new_fails_with_actionable_error_when_no_model_resolves() {
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    // An unresolvable model spec fails deterministically at session creation
    // (hermetic: no registered provider matches, so no credential probing).
    let cli = Cli::try_parse_from([
        "rpi",
        "--model",
        "definitely-not-a-real-provider/xyz",
        "--session-dir",
        sessions.path().to_str().expect("session dir"),
    ])
    .expect("parse cli");
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    let (response, _) = client.request("initialize", json!({ "protocolVersion": 1 })).await;
    assert!(response.get("result").is_some(), "{response}");
    let (response, _) = client
        .request("session/new", json!({ "cwd": agent.path().to_str().expect("cwd") }))
        .await;
    let error = &response["error"];
    assert!(
        error.get("message").and_then(Value::as_str).is_some_and(|message| {
            message.contains("definitely-not-a-real-provider/xyz")
        }),
        "session/new must fail with the unresolved model named: {response}"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

// ---------------------------------------------------------------------------
// prompt round trip (streaming assistant text)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_streams_assistant_text_and_returns_end_turn() {
    let (registration, model_id) = register_faux("prompt");
    registration.set_responses(vec![FauxResponse::text("Hello ACP")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    let (response, notifications) = client
        .request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hi there" }] }),
        )
        .await;
    assert_eq!(TestClient::result(&response)["stopReason"], "end_turn");

    // The user message is echoed as one chunk.
    let user_chunks = notifications
        .iter()
        .filter(|message| {
            message["params"]["update"]["sessionUpdate"] == "user_message_chunk"
        })
        .collect::<Vec<_>>();
    assert_eq!(user_chunks.len(), 1, "one user_message_chunk: {notifications:?}");
    assert_eq!(
        user_chunks[0]["params"]["update"]["content"]["text"],
        "hi there",
        "echoed prompt text"
    );

    // The assistant's response streams as agent_message_chunk deltas.
    let text = joined_agent_text(&notifications);
    assert_eq!(text, "Hello ACP", "assistant text must stream through chunks");
    assert!(
        notifications.iter().any(|message| {
            message["params"]["update"]["sessionUpdate"] == "usage_update"
        }),
        "a usage_update should be emitted after the turn"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn prompt_embeds_images_and_resource_links() {
    let (registration, model_id) = register_faux("prompt-blocks");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    // A tiny 1x1 transparent PNG plus a file to embed via resource_link.
    let png = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
    );
    std::fs::write(agent.path().join("context.txt"), "embedded file contents")
        .expect("write context file");
    let link_uri = format!("file://{}", agent.path().join("context.txt").display());
    let (response, notifications) = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "look at this" },
                    { "type": "image", "mimeType": "image/png", "data": png },
                    { "type": "resource_link", "uri": link_uri },
                ],
            }),
        )
        .await;
    assert_eq!(TestClient::result(&response)["stopReason"], "end_turn");
    let echo = notifications
        .iter()
        .find(|message| message["params"]["update"]["sessionUpdate"] == "user_message_chunk")
        .expect("user echo");
    let echoed = echo["params"]["update"]["content"]["text"].as_str().expect("echo text");
    assert!(echoed.contains("embedded file contents"), "resource_link must be embedded: {echoed}");

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn concurrent_prompt_on_same_session_is_rejected() {
    // One prompt turn per session at a time: a second session/prompt while a
    // turn is in flight must fail with INTERNAL_ERROR (and the first turn is
    // unaffected — it still completes normally after the client decides).
    let (registration, model_id) = register_faux("concurrent");
    std::fs::write("/tmp/acp-concurrent.txt", "data").expect("notes file");
    registration.set_responses(vec![
        tool_call_response("call-x", "read", json!({ "path": "/tmp/acp-concurrent.txt" })),
        FauxResponse::text("first turn done"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    // The first prompt deterministically blocks on the permission request.
    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 500,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] },
        }))
        .await
        .expect("send first prompt");
    let (permission_id, _) = wait_for_permission_request(&mut client).await;

    // A second prompt on the same session is refused while the first runs.
    let (response, _) = client
        .request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "me too" }] }),
        )
        .await;
    assert_eq!(
        TestClient::error_code(&response),
        INTERNAL_ERROR,
        "concurrent prompt must be rejected: {response}"
    );

    // The first turn still completes once the client allows the tool.
    client
        .respond(
            &permission_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;
    let prompt_response = loop {
        let message = client.next().await;
        if message.get("id") == Some(&json!(500)) {
            break message;
        }
    };
    assert_eq!(
        TestClient::result(&prompt_response)["stopReason"], "end_turn",
        "first turn must complete: {prompt_response}"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn prompt_without_content_or_with_audio_is_invalid() {
    let (registration, model_id) = register_faux("prompt-invalid");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    let (response, _) = client
        .request("session/prompt", json!({ "sessionId": session_id, "prompt": [] }))
        .await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);
    let (response, _) = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "audio", "mimeType": "audio/wav", "data": "abc" }],
            }),
        )
        .await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);

    drop(client);
    server.await.expect("server join").expect("server ok");
}

/// Contract (ACP-1): a finished turn releases the session's in-flight slot,
/// so a second sequential prompt on the same session completes instead of
/// failing with "session is already processing a prompt".
#[tokio::test]
async fn sequential_prompts_on_same_session_both_complete() {
    let (registration, model_id) = register_faux("sequential");
    registration.set_responses(vec![
        FauxResponse::text("first turn"),
        FauxResponse::text("second turn"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    for (prompt, expected) in [
        ("first", "first turn"),
        ("second", "second turn"),
    ] {
        let (response, notifications) = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": prompt }],
                }),
            )
            .await;
        assert_eq!(
            TestClient::result(&response)["stopReason"], "end_turn",
            "prompt {prompt:?} must complete: {response}"
        );
        assert_eq!(
            joined_agent_text(&notifications),
            expected,
            "the {prompt} turn must stream its own reply"
        );
    }

    drop(client);
    server.await.expect("server join").expect("server ok");
}

/// Contract (ACP-1): a FAILED first turn still releases the session — the
/// second prompt after a run failure succeeds (failed-first recovery).
#[tokio::test]
async fn failed_prompt_releases_session_for_retry() {
    let (registration, model_id) = register_faux("failed-first");
    registration.set_responses(vec![
        FauxResponse::error("simulated run failure"),
        FauxResponse::text("recovered"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    let (response, _) = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "boom" }],
            }),
        )
        .await;
    assert_eq!(
        TestClient::error_code(&response),
        INTERNAL_ERROR,
        "the failing turn must resolve with INTERNAL_ERROR: {response}"
    );

    let (response, notifications) = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "retry" }],
            }),
        )
        .await;
    assert_eq!(
        TestClient::result(&response)["stopReason"], "end_turn",
        "the retry prompt must complete after the failure: {response}"
    );
    assert_eq!(joined_agent_text(&notifications), "recovered");

    drop(client);
    server.await.expect("server join").expect("server ok");
}

/// Contract (ACP-3): concurrent sessions route every permission request to
/// their own session id. Session A's SECOND tool call happens after session B
/// exists; it must still carry A's id (the shared-slot bug sent it to B).
///
/// The faux provider answers model calls from one FIFO queue shared by every
/// session, so the queue consumption is orchestrated: A pops `call-a1` (A1),
/// A pops `call-a2` (A2, after B exists), B pops `A done` (terminal text),
/// A pops `call-b1` (A3), A pops `B done` (terminal text).
#[tokio::test]
async fn concurrent_sessions_keep_permission_requests_session_correct() {
    let (registration, model_id) = register_faux("two-sessions");
    std::fs::write("/tmp/acp-two-sessions.txt", "data").expect("notes file");
    registration.set_responses(vec![
        tool_call_response("call-a1", "read", json!({ "path": "/tmp/acp-two-sessions.txt" })),
        tool_call_response("call-a2", "read", json!({ "path": "/tmp/acp-two-sessions.txt" })),
        FauxResponse::text("A done"),
        tool_call_response("call-b1", "read", json!({ "path": "/tmp/acp-two-sessions.txt" })),
        FauxResponse::text("B done"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    // Session A starts a turn and blocks on its first permission request.
    let session_a = connect(&mut client, agent.path()).await;
    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 700,
            "method": "session/prompt",
            "params": { "sessionId": session_a, "prompt": [{ "type": "text", "text": "go A" }] },
        }))
        .await
        .expect("send prompt A");
    let (permission_a1_id, permission_a1) = wait_for_permission_request(&mut client).await;
    assert_eq!(
        permission_a1["params"]["sessionId"], session_a,
        "A's first permission request must carry A: {permission_a1}"
    );
    assert_eq!(permission_a1["params"]["toolCall"]["toolCallId"], "call-a1");

    // Session B is created while A's turn is still blocked.
    let session_b = connect(&mut client, agent.path()).await;
    assert_ne!(session_a, session_b, "the two sessions must differ");

    // Allow A's first tool; A's turn immediately issues a SECOND permission
    // request — now that B's session exists, it must still carry A's id
    // (the shared-slot bug rewrote it to B here).
    client
        .respond(
            &permission_a1_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;
    let (permission_a2_id, permission_a2) = wait_for_permission_request(&mut client).await;
    assert_eq!(
        permission_a2["params"]["sessionId"], session_a,
        "A's second permission request must carry A even with B active: {permission_a2}"
    );
    assert_eq!(permission_a2["params"]["toolCall"]["toolCallId"], "call-a2");

    // B's prompt now pops the queued text response, so its turn completes
    // without a permission request (the FIFO is deterministic at this point:
    // A already consumed call-a1 and call-a2).
    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 701,
            "method": "session/prompt",
            "params": { "sessionId": session_b, "prompt": [{ "type": "text", "text": "go B" }] },
        }))
        .await
        .expect("send prompt B");
    let response_701 = loop {
        let message = client.next().await;
        if message.get("id") == Some(&json!(701)) {
            break message;
        }
    };
    assert_eq!(
        TestClient::result(&response_701)["stopReason"], "end_turn",
        "B's text turn must complete: {response_701}"
    );

    // A's turn continues after its second permission is granted: the next
    // permission request is A's third tool call, still on session A.
    client
        .respond(
            &permission_a2_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;
    let (permission_a3_id, permission_a3) = wait_for_permission_request(&mut client).await;
    assert_eq!(
        permission_a3["params"]["sessionId"], session_a,
        "A's third permission request must carry A: {permission_a3}"
    );
    assert_eq!(permission_a3["params"]["toolCall"]["toolCallId"], "call-b1");
    client
        .respond(
            &permission_a3_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;

    // A's turn finishes with the final queued text.
    let response_700 = loop {
        let message = client.next().await;
        if message.get("id") == Some(&json!(700)) {
            break message;
        }
    };
    assert_eq!(
        TestClient::result(&response_700)["stopReason"], "end_turn",
        "A's turn must complete after all tools are allowed: {response_700}"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

/// Contract (ACP-5): internal session-creation failures reach the wire as
/// stable, path-free INTERNAL_ERROR messages — the agent dir's absolute path
/// must never leak into the ACP response.
#[tokio::test]
async fn session_new_internal_errors_are_path_free_on_the_wire() {
    let (registration, model_id) = register_faux("path-free");
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    // Corrupt settings force ResourceManager::new to fail with a chain that
    // names the settings file's absolute path.
    std::fs::write(agent.path().join("settings.json"), "{ not valid json !!!")
        .expect("corrupt settings");
    let (mut client, server) = spawn_server(&cli, agent.path()).await;

    let (response, _) = client.request("initialize", json!({ "protocolVersion": 1 })).await;
    assert!(response.get("result").is_some(), "{response}");
    let (response, _) = client
        .request("session/new", json!({ "cwd": agent.path().to_str().expect("cwd") }))
        .await;
    assert_eq!(TestClient::error_code(&response), INTERNAL_ERROR);
    let message = response["error"]["message"].as_str().expect("error message");
    let agent_dir = agent.path().to_str().expect("agent dir");
    assert!(
        !message.contains(agent_dir),
        "the wire error must not leak the agent dir path: {message}"
    );
    assert!(
        !message.contains("/tmp/"),
        "the wire error must not leak any absolute temp path: {message}"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

// ---------------------------------------------------------------------------
// reverse request: session/request_permission (allow / deny / cancel)
// ---------------------------------------------------------------------------

fn tool_call_response(id: &str, name: &str, arguments: Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: pi_ai::StopReason::ToolUse,
        error_message: None,
    }
}

async fn wait_for_permission_request(
    client: &mut TestClient,
) -> (Value, Value) {
    loop {
        let message = client.next().await;
        if message.get("method") == Some(&json!("session/request_permission")) {
            return (message.get("id").cloned().expect("request id"), message);
        }
        // Streamed tool_call notifications may arrive before the request.
        assert_eq!(
            message["method"], "session/update",
            "expected a permission request, got: {message}"
        );
    }
}

#[tokio::test]
async fn permission_allow_executes_tool_and_continues_turn() {
    let (registration, model_id) = register_faux("permission-allow-2");
    std::fs::write("/tmp/acp-perm-allow.txt", "read me").expect("notes file");
    registration.set_responses(vec![
        tool_call_response("call-a", "read", json!({ "path": "/tmp/acp-perm-allow.txt" })),
        FauxResponse::text("finished reading"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    // Send the prompt; the turn blocks on the permission request.
    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "read it" }] },
        }))
        .await
        .expect("send prompt");

    let (permission_id, permission) = wait_for_permission_request(&mut client).await;
    assert_eq!(
        permission["params"]["sessionId"], session_id,
        "permission request must carry the session id"
    );
    assert_eq!(permission["params"]["toolCall"]["toolCallId"], "call-a");
    let options = permission["params"]["options"].as_array().expect("options");
    assert!(
        options
            .iter()
            .any(|option| option["optionId"] == "allow-once" && option["kind"] == "allow_once"),
        "allow-once option must be offered: {permission}"
    );
    assert!(
        options
            .iter()
            .any(|option| option["optionId"] == "reject-once" && option["kind"] == "reject_once"),
        "reject-once option must be offered"
    );

    client
        .respond(
            &permission_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;

    // The tool executes and the turn continues to the queued text response.
    let mut saw_completed = false;
    let mut prompt_response = None;
    while prompt_response.is_none() {
        let message = client.next().await;
        if message.get("id") == Some(&json!(100)) {
            prompt_response = Some(message);
            break;
        }
        if message["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && message["params"]["update"]["toolCallId"] == "call-a"
        {
            saw_completed = true;
        }
    }
    let prompt_response = prompt_response.expect("prompt response");
    assert_eq!(
        TestClient::result(&prompt_response)["stopReason"], "end_turn",
        "allow must not cancel the turn: {prompt_response}"
    );
    assert!(saw_completed, "the allowed tool must report a tool_call_update");

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn permission_deny_blocks_tool_and_reports_failed() {
    let (registration, model_id) = register_faux("permission-deny");
    registration.set_responses(vec![
        tool_call_response("call-b", "read", json!({ "path": "/tmp/acp-perm-deny.txt" })),
        FauxResponse::text("after denial"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "do it" }] },
        }))
        .await
        .expect("send prompt");

    let (permission_id, _) = wait_for_permission_request(&mut client).await;
    client
        .respond(
            &permission_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } }),
        )
        .await;

    let mut saw_failed = false;
    let mut prompt_response = None;
    while prompt_response.is_none() {
        let message = client.next().await;
        if message.get("id") == Some(&json!(200)) {
            prompt_response = Some(message);
            break;
        }
        if message["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && message["params"]["update"]["toolCallId"] == "call-b"
        {
            saw_failed = message["params"]["update"]["status"] == "failed";
        }
    }
    assert!(
        saw_failed,
        "the denied tool must be reported as failed (status: failed)"
    );
    assert_eq!(
        TestClient::result(&prompt_response.expect("prompt response"))["stopReason"],
        "end_turn",
        "a denial blocks the tool but not the turn"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn permission_rules_deny_and_allow_short_circuit_before_reverse_request() {
    // Path-level permissionRules are evaluated BEFORE the capability-wide
    // approval mode: a deny rule blocks the tool and an allow rule lets it
    // run, and neither may reach the client as a session/request_permission
    // reverse request.
    let (registration, model_id) = register_faux("rules");
    std::fs::write("/tmp/acp-rules-deny.txt", "secret").expect("deny file");
    std::fs::write("/tmp/acp-rules-allow.txt", "public").expect("allow file");
    registration.set_responses(vec![
        tool_call_response("call-d", "read", json!({ "path": "/tmp/acp-rules-deny.txt" })),
        tool_call_response("call-a", "read", json!({ "path": "/tmp/acp-rules-allow.txt" })),
        FauxResponse::text("done"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    // Global settings in the agent dir: one deny rule and one allow rule,
    // both for the read tool.
    std::fs::write(
        agent.path().join("settings.json"),
        r#"{
            "permissionRules": [
                { "action": "deny", "path": "/tmp/acp-rules-deny.txt" },
                { "action": "allow", "path": "/tmp/acp-rules-allow.txt" }
            ]
        }"#,
    )
    .expect("write settings.json");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 400,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "do it" }] },
        }))
        .await
        .expect("send prompt");

    let mut saw_denied = false;
    let mut saw_allowed = false;
    let mut prompt_response = None;
    while prompt_response.is_none() {
        let message = client.next().await;
        assert_ne!(
            message.get("method"),
            Some(&json!("session/request_permission")),
            "path rules must decide without a reverse request: {message}"
        );
        if message.get("id") == Some(&json!(400)) {
            prompt_response = Some(message);
            break;
        }
        if message["params"]["update"]["sessionUpdate"] == "tool_call_update" {
            match message["params"]["update"]["toolCallId"].as_str() {
                Some("call-d") => {
                    saw_denied = message["params"]["update"]["status"] == "failed";
                }
                Some("call-a") => {
                    saw_allowed = message["params"]["update"]["status"] == "completed";
                }
                _ => {}
            }
        }
    }
    assert!(saw_denied, "the denied tool must be reported failed");
    assert!(saw_allowed, "the allowed tool must be reported completed");
    assert_eq!(
        TestClient::result(&prompt_response.expect("prompt response"))["stopReason"],
        "end_turn",
        "rules block/allow tools but not the turn"
    );

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn cancel_aborts_turn_and_resolves_pending_permission_as_cancelled() {
    let (registration, model_id) = register_faux("permission-cancel");
    registration.set_responses(vec![
        tool_call_response("call-c", "read", json!({ "path": "/tmp/acp-perm-cancel.txt" })),
        FauxResponse::text("should not be reached"),
    ]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    client
        .tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": 300,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] },
        }))
        .await
        .expect("send prompt");

    // The turn is deterministically blocked on the permission request.
    let (permission_id, _) = wait_for_permission_request(&mut client).await;

    // Cancel the turn; the pending permission must resolve as cancelled.
    client.notify("session/cancel", json!({ "sessionId": session_id })).await;

    let prompt_response = loop {
        let message = client.next().await;
        if message.get("id") == Some(&json!(300)) {
            break message;
        }
    };
    assert_eq!(
        TestClient::result(&prompt_response)["stopReason"], "cancelled",
        "a cancelled turn must respond with stopReason cancelled: {prompt_response}"
    );

    // The permission request is resolved (the tool block returns without
    // needing a client decision) — responding late is ignored.
    client
        .respond(
            &permission_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        )
        .await;

    drop(client);
    server.await.expect("server join").expect("server ok");
}

// ---------------------------------------------------------------------------
// session/close and prompt-without-session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_close_releases_the_session() {
    let (registration, model_id) = register_faux("close");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    let (response, _) = client
        .request("session/close", json!({ "sessionId": session_id }))
        .await;
    assert!(response.get("result").is_some(), "session/close must succeed: {response}");

    // Prompting a closed session fails with resource-not-found.
    let (response, _) = client
        .request("session/prompt", json!({ "sessionId": session_id, "prompt": [] }))
        .await;
    assert_eq!(TestClient::error_code(&response), RESOURCE_NOT_FOUND);

    drop(client);
    server.await.expect("server join").expect("server ok");
}

#[tokio::test]
async fn session_cancel_as_request_acknowledges_and_validates() {
    // The spec defines session/cancel as a notification, but the server also
    // answers the request form (some clients send one); a request must be
    // acknowledged with a result, and a missing sessionId must fail with
    // INVALID_PARAMS rather than crash the connection.
    let (registration, model_id) = register_faux("cancel-req");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let (mut client, server) = spawn_server(&cli, agent.path()).await;
    let session_id = connect(&mut client, agent.path()).await;

    let (response, _) = client
        .request("session/cancel", json!({ "sessionId": session_id }))
        .await;
    assert!(response.get("result").is_some(), "cancel as request: {response}");

    let (response, _) = client.request("session/cancel", json!({})).await;
    assert_eq!(TestClient::error_code(&response), INVALID_PARAMS);

    drop(client);
    server.await.expect("server join").expect("server ok");
}

// ---------------------------------------------------------------------------
// framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_frames_round_trip_through_encode_and_pump() {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    let frame = pi_coding::tools::framing::encode_message(&body).expect("encode");
    let serialized = serde_json::to_vec(&body).expect("serialize");
    let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
    assert!(frame.starts_with(header.as_bytes()), "frame must start with the header");
    assert_eq!(&frame[header.len()..], &serialized[..]);

    let (tx, mut rx) = mpsc::channel(4);
    let (errors, _error_rx) = mpsc::channel(4);
    let reader = tokio::spawn(read_stdio_frames(std::io::Cursor::new(frame), tx, errors));
    let decoded = rx.recv().await.expect("decoded message");
    assert_eq!(decoded, body);
    drop(rx);
    reader.await.expect("reader join").expect("reader ok");
}

#[tokio::test]
async fn stdio_reader_survives_parse_errors_but_stops_on_framing_errors() {
    // Frame 1 is valid; frame 2 declares a correct length but invalid JSON.
    let good = pi_coding::tools::framing::encode_message(&json!({ "jsonrpc": "2.0", "id": 1 }))
        .expect("good frame");
    let bad_body = b"not json at all";
    let bad = format!("Content-Length: {}\r\n\r\n", bad_body.len());
    let mut payload = good.clone();
    payload.extend_from_slice(bad.as_bytes());
    payload.extend_from_slice(bad_body);
    // Frame 3 is valid again: parsing must resync after the parse error.
    payload.extend(
        pi_coding::tools::framing::encode_message(&json!({ "jsonrpc": "2.0", "id": 3 }))
            .expect("third frame"),
    );

    let (tx, mut rx) = mpsc::channel(8);
    let (errors, mut error_rx) = mpsc::channel(8);
    let reader = tokio::spawn(read_stdio_frames(std::io::Cursor::new(payload), tx, errors));

    let first = rx.recv().await.expect("first frame");
    assert_eq!(first["id"], 1);
    let parse_error = error_rx.recv().await.expect("parse error response");
    assert_eq!(parse_error["error"]["code"], PARSE_ERROR);
    let third = rx.recv().await.expect("third frame");
    assert_eq!(third["id"], 3);
    assert!(rx.recv().await.is_none(), "reader must close after the stream ends");
    drop(rx);
    reader.await.expect("reader join").expect("reader ok");

    // A truncated body (declared length exceeds available bytes) ends the
    // reader without forwarding anything further.
    let truncated = format!(
        "Content-Length: 1000\r\n\r\n{}",
        serde_json::to_string(&good).expect("json")
    );
    let (tx, mut rx) = mpsc::channel(4);
    let (errors, _error_rx) = mpsc::channel(4);
    let reader = tokio::spawn(read_stdio_frames(
        std::io::Cursor::new(truncated.into_bytes()),
        tx,
        errors,
    ));
    assert!(rx.recv().await.is_none(), "truncated frame must close the reader");
    drop(rx);
    reader.await.expect("reader join").expect("reader ok");
}

// ---------------------------------------------------------------------------
// WebSocket serve smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn websocket_connection_serves_initialize() {
    let (registration, model_id) = register_faux("ws");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let blueprint = test_blueprint(&cli, agent.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws listener");
    let address = listener.local_addr().expect("ws address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept ws");
        handle_ws_connection(stream, blueprint, cli, None).await.expect("ws connection")
    });

    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    // A native client (no Origin header) is the tokenless-loopback case.
    let url = format!("ws://{address}/");
    let request = url.clone().into_client_request().expect("ws request");
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.expect("ws connect");

    let id = json!(1);
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": { "protocolVersion": 1 },
            }))
            .expect("initialize json")
            .into(),
        ))
        .await
        .expect("send initialize");
    let message = match socket.next().await.expect("ws message") {
        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => text,
        other => panic!("expected text message, got {other:?}"),
    };
    let response: Value = serde_json::from_str(&message).expect("response json");
    assert_eq!(response["id"], id);
    assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);

    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("close ws");
    server.await.expect("ws server join");
}

/// Contract (ACP-2): the tokenless loopback policy rejects browser clients —
/// they always send `Origin` — with HTTP 401 before any ACP message, while a
/// native client (no Origin) is upgraded and served.
#[tokio::test]
async fn websocket_rejects_browser_origin_without_token_but_accepts_native() {
    let (registration, model_id) = register_faux("ws-origin");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let blueprint = test_blueprint(&cli, agent.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws listener");
    let address = listener.local_addr().expect("ws address");
    let server = tokio::spawn(async move {
        // First connection: a browser Origin must be refused (401) and the
        // handler returns without upgrading.
        let (stream, _) = listener.accept().await.expect("accept browser conn");
        let _ = handle_ws_connection(stream, blueprint.clone(), cli.clone(), None).await;
        // Second connection: a native client must be served.
        let (stream, _) = listener.accept().await.expect("accept native conn");
        handle_ws_connection(stream, blueprint, cli, None).await.expect("native ws connection")
    });

    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let url = format!("ws://{address}/");
    let mut browser = url.clone().into_client_request().expect("browser ws request");
    browser
        .headers_mut()
        .insert("Origin", http::HeaderValue::from_static("http://localhost"));
    match tokio_tungstenite::connect_async(browser).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(
                response.status(),
                http::StatusCode::UNAUTHORIZED,
                "browser Origin must be rejected with 401"
            );
        }
        other => panic!("browser Origin must be rejected, got {other:?}"),
    }

    let request = url.clone().into_client_request().expect("native ws request");
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.expect("native ws connect");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("close ws");
    server.await.expect("ws server join");
}

/// Contract (ACP-2): with a token configured, the `rpi-auth.<token>`
/// subprotocol authenticates (and is echoed in the upgrade response), while a
/// wrong token, a missing subprotocol, and an unrelated subprotocol are all
/// refused with 401; the `Authorization: Bearer` header path also works.
#[tokio::test]
async fn websocket_token_policy_accepts_valid_subprotocol_and_rejects_wrong_missing_and_bearerless() {
    let (registration, model_id) = register_faux("ws-token");
    registration.set_responses(vec![FauxResponse::text("ok")]);
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let blueprint = test_blueprint(&cli, agent.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws listener");
    let address = listener.local_addr().expect("ws address");
    let token: Option<Arc<[u8]>> = Some(Arc::from(&b"secret"[..]));
    let server = tokio::spawn(async move {
        // The accept loop must not block on a refused connection: every
        // connection is handed to its own handler task.
        for _ in 0..5 {
            let (stream, _) = listener.accept().await.expect("accept ws conn");
            let blueprint = blueprint.clone();
            let cli = cli.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _ = handle_ws_connection(stream, blueprint, cli, token).await;
            });
        }
    });

    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let url = format!("ws://{address}/");
    let mut request = url.clone().into_client_request().expect("ws request");

    // Valid subprotocol: upgraded and the exact protocol is echoed.
    request
        .headers_mut()
        .insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_static("rpi-auth.secret"),
        );
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.expect("valid token connect");
    assert_eq!(
        response.headers().get(http::header::SEC_WEBSOCKET_PROTOCOL),
        Some(&http::HeaderValue::from_static("rpi-auth.secret")),
        "the matched subprotocol must be echoed in the upgrade response"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("close ws");

    // Wrong token: 401.
    let mut request = url.clone().into_client_request().expect("ws request");
    request
        .headers_mut()
        .insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_static("rpi-auth.wrong"),
        );
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "wrong token must be rejected");
        }
        other => panic!("wrong token must be rejected, got {other:?}"),
    }

    // Missing subprotocol (no token anywhere): 401 when a token is configured.
    let request = url.clone().into_client_request().expect("ws request");
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "missing token must be rejected");
        }
        other => panic!("missing token must be rejected, got {other:?}"),
    }

    // Unrelated subprotocol: 401.
    let mut request = url.clone().into_client_request().expect("ws request");
    request
        .headers_mut()
        .insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_static("chat"),
        );
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "unrelated subprotocol must be rejected");
        }
        other => panic!("unrelated subprotocol must be rejected, got {other:?}"),
    }

    // Bearer header path: accepted.
    let mut request = url.clone().into_client_request().expect("ws request");
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, http::HeaderValue::from_static("Bearer secret"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.expect("bearer connect");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("close ws");
    server.await.expect("ws server join");
}

/// Contract (ACP-4): concurrent WebSocket connection tasks are capped at
/// [`MAX_CONNECTION_TASKS`]; the next connection is accepted and dropped
/// without an upgrade instead of spawning an unbounded task.
#[tokio::test]
async fn websocket_connection_cap_drops_saturated_connections() {
    let (registration, model_id) = register_faux("ws-cap");
    let agent = tempfile::tempdir().expect("agent dir");
    let sessions = tempfile::tempdir().expect("sessions dir");
    let cli = test_cli(sessions.path(), &model_id);
    let blueprint = test_blueprint(&cli, agent.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws listener");
    let address = listener.local_addr().expect("ws address");
    let server = tokio::spawn(serve_ws(listener, blueprint, cli, None));

    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let url = format!("ws://{address}/");
    let mut sockets = Vec::with_capacity(MAX_CONNECTION_TASKS);
    for _ in 0..MAX_CONNECTION_TASKS {
        let request = url.clone().into_client_request().expect("ws request");
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connection within the cap must upgrade");
        sockets.push(socket);
    }
    // The saturated connection is dropped before the WebSocket upgrade.
    let request = url.clone().into_client_request().expect("ws request");
    assert!(
        tokio_tungstenite::connect_async(request).await.is_err(),
        "a connection beyond the cap must be dropped"
    );
    for socket in &mut sockets {
        let _ = socket.send(tokio_tungstenite::tungstenite::Message::Close(None)).await;
    }
    server.abort();
    let _ = server.await;
}

/// Contract (ACP-2): `rpi agent serve` is loopback-only. Every non-loopback
/// address is refused before the socket is opened — even when a token file
/// is supplied (the old vulnerable path), because plaintext WebSocket cannot
/// safely carry the bearer token off the local host.
#[tokio::test]
async fn serve_refuses_non_loopback_even_with_token_file() {
    let sessions = tempfile::tempdir().expect("sessions dir");
    let token_dir = tempfile::tempdir().expect("token dir");
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, "remote-secret").expect("write token");
    let model_id = "faux-non-loopback".to_string();

    // Wildcard, distinct non-loopback, public, and documentation ranges across v4/v6.
    let negatives: &[&str] = &[
        "0.0.0.0:0",
        "[::]:0",
        "198.51.100.7:0",
        "192.0.2.1:0",
        "8.8.8.8:0",
        "[2001:db8::1]:0",
    ];
    for addr in negatives {
        for token in [None, Some(token_file.clone())] {
            let cli = test_cli(sessions.path(), &model_id);
            let error = run_serve(cli, addr.parse().expect("addr"), token)
                .await
                .expect_err("non-loopback serve must be refused");
            let message = format!("{error:#}");
            assert!(
                message.contains("loopback"),
                "non-loopback {addr}: refusal must explain loopback-only: {message}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// pure helpers
// ---------------------------------------------------------------------------

#[test]
fn parse_prompt_blocks_handles_each_content_type() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("f.txt"), "file text").expect("write file");
    let link = format!("file://{}", dir.path().join("f.txt").display());
    let params = json!({
        "prompt": [
            { "type": "text", "text": "hello" },
            { "type": "image", "mimeType": "image/png", "data": "aGVsbG8=" },
            { "type": "resource", "resource": { "uri": "file:///x.txt", "text": "embedded" } },
            { "type": "resource_link", "uri": link },
        ],
    });
    let (text, images) = parse_prompt_blocks(&params).expect("parse");
    assert!(text.contains("hello"));
    assert!(text.contains("embedded"), "resource text must be embedded");
    assert!(text.contains("file text"), "resource_link must be read: {text}");
    assert_eq!(images.len(), 1, "one image block");
    match &images[0] {
        ContentBlock::Image { data, mime_type } => {
            assert_eq!(data, "aGVsbG8=");
            assert_eq!(mime_type, "image/png");
        }
        other => panic!("expected an image block, got {other:?}"),
    }
}

#[test]
fn file_uri_to_path_decodes_local_links() {
    let absolute_path = Path::new("/").join("workspace").join("a b.txt");
    let uri = "file:///workspace/a%20b.txt";
    assert_eq!(file_uri_to_path(uri), Some(absolute_path));
    assert_eq!(file_uri_to_path("https://example.com/x"), None);
    assert_eq!(file_uri_to_path("file:///plain.txt"), Some(PathBuf::from("/plain.txt")));
}

#[test]
fn parse_incoming_routes_requests_notifications_and_responses() {
    // A message with a method and an id is a request; a method without an id
    // is a notification; a message with a result/error (and no method) is a
    // response to one of our reverse requests.
    let request = parse_incoming(&json!({
        "jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": { "x": 1 }
    }))
    .expect("request");
    let Incoming::Request { id, method, params } = request else {
        panic!("expected a request");
    };
    assert_eq!(id, json!(7));
    assert_eq!(method, "session/prompt");
    assert_eq!(params["x"], 1);

    // Missing params default to an empty object.
    let request = parse_incoming(&json!({ "jsonrpc": "2.0", "id": 8, "method": "logout" }))
        .expect("request without params");
    let Incoming::Request { params, .. } = request else {
        panic!("expected a request");
    };
    assert_eq!(params, json!({}));

    let notification = parse_incoming(&json!({
        "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "s" }
    }))
    .expect("notification");
    let Incoming::Notification { method, params } = notification else {
        panic!("expected a notification");
    };
    assert_eq!(method, "session/cancel");
    assert_eq!(params["sessionId"], "s");

    let response = parse_incoming(&json!({
        "jsonrpc": "2.0", "id": "req-1", "result": { "outcome": { "outcome": "selected" } }
    }))
    .expect("response");
    let Incoming::Response { id, result, error } = response else {
        panic!("expected a response");
    };
    assert_eq!(id, json!("req-1"));
    assert!(result.is_some(), "result carried");
    assert!(error.is_none());
}

#[test]
fn parse_incoming_rejects_malformed_messages() {
    fn code(message: &Value) -> i64 {
        match parse_incoming(message) {
            Ok(_) => panic!("malformed message must fail: {message}"),
            Err(error) => error.code,
        }
    }

    // Non-"2.0" jsonrpc version.
    assert_eq!(
        code(&json!({ "jsonrpc": "1.0", "id": 1, "method": "initialize" })),
        INVALID_REQUEST
    );
    assert_eq!(
        code(&json!({ "id": 1, "method": "initialize" })),
        INVALID_REQUEST
    );
    // A method combined with a result/error is ambiguous and rejected.
    assert_eq!(
        code(&json!({ "jsonrpc": "2.0", "id": 1, "method": "x", "result": {} })),
        INVALID_REQUEST
    );
    assert_eq!(
        code(&json!({ "jsonrpc": "2.0", "id": 1, "method": "x", "error": {} })),
        INVALID_REQUEST
    );
    // Neither a method nor a result/error.
    assert_eq!(
        code(&json!({ "jsonrpc": "2.0", "id": 1 })),
        INVALID_REQUEST
    );
}

#[test]
fn strip_absolute_paths_removes_local_paths_but_keeps_messages() {
    let absolute_path = Path::new("/").join("workspace").join(".pi/agent/settings.json");
    let message = format!("Failed to parse settings.json\nFile: {}", absolute_path.display());
    assert_eq!(
        strip_absolute_paths(&message),
        "Failed to parse settings.json File: <path>"
    );
    assert_eq!(
        strip_absolute_paths("read error at /tmp/x.rs:12:5"),
        "read error at <path>"
    );
    assert_eq!(
        strip_absolute_paths("windows path C:\\Users\\me\\x.txt failed"),
        "windows path <path> failed"
    );
    assert_eq!(
        strip_absolute_paths("plain message, no paths here"),
        "plain message, no paths here"
    );
    assert_eq!(
        strip_absolute_paths("provider 429 rate limited https://example.com/models"),
        "provider 429 rate limited https://example.com/models"
    );
}
