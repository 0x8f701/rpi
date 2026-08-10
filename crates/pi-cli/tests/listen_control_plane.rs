//! End-to-end wire contracts for the `--listen` control plane.
//!
//! Drives the real `modes::listen` server over TCP against a faux
//! `Application`, asserting exact JSON-RPC responses, WebSocket frames, auth
//! / Origin enforcement, bounded overload, shared Application identity,
//! canonical TUI extension queries, and remote interactive UI isolation —
//! never source text.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::Model;
use pi_agent::ThinkingLevel;
use pi_cli::extension_ui::ExtensionUiAdapter;
use pi_cli::modes::listen::{ListenConfig, ListenHandle, MAX_CONNECTION_TASKS, start};
use pi_coding::{
    Application, ExtensionCancellation, ExtensionInstanceId, ExtensionMode,
    ExtensionThemeDescriptor, ExtensionUiContext, ExtensionUiHost, ExtensionUiRequest,
    ExtensionUiResponse, ProcessSpawnSpec, Session, SessionOptions, TodoPhase,
    collab::{FrameDirection, capability, derive_connection_key, open_frame, parse_link, seal_frame},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};

#[path = "common/mod.rs"]
mod common;
use common::*;

#[tokio::test]
async fn http_get_state_returns_exact_rpc_response() {
    let app = faux_application("listen-get-state").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"state-1"}).to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(
        status,
        200,
        "status {status} body {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse response");
    assert_eq!(value["type"], "response");
    assert_eq!(value["command"], "get_state");
    assert_eq!(value["id"], "state-1");
    assert!(value["success"].as_bool().unwrap_or(false), "response: {value}");
    assert!(value["data"].is_object(), "data: {:?}", value["data"]);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_wrong_path_and_method() {
    let app = faux_application("listen-paths").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let (status, _) = http_get(addr, "/unknown", None).await;
    assert_eq!(status, 404);
    let body = json!({"type":"get_state"}).to_string();
    let (status, _) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200);
    let (status, _) = http_get(addr, "/rpc", None).await;
    assert_eq!(status, 404);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_missing_content_length() {
    let app = faux_application("listen-no-cl").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let request =
        b"POST /rpc HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
    let response = http_raw_exchange(addr, request).await;
    assert_eq!(parse_status(&response).unwrap_or(0), 411);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_oversized_body() {
    let app = faux_application("listen-oversized").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        5 * 1024 * 1024
    );
    let response = http_raw_exchange(addr, request.as_bytes()).await;
    assert_eq!(parse_status(&response), Some(413));
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}


#[tokio::test]
async fn ws_get_state_returns_response_and_application_events() {
    let app = faux_application("listen-ws").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"ws-1"}).to_string().into(),
    ))
    .await
    .expect("send command");

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut got_response = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "response" && value["id"] == "ws-1" {
                    assert_eq!(value["command"], "get_state");
                    assert!(value["success"].as_bool().unwrap_or(false));
                    got_response = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(got_response, "ws did not return get_state response");

    app.application
        .set_todos(vec![TodoPhase {
            name: "listen-event".into(),
            tasks: vec![],
        }])
        .expect("set todos publishes TodoUpdated");

    let mut saw_todo_event = false;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse event");
                if value["type"] == "todo_updated" {
                    assert_eq!(value["phases"][0]["name"], "listen-event");
                    saw_todo_event = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(saw_todo_event, "ws never projected application TodoUpdated");
    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn collab_wire_auth_encrypts_snapshot_commands_and_stop_close() {
    let app = faux_application("listen-collab-wire").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({
        "type": "collab_start",
        "id": "collab-start",
        "baseUrl": format!("http://{addr}"),
    })
    .to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response));
    let response: Value = serde_json::from_slice(&response).expect("start response");
    let link_text = response["data"]["controlLink"].as_str().expect("control link");
    let link = parse_link(link_text).expect("parse control link");
    let protocol = format!(
        "rpi-collab.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(capability(&link.secret.key)),
    );
    let path = format!("/collab/ws/{}", link.room_id);
    let (mut ws, handshake) = ws_connect_path_with_subprotocol(addr, &path, Some(&protocol), None)
        .await
        .expect("collab connect");
    assert_eq!(
        handshake.headers().get(http::header::SEC_WEBSOCKET_PROTOCOL).and_then(|v| v.to_str().ok()),
        Some(protocol.as_str()),
    );
    let hello = match ws.next().await.expect("hello frame").expect("hello") {
        WsMessage::Text(text) => serde_json::from_str::<Value>(&text).expect("hello json"),
        other => panic!("expected hello text, got {other:?}"),
    };
    let epoch: [u8; 8] = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(hello["epoch"].as_str().expect("epoch"))
        .expect("epoch base64")
        .try_into()
        .expect("epoch length");
    let server_key = derive_connection_key(&link.secret.key, &epoch, FrameDirection::ServerToClient)
        .expect("server key");
    let client_key = derive_connection_key(&link.secret.key, &epoch, FrameDirection::ClientToServer)
        .expect("client key");
    let snapshot_frame = match ws.next().await.expect("snapshot frame").expect("snapshot") {
        WsMessage::Binary(frame) => frame,
        other => panic!("expected encrypted snapshot, got {other:?}"),
    };
    let snapshot_plain = open_frame(
        &server_key,
        &link.room_id,
        FrameDirection::ServerToClient,
        &epoch,
        0,
        &snapshot_frame,
    )
    .expect("decrypt snapshot");
    assert!(!snapshot_frame.windows(b"sessionId".len()).any(|w| w == b"sessionId"));
    let snapshot: Value = serde_json::from_slice(&snapshot_plain).expect("snapshot json");
    assert_eq!(snapshot["type"], "snapshot");

    let command = json!({"type":"command","command":"abort","id":"abort-1"});
    let command_frame = seal_frame(
        &client_key,
        &link.room_id,
        FrameDirection::ClientToServer,
        &epoch,
        0,
        &serde_json::to_vec(&command).expect("command json"),
    )
    .expect("seal command");
    ws.send(WsMessage::Binary(command_frame.into())).await.expect("send command");
    let response_frame = match ws.next().await.expect("response frame").expect("response") {
        WsMessage::Binary(frame) => frame,
        other => panic!("expected encrypted response, got {other:?}"),
    };
    let response_plain = open_frame(
        &server_key,
        &link.room_id,
        FrameDirection::ServerToClient,
        &epoch,
        1,
        &response_frame,
    )
    .expect("decrypt response");
    let response: Value = serde_json::from_slice(&response_plain).expect("response json");
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "abort");
    assert_eq!(response["success"], true);

    handle.collab_service().stop(&link.room_id).await.expect("stop room");
    let close = tokio::time::timeout(DEADLINE, ws.next()).await.expect("close timeout");
    assert!(matches!(close, Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None));
    handle.stop().await.expect("stop listener");
    app.application.cleanup().await;
}

#[tokio::test]
async fn ws_rejects_binary_messages() {
    let app = faux_application("listen-ws-binary").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    ws.send(WsMessage::Binary(vec![1, 2, 3].into()))
        .await
        .expect("send binary");
    let closed = tokio::time::timeout(DEADLINE, ws.next()).await;
    assert!(
        matches!(
            closed,
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(Some(Err(_))) | Ok(None)
        ),
        "ws did not close on binary"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn auth_rejects_missing_and_wrong_token_and_accepts_correct() {
    let app = faux_application("listen-auth").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "secret-token").await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"auth-1"}).to_string();
    assert_eq!(http_post_rpc(addr, body.as_bytes(), None).await.0, 401);
    assert_eq!(
        http_post_rpc(addr, body.as_bytes(), Some("wrong")).await.0,
        401
    );
    let (status, response) = http_post_rpc(addr, body.as_bytes(), Some("secret-token")).await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_slice(&response).expect("parse response");
    assert!(value["success"].as_bool().unwrap_or(false));
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn auth_rejects_ws_without_token() {
    let app = faux_application("listen-ws-auth").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "ws-secret").await;
    let addr = handle.local_addr();
    let result = tokio::time::timeout(
        DEADLINE,
        tokio_tungstenite::connect_async(
            format!("ws://{addr}/ws")
                .into_client_request()
                .unwrap(),
        ),
    )
    .await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "ws without token should fail"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// `GET /web` serves the self-contained web client page (200, text/html,
/// inline assets only) without authentication — the page carries no data and
/// every command/event flows through the token-gated /rpc and /ws routes.
#[tokio::test]
async fn http_get_web_serves_self_contained_page_without_auth() {
    let app = faux_application("listen-web-page").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();

    let response = http_raw_exchange(
        addr,
        b"GET /web HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200"), "status line: {text}");
    assert!(
        text.contains("content-type: text/html; charset=utf-8"),
        "content type: {text}"
    );
    assert!(text.contains("<!doctype html"), "page doctype");
    assert!(text.contains("<script"), "inline script, no external assets");
    assert!(text.contains("</html>"), "page closing tag");
    assert!(
        !text.contains("src=\"http") && !text.contains("href=\"http"),
        "page must not reference external assets"
    );
    assert!(text.contains("rpi-auth."), "page must document the subprotocol");

    // The page itself stays auth-optional even when a token is configured.
    let (status, _) = http_get(addr, "/web", None).await;
    assert_eq!(status, 200, "auth-optional /web with token configured");

    // Other GET paths remain 404, and /rpc + /ws auth is unchanged.
    let (status, _) = http_get(addr, "/", None).await;
    assert_eq!(status, 404);
    let (status, _) = http_get(addr, "/nope", None).await;
    assert_eq!(status, 404);

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// Browser WebSocket auth via the `Sec-WebSocket-Protocol: rpi-auth.<token>`
/// subprotocol: a matching token authenticates even with a browser Origin,
/// and the server echoes the exact offered protocol in the handshake.
#[tokio::test]
async fn ws_subprotocol_token_auth_accepts_and_echoes() {
    let app = faux_application("listen-ws-subprotocol-ok").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "web-token-ok").await;
    let addr = handle.local_addr();

    let (mut ws, response) = ws_connect_with_subprotocol(
        addr,
        Some("rpi-auth.web-token-ok"),
        Some("https://app.example"),
    )
    .await
    .expect("subprotocol-authenticated ws must connect");
    assert_eq!(
        response
            .headers()
            .get(http::header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some("rpi-auth.web-token-ok"),
        "server must reflect the chosen subprotocol (RFC 6455)"
    );

    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"subproto-1"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "subproto-1" {
                    assert!(value["success"].as_bool().unwrap_or(false));
                    ok = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "subprotocol-authenticated get_state missing");

    // The Authorization header path still works untouched on the same listener.
    let mut auth_ws = ws_connect_with_origin(addr, Some("web-token-ok"), None).await;
    auth_ws
        .send(WsMessage::Text(
            json!({"type":"get_state","id":"header-1"}).to_string().into(),
        ))
        .await
        .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut header_ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, auth_ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "header-1" {
                    header_ok = value["success"].as_bool().unwrap_or(false);
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(header_ok, "authorization-header ws must keep working");

    ws.close(None).await.ok();
    auth_ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// The subprotocol channel rejects wrong tokens and empty candidates with a
/// token configured. Tokenless, the channel grants nothing but also demands
/// nothing: a browser connection is accepted with or without a subprotocol.
#[tokio::test]
async fn ws_subprotocol_wrong_token_and_missing_token_rejected() {
    let app = faux_application("listen-ws-subprotocol-bad").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "web-token-secret").await;
    let addr = handle.local_addr();

    // Wrong token: the handshake must fail outright.
    let result = ws_connect_with_subprotocol(
        addr,
        Some("rpi-auth.web-token-wrong"),
        Some("https://app.example"),
    )
    .await;
    assert!(
        result.is_err(),
        "wrong subprotocol token must fail: {result:?}"
    );

    // Empty candidate after the prefix.
    let result = ws_connect_with_subprotocol(addr, Some("rpi-auth."), Some("https://app.example"))
        .await;
    assert!(
        result.is_err(),
        "empty subprotocol candidate must fail: {result:?}"
    );

    // Browser connection with no subprotocol and no Authorization header.
    let result = ws_connect_with_subprotocol(addr, None, Some("https://app.example")).await;
    assert!(
        result.is_err(),
        "browser origin without any token channel must fail: {result:?}"
    );

    // A non-auth subprotocol must not authenticate either.
    let result = ws_connect_with_subprotocol(addr, Some("chat"), Some("https://app.example")).await;
    assert!(
        result.is_err(),
        "unrelated subprotocol must fail: {result:?}"
    );

    handle.stop().await.expect("stop");
    app.application.cleanup().await;

    // Tokenless loopback: there is no configured token to compare against, so
    // the subprotocol channel grants nothing. A browser connection without a
    // subprotocol is accepted and served; offering one gets no echo (the
    // server never authenticates a protocol it could not match), which makes
    // the client abort the handshake — the correct RFC 6455 outcome.
    let tokenless = faux_application("listen-ws-subprotocol-tokenless").await;
    let (handle, _extension_ui) = listen(tokenless.application.clone()).await;
    let addr = handle.local_addr();
    let origin = format!("http://{addr}");

    let (mut ws, response) = ws_connect_with_subprotocol(addr, None, Some(&origin))
        .await
        .expect("tokenless loopback must accept browser ws without a subprotocol");
    assert_eq!(
        response
            .headers()
            .get(http::header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        None,
        "no configured token means no subprotocol may be echoed"
    );
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"tokenless-subproto-1"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "tokenless-subproto-1" {
                    ok = value["success"].as_bool().unwrap_or(false);
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "tokenless browser ws must serve get_state");
    ws.close(None).await.ok();

    // A subprotocol offer on a tokenless listener is never echoed, so the
    // client-side handshake aborts (nothing to authenticate against).
    let result = ws_connect_with_subprotocol(
        addr,
        Some("rpi-auth.web-token-secret"),
        Some(&origin),
    )
    .await;
    assert!(
        result.is_err(),
        "unmatched subprotocol must not be echoed on a tokenless listener"
    );

    handle.stop().await.expect("stop");
    tokenless.application.cleanup().await;
}

#[tokio::test]
async fn tokenless_loopback_accepts_same_origin_browser_over_http_and_ws() {
    let app = faux_application("listen-origin").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let origin = format!("http://{addr}");
    let body = json!({"type":"get_state","id":"origin-1"}).to_string();

    // A browser-origin /rpc POST whose Origin authority matches the
    // request's Host (the page the user opened) carries no token and is
    // accepted.
    let (status, response) = http_post_rpc_with_headers(
        addr,
        body.as_bytes(),
        None,
        &[("origin", &origin)],
    )
    .await;
    assert_eq!(
        status,
        200,
        "same-origin browser without token must be accepted: {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse rpc response");
    assert_eq!(value["id"], "origin-1");
    assert_eq!(value["success"], true);

    let mut ws = try_ws_connect(addr, None, Some(&origin))
        .await
        .expect("ws same-origin browser without token must connect");
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"origin-ws-1"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "origin-ws-1" {
                    ok = value["success"].as_bool().unwrap_or(false);
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "tokenless browser ws must serve get_state");
    ws.close(None).await.ok();

    // Ordinary request same-origin: a page from an unrelated origin is
    // rejected over both channels even though the listener is tokenless.
    let (status, _) = http_post_rpc_with_headers(
        addr,
        body.as_bytes(),
        None,
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(status, 401, "foreign browser origin without token must be 401");
    assert!(
        try_ws_connect(addr, None, Some("https://evil.example"))
            .await
            .is_err(),
        "ws foreign browser origin without token must fail"
    );

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn token_authenticated_browser_origin_is_accepted() {
    let app = faux_application("listen-origin-token").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "browser-ok").await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"origin-ok"}).to_string();
    let (status, response) = http_post_rpc_with_headers(
        addr,
        body.as_bytes(),
        Some("browser-ok"),
        &[("origin", "https://app.example")],
    )
    .await;
    assert_eq!(
        status,
        200,
        "token+origin http: {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));

    let mut ws =
        ws_connect_with_origin(addr, Some("browser-ok"), Some("https://app.example")).await;
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"ws-origin"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "ws-origin" {
                    assert!(value["success"].as_bool().unwrap_or(false));
                    ok = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "token+origin ws get_state missing");
    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn non_loopback_policy_requires_explicit_opt_in() {
    let app = faux_application("listen-remote-policy").await;
    let extension_ui = ExtensionUiAdapter::new();
    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("token-file");
    std::fs::write(&token_path, "fixture-value").expect("write token");

    let cases = [
        ("0.0.0.0:0", None, "IPv4 wildcard without token or opt-in"),
        ("0.0.0.0:0", Some(token_path.clone()), "IPv4 wildcard without opt-in"),
        ("[::]:0", None, "IPv6 wildcard without token or opt-in"),
        ("[::]:0", Some(token_path.clone()), "IPv6 wildcard without opt-in"),
        ("198.51.100.7:0", Some(token_path.clone()), "distinct non-loopback IPv4 without opt-in"),
        ("192.0.2.1:0", Some(token_path.clone()), "documentation IPv4 without opt-in"),
        ("8.8.8.8:0", Some(token_path.clone()), "public IPv4 without opt-in"),
        ("[2001:db8::1]:0", Some(token_path.clone()), "documentation IPv6 without opt-in"),
    ];
    for (address, token_file, label) in cases {
        let error = match start(
            app.application.clone(),
            extension_ui.clone(),
            ListenConfig {
                address: address.parse().unwrap(),
                token_file,
                allow_insecure_remote: false,
                advertised_origin: None,
                plaintext: true,
                tls_cert: None,
                tls_key: None,
                session_factory: None,
            },
        )
        .await
        {
            Ok(handle) => {
                handle.stop().await.expect("stop unexpected listener");
                panic!("{label} must be refused before bind");
            }
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("loopback"),
            "{label}: refusal must explain the loopback-only policy: {message}"
        );
    }
    app.application.cleanup().await;
}

#[tokio::test]
async fn tokenless_wildcard_opt_in_accepts_same_origin_browser_without_advertised_origin() {
    let app = faux_application("listen-wildcard-tokenless").await;
    let extension_ui = ExtensionUiAdapter::new();
    let handle = start(
        app.application.clone(),
        extension_ui,
        ListenConfig {
            address: "0.0.0.0:0".parse().unwrap(),
            token_file: None,
            allow_insecure_remote: true,
            advertised_origin: None,
            plaintext: true,
            tls_cert: None,
            tls_key: None,
            session_factory: None,
        },
    )
    .await
    .expect("tokenless wildcard opt-in must bind");
    let wildcard = handle.local_addr();
    assert!(wildcard.ip().is_unspecified(), "expected wildcard bind: {wildcard}");
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], wildcard.port()));
    let port = wildcard.port();

    // Native tokenless clients (no Origin) stay accepted.
    let (status, body) = http_post_rpc(
        loopback,
        br#"{"type":"get_state","id":"native-tokenless"}"#,
        None,
    )
    .await;
    assert_eq!(
        status,
        200,
        "native tokenless wildcard RPC failed: {}",
        String::from_utf8_lossy(&body)
    );
    let value: Value = serde_json::from_slice(&body).expect("parse rpc response");
    assert_eq!(value["id"], "native-tokenless");
    assert_eq!(value["success"], true);

    // A wildcard bind has no advertised origin, but the request's own Host
    // is the comparison target: a browser whose `http://` Origin authority
    // matches the Host it sent is accepted tokenless over RPC and WS — no
    // --listen-advertised-origin is needed for browser auth.
    let (status, body) = http_post_rpc_with_headers(
        loopback,
        br#"{"type":"get_state","id":"browser-tokenless"}"#,
        None,
        &[("origin", &format!("http://{loopback}"))],
    )
    .await;
    assert_eq!(
        status,
        200,
        "same-origin browser RPC failed: {}",
        String::from_utf8_lossy(&body)
    );
    let value: Value = serde_json::from_slice(&body).expect("parse rpc response");
    assert_eq!(value["id"], "browser-tokenless");
    assert_eq!(value["success"], true);

    let mut ws = try_ws_connect(loopback, None, Some(&format!("http://{loopback}")))
        .await
        .expect("same-origin browser ws must connect");
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"browser-ws-tokenless"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "browser-ws-tokenless" {
                    ok = value["success"].as_bool().unwrap_or(false);
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "same-origin browser ws must serve get_state");
    ws.close(None).await.ok();

    // Any LAN address or hostname the user's browser actually used works:
    // the connection arrives on loopback while Host/Origin model the LAN
    // page address, so arbitrary LAN IPs, hostnames, and the `localhost`
    // alias all pass the same Host-based check.
    let post = |host: &str, origin: &str, id: &str| {
        let body = format!(r#"{{"type":"get_state","id":"{id}"}}"#);
        format!(
            "POST /rpc HTTP/1.1\r\nhost: {host}\r\norigin: {origin}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    for (host, origin, id) in [
        (format!("192.168.1.50:{port}"), format!("http://192.168.1.50:{port}"), "browser-lan-ip"),
        (format!("mypi.lan:{port}"), format!("http://mypi.lan:{port}"), "browser-lan-hostname"),
        (format!("localhost:{port}"), format!("http://localhost:{port}"), "browser-localhost"),
    ] {
        let response = http_raw_exchange(loopback, post(&host, &origin, &id).as_bytes()).await;
        assert_eq!(
            parse_status(&response).unwrap_or(0),
            200,
            "{id}: same-origin browser on {host} must be accepted tokenless"
        );
    }

    // Ordinary request same-origin: an unrelated cross-origin page is
    // rejected over both channels even on the tokenless wildcard listener.
    let (status, _) = http_post_rpc_with_headers(
        loopback,
        br#"{"type":"get_state","id":"browser-mismatch"}"#,
        None,
        &[("origin", &format!("http://192.168.1.50:{port}"))],
    )
    .await;
    assert_eq!(status, 401, "mismatched browser origin must be 401");
    assert!(
        try_ws_connect(loopback, None, Some("https://evil.example"))
            .await
            .is_err(),
        "mismatched browser ws origin must fail"
    );
    // Duplicate Host headers are rejected like duplicate Origins.
    let body = r#"{"type":"get_state","id":"dup-host"}"#;
    let duplicate_host = format!(
        "POST /rpc HTTP/1.1\r\nhost: {loopback}\r\nhost: {loopback}\r\norigin: http://{loopback}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let response = http_raw_exchange(loopback, duplicate_host.as_bytes()).await;
    assert_eq!(parse_status(&response).unwrap_or(0), 401, "duplicate Host must be rejected");

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn wildcard_listener_with_opt_in_enforces_token_over_loopback_connection() {
    let app = faux_application("listen-wildcard-auth").await;
    let extension_ui = ExtensionUiAdapter::new();
    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("token-file");
    std::fs::write(&token_path, "fixture-value").expect("write token");
    let handle = start(
        app.application.clone(),
        extension_ui,
        ListenConfig {
            address: "0.0.0.0:0".parse().unwrap(),
            token_file: Some(token_path),
            allow_insecure_remote: true,
            advertised_origin: None,
            plaintext: true,
            tls_cert: None,
            tls_key: None,
            session_factory: None,
        },
    )
    .await
    .expect("authenticated wildcard opt-in must bind");
    let wildcard = handle.local_addr();
    assert!(wildcard.ip().is_unspecified(), "expected wildcard bind: {wildcard}");
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], wildcard.port()));

    let (missing_status, _) = http_post_rpc(loopback, br#"{"type":"get_state","id":"missing"}"#, None).await;
    assert_eq!(missing_status, 401, "wildcard listener accepted unauthenticated RPC");
    let (wrong_status, _) = http_post_rpc(
        loopback,
        br#"{"type":"get_state","id":"wrong"}"#,
        Some("wrong-value"),
    )
    .await;
    assert_eq!(wrong_status, 401, "wildcard listener accepted wrong token");
    let (ok_status, ok_body) = http_post_rpc(
        loopback,
        br#"{"type":"get_state","id":"authenticated"}"#,
        Some("fixture-value"),
    )
    .await;
    assert_eq!(ok_status, 200, "authenticated wildcard RPC failed");
    let response: Value = serde_json::from_slice(&ok_body).expect("parse authenticated response");
    assert_eq!(response["id"], "authenticated");
    assert_eq!(response["success"], true);

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn loopback_listen_accepts_v4_and_v6_with_and_without_token() {
    // Loopback behavior is unchanged: both v4 and v6 bind, tokenless and with
    // a token. This guards against the loopback-only tightening regressing
    // the legitimate local path.
    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("token");
    std::fs::write(&token_path, "fixture-value").expect("write token");
    for addr in ["127.0.0.1:0", "[::1]:0"] {
        for token_file in [None, Some(token_path.clone())] {
            let app = faux_application(&format!("listen-loopback-{addr}-{}", token_file.is_some())).await;
            let extension_ui = ExtensionUiAdapter::new();
            let handle = start(
                app.application.clone(),
                extension_ui,
                ListenConfig {
                    address: addr.parse().unwrap(),
                    token_file: token_file.as_deref().map(std::path::PathBuf::from),
                    allow_insecure_remote: false,
                    advertised_origin: None,
                    plaintext: true,
                    tls_cert: None,
                    tls_key: None,
                    session_factory: None,
                },
            )
            .await
            .expect("loopback must bind");
            let bound = handle.local_addr();
            assert!(bound.ip().is_loopback(), "{addr} bound non-loopback {bound}");
            handle.stop().await.expect("stop");
            app.application.cleanup().await;
        }
    }
}

#[tokio::test]
async fn stop_handle_closes_listener_and_cleanup_still_runs() {
    let app = faux_application("listen-stop").await;
    let process = app
        .application
        .process_spawn(spawn_sleep_spec(app.cwd.path(), 30))
        .await
        .expect("spawn process for cleanup observability");
    assert!(
        app.application
            .process_list()
            .iter()
            .any(|info| info.id == process.id)
    );

    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    handle.stop().await.expect("stop");
    let result = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "listener should be closed after stop"
    );

    app.application.cleanup().await;
    assert!(
        app.application.process_list().is_empty(),
        "cleanup must shut down owned processes"
    );
}

#[tokio::test]
async fn overload_produces_429_when_concurrency_exceeded() {
    let app = faux_application("listen-overload").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let spawn = json!({
        "type": "process_spawn",
        "id": "overload-process",
        "spec": {
            "argv": ["sleep", "30"],
            "cwd": app.cwd.path(),
            "env": {},
            "tty": false
        }
    })
    .to_string();
    let (spawn_status, spawn_response) = http_post_rpc(addr, spawn.as_bytes(), None).await;
    assert_eq!(
        spawn_status,
        200,
        "spawn response: {}",
        String::from_utf8_lossy(&spawn_response)
    );
    let process: pi_coding::ProcessInfo = serde_json::from_value(
        serde_json::from_slice::<Value>(&spawn_response).expect("parse spawn response")["data"]
            .clone(),
    )
    .expect("parse process info");
    let process_id = process.id.as_str().to_owned();

    let mut wait_streams = Vec::new();
    for index in 0..16 {
        let body = json!({
            "type": "process_wait",
            "id": format!("wait-{index}"),
            "processId": process_id,
            "timeoutMs": 30_000
        })
        .to_string();
        let stream = tokio::time::timeout(DEADLINE, async {
            let mut stream = TcpStream::connect(addr).await.expect("connect wait");
            let request = format!(
                "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(request.as_bytes())
                .await
                .expect("write wait request");
            stream
        })
        .await
        .expect("process_wait connect/write timed out");
        wait_streams.push(stream);
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut overload_response = None;
    for attempt in 0..32 {
        let overflow = json!({
            "type": "process_wait",
            "id": format!("wait-overflow-{attempt}"),
            "processId": process_id,
            "timeoutMs": 1
        })
        .to_string();
        let response = http_post_rpc(addr, overflow.as_bytes(), None).await;
        if response.0 == 429 {
            overload_response = Some(response);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (overflow_status, overflow_body) = overload_response.expect("expected overload rejection");
    assert_eq!(overflow_status, 429);
    let overflow_json: Value =
        serde_json::from_slice(&overflow_body).expect("parse overload body");
    assert!(
        overflow_json["error"]
            .as_str()
            .is_some_and(|error| error.contains("too many concurrent RPC commands")),
        "overload body: {overflow_json}"
    );

    // Recovery must go through the public HTTP boundary, not Application APIs.
    // process_stop is runs_inline and must bypass the saturated work slots.
    let stop = json!({
        "type": "process_stop",
        "id": "recover",
        "processId": process_id
    })
    .to_string();
    let (stop_status, stop_body) = http_post_rpc(addr, stop.as_bytes(), None).await;
    assert_ne!(
        stop_status, 429,
        "process_stop must bypass saturation: {}",
        String::from_utf8_lossy(&stop_body)
    );
    assert_eq!(
        stop_status, 200,
        "process_stop over HTTP while saturated: {}",
        String::from_utf8_lossy(&stop_body)
    );
    let stop_json: Value = serde_json::from_slice(&stop_body).expect("parse stop");
    assert!(
        stop_json["success"].as_bool().unwrap_or(false),
        "process_stop body: {stop_json}"
    );

    // Waiters must settle after the HTTP stop (no hang / deadlock).
    for mut stream in wait_streams {
        let mut response = Vec::new();
        let read = tokio::time::timeout(DEADLINE, stream.read_to_end(&mut response)).await;
        assert!(
            read.is_ok(),
            "saturated process_wait must drain after HTTP process_stop"
        );
    }
    handle.stop().await.expect("stop listener");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_shares_live_application_identity() {
    let app = faux_application("listen-shared-app").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();

    let before = app.application.state().await;
    let body = json!({"type":"get_state","id":"shared-1"}).to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));
    if let Some(session_id) = before.session_id.as_deref() {
        assert_eq!(
            value["data"]["sessionId"].as_str(),
            Some(session_id),
            "listener must expose the same session id"
        );
    }

    let set_todos = json!({
        "type": "set_todos",
        "id": "shared-todos",
        "phases": [{"name":"from-rpc","tasks":[]}]
    })
    .to_string();
    let (status, response) = http_post_rpc(addr, set_todos.as_bytes(), None).await;
    assert_eq!(
        status,
        200,
        "set_todos: {}",
        String::from_utf8_lossy(&response)
    );
    let todos = app.application.session().todo_state();
    assert!(
        todos.phases.iter().any(|phase| phase.name == "from-rpc"),
        "RPC set_todos must mutate the shared Application todos: {todos:?}"
    );

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_preserves_canonical_tui_extension_queries() {
    let app = faux_application("listen-canonical").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    extension_ui.set_host_editor_text("canonical-editor-buffer");
    extension_ui.set_host_tools_expanded(true);
    extension_ui.set_themes(vec![ExtensionThemeDescriptor {
        name: "listen-theme".into(),
        path: None,
    }]);
    extension_ui.set_active_theme(Some("listen-theme".into()));

    let context = ExtensionUiContext {
        instance: ExtensionInstanceId {
            extension_id: "canonical-owner".into(),
            generation: 1,
        },
        mode: ExtensionMode::Tui,
    };
    let editor = extension_ui
        .request(
            context.clone(),
            ExtensionUiRequest::GetEditorText,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetEditorText");
    assert!(
        matches!(
            editor,
            ExtensionUiResponse::EditorText { ref value } if value == "canonical-editor-buffer"
        ),
        "editor: {editor:?}"
    );
    let themes = extension_ui
        .request(
            context.clone(),
            ExtensionUiRequest::GetAllThemes,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetAllThemes");
    match themes {
        ExtensionUiResponse::Themes { themes } => {
            assert!(
                themes.iter().any(|theme| theme.name == "listen-theme"),
                "themes: {themes:?}"
            );
        }
        other => panic!("expected Themes response, got {other:?}"),
    }
    let expanded = extension_ui
        .request(
            context,
            ExtensionUiRequest::GetToolsExpanded,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetToolsExpanded");
    assert!(
        matches!(
            expanded,
            ExtensionUiResponse::ToolsExpanded { expanded: true }
        ),
        "expanded: {expanded:?}"
    );

    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"canonical-rpc"}).to_string();
    assert_eq!(http_post_rpc(addr, body.as_bytes(), None).await.0, 200);

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn tui_interaction_ids_are_not_ws_respondable() {
    let app = faux_application("listen-tui-interaction").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    // The listen server projects EXTENSION-owned interactive requests as
    // read-only notices, but HOST/TUI-owned interactions stay private to the
    // live terminal (instance id "host"), which is the exclusive owner
    // required to keep those requests pending.
    let _tui_events = extension_ui.subscribe();
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    let _ = tokio::time::timeout(Duration::from_millis(50), ws.next()).await;

    let adapter = extension_ui.clone();
    let pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "host".into(),
                        generation: 0,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Approve?".into(),
                    message: "tui-only".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let interaction_id = loop {
        if let Some(interaction) = extension_ui.pending_interactions().first() {
            break interaction.id.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("TUI interaction never became pending");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let scan_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < scan_deadline {
        match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap_or(json!({}));
                if value["type"] == "extension_ui_request" {
                    assert_ne!(
                        value["id"].as_str(),
                        Some(interaction_id.as_str()),
                        "WS must not receive TUI InteractionRequested id"
                    );
                    assert_ne!(
                        value["method"], "confirm",
                        "WS received confirm interaction: {value}"
                    );
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }

    ws.send(WsMessage::Text(
        json!({
            "type": "extension_ui_response",
            "id": interaction_id,
            "confirmed": true
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send remote response");

    let mut saw_failure = false;
    let fail_deadline = tokio::time::Instant::now() + DEADLINE;
    while tokio::time::Instant::now() < fail_deadline {
        match tokio::time::timeout_at(fail_deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["type"] == "response"
                    && value["command"] == "extension_ui_response"
                    && value["success"] == false
                {
                    assert_eq!(
                        value["error"].as_str(),
                        Some(REMOTE_UI_DISABLED),
                        "exact disable error required, got {value}"
                    );
                    saw_failure = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(
        saw_failure,
        "WS must emit explicit extension_ui_response failure"
    );
    assert!(
        extension_ui
            .pending_interactions()
            .iter()
            .any(|interaction| interaction.id == interaction_id),
        "remote WS response must not consume TUI interaction"
    );

    extension_ui
        .respond_confirmed(&interaction_id, false)
        .expect("local TUI deny");
    let decision = tokio::time::timeout(DEADLINE, pending)
        .await
        .expect("pending join")
        .expect("pending task")
        .expect("pending result");
    assert!(matches!(
        decision,
        ExtensionUiResponse::Confirmed { confirmed: false }
    ));

    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn multiple_ws_clients_cannot_resolve_tui_interaction() {
    let app = faux_application("listen-multi-ws").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    // The listen server projects EXTENSION-owned interactive requests as
    // read-only notices, but HOST/TUI-owned interactions stay private to the
    // live terminal (instance id "host"), which is the exclusive owner
    // required to keep those requests pending.
    let _tui_events = extension_ui.subscribe();
    let addr = handle.local_addr();
    let mut ws_a = ws_connect(addr, None).await;
    let mut ws_b = ws_connect(addr, None).await;

    let adapter = extension_ui.clone();
    let pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "host".into(),
                        generation: 0,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Race?".into(),
                    message: "only tui".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let interaction_id = loop {
        if let Some(interaction) = extension_ui.pending_interactions().first() {
            break interaction.id.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("interaction missing");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    for ws in [&mut ws_a, &mut ws_b] {
        ws.send(WsMessage::Text(
            json!({
                "type": "extension_ui_response",
                "id": interaction_id,
                "confirmed": true
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send race response");
    }

    // Both clients should get the same hard failure; neither consumes the pending id.
    for (label, ws) in [("a", &mut ws_a), ("b", &mut ws_b)] {
        let mut saw_failure = false;
        let fail_deadline = tokio::time::Instant::now() + DEADLINE;
        while tokio::time::Instant::now() < fail_deadline {
            match tokio::time::timeout_at(fail_deadline, ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse");
                    if value["type"] == "response"
                        && value["command"] == "extension_ui_response"
                        && value["success"] == false
                    {
                        assert_eq!(
                            value["error"].as_str(),
                            Some(REMOTE_UI_DISABLED),
                            "{label} failure text"
                        );
                        saw_failure = true;
                        break;
                    }
                }
                Ok(Some(Ok(_))) => continue,
                _ => break,
            }
        }
        assert!(saw_failure, "{label} must get remote UI disabled failure");
    }

    assert!(
        extension_ui
            .pending_interactions()
            .iter()
            .any(|interaction| interaction.id == interaction_id),
        "neither WS client may consume the TUI interaction"
    );

    extension_ui
        .respond_confirmed(&interaction_id, true)
        .expect("tui owner allow");
    let decision = tokio::time::timeout(DEADLINE, pending)
        .await
        .expect("join")
        .expect("task")
        .expect("result");
    assert!(matches!(
        decision,
        ExtensionUiResponse::Confirmed { confirmed: true }
    ));

    ws_a.close(None).await.ok();
    ws_b.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// Pre-auth slow-header connections are hard-capped at [`MAX_CONNECTION_TASKS`].
/// Connection N+1 must be dropped promptly (never hang for the 10s header timeout).
#[tokio::test]
async fn preauth_slow_header_connections_are_capped() {
    let app = faux_application("listen-preauth-cap").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    assert_eq!(MAX_CONNECTION_TASKS, 64);

    let mut slow = Vec::with_capacity(MAX_CONNECTION_TASKS);
    for _ in 0..MAX_CONNECTION_TASKS {
        let stream = tokio::time::timeout(DEADLINE, async {
            let mut stream = TcpStream::connect(addr).await.expect("slow connect");
            stream
                .write_all(b"POST /rpc HTTP/1.1\r\nhost: x\r\n")
                .await
                .expect("partial headers");
            stream
        })
        .await
        .expect("slow-header connect/write timed out");
        slow.push(stream);
    }

    // Let the accept loop observe the saturated JoinSet.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connection 65: production accepts then immediately drops with no header task.
    // A full RPC write+read must fail promptly; a 2s hang is a contract failure.
    let overflow = tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = TcpStream::connect(addr).await.expect("overflow connect");
        let body = br#"{"type":"get_state","id":"cap-overflow"}"#;
        let request = format!(
            "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(request.as_bytes()).await.is_err() {
            return None;
        }
        if stream.write_all(body).await.is_err() {
            return None;
        }
        let mut response = Vec::new();
        match stream.read_to_end(&mut response).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(response),
        }
    })
    .await
    .expect("over-cap path must settle within 2s; timeout means the cap is not enforced");

    if let Some(response) = overflow {
        if let Some(status) = parse_status(&response) {
            assert_ne!(status, 200, "over-cap must not serve successful RPC");
        }
        if let Ok(value) =
            serde_json::from_slice::<Value>(&parse_body(&response).unwrap_or_default())
        {
            assert_ne!(value["success"], true, "over-cap body: {value}");
        }
    }

    // Release capacity and prove a normal request recovers.
    drop(slow.pop());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let body = json!({"type":"get_state","id":"after-cap"}).to_string();
    let mut recovered = None;
    for _ in 0..32 {
        let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
        if status == 200 {
            recovered = Some(response);
            break;
        }
        let _ = slow.pop();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let response = recovered.expect("normal request should succeed after releasing capacity");
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));

    drop(slow);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_stop_error_path_still_allows_application_cleanup() {
    let app = faux_application("listen-stop-error").await;
    let process = app
        .application
        .process_spawn(spawn_sleep_spec(app.cwd.path(), 60))
        .await
        .expect("spawn");
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let stop_result = handle.stop().await;
    if let Err(error) = &stop_result {
        assert!(!format!("{error:#}").is_empty());
    }
    // Cleanup always runs after stop returns (Ok or Err), matching main_run.
    app.application.cleanup().await;
    assert!(
        app.application
            .process_list()
            .iter()
            .all(|info| info.id != process.id)
            || app.application.process_list().is_empty(),
        "cleanup must reclaim processes after listener stop"
    );
}

#[tokio::test]
async fn http_rejects_extension_ui_response_command() {
    let app = faux_application("listen-http-ui-response").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({
        "type": "extension_ui_response",
        "id": "nope",
        "confirmed": true
    })
    .to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert!(
        status == 400 || status == 200,
        "status {status} {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert_eq!(value["success"], false);
    assert_eq!(
        value["error"].as_str(),
        Some(REMOTE_UI_DISABLED),
        "http body: {value}"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn ws_projects_ui_events_but_isolates_host_interactions() {
    let app = faux_application("listen-ui-notify").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;

    // Non-interactive UI state still projects (notify).
    extension_ui
        .request(
            ExtensionUiContext {
                instance: ExtensionInstanceId {
                    extension_id: "notify-owner".into(),
                    generation: 1,
                },
                mode: ExtensionMode::Tui,
            },
            ExtensionUiRequest::Notify {
                message: "hello-from-host".into(),
                level: pi_coding::UiNotificationLevel::Info,
            },
            ExtensionCancellation::new(),
        )
        .await
        .expect("notify");

    // An EXTENSION-owned interactive confirm projects as a read-only notice
    // (the D94 web approval card), carrying the extension identity.
    let adapter = extension_ui.clone();
    let ext_pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "ext-owner".into(),
                        generation: 2,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Approve?".into(),
                    message: "extension ask".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    // A HOST/TUI-owned interactive confirm stays private to the terminal.
    let adapter = extension_ui.clone();
    let host_pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "host".into(),
                        generation: 0,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Host?".into(),
                    message: "tui only".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    // Collect both interaction ids (extension-owned + host-owned).
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ext_id = None;
    let mut host_id = None;
    while ext_id.is_none() || host_id.is_none() {
        for interaction in extension_ui.pending_interactions() {
            match interaction.context.instance.extension_id.as_str() {
                "ext-owner" => ext_id = Some(interaction.id.clone()),
                "host" => host_id = Some(interaction.id.clone()),
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("interactions never became pending");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ext_id = ext_id.expect("extension interaction id");
    let host_id = host_id.expect("host interaction id");

    // The WS must see the extension-owned confirm (method + extensionId) and
    // the notify; it must NOT see the host-owned confirm.
    let mut saw_notify = false;
    let mut saw_ext_confirm = false;
    let mut saw_host_confirm = false;
    let scan_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < scan_deadline {
        match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap_or(json!({}));
                if value["type"] == "extension_ui_request" {
                    match value["method"].as_str() {
                        Some("notify") => {
                            assert_eq!(value["message"], "hello-from-host");
                            saw_notify = true;
                        }
                        Some("confirm") => match value["extensionId"].as_str() {
                            Some("ext-owner") => {
                                assert_ne!(
                                    value["id"].as_str(),
                                    Some(host_id.as_str()),
                                    "extension confirm must not carry the host interaction id"
                                );
                                saw_ext_confirm = true;
                            }
                            Some("host") => {
                                saw_host_confirm = true;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(saw_notify, "non-interactive notify should project over WS");
    assert!(
        saw_ext_confirm,
        "extension-owned confirm must project over WS as a read-only notice"
    );
    assert!(
        !saw_host_confirm,
        "host/TUI-owned confirm must stay private to the terminal"
    );

    // The terminal answers both locally; remote answers remain rejected (the
    // dedicated tests cover the rejection paths).
    extension_ui
        .respond_confirmed(&ext_id, true)
        .expect("extension answer");
    extension_ui
        .respond_confirmed(&host_id, false)
        .expect("host answer");
    let ext_decision = tokio::time::timeout(DEADLINE, ext_pending)
        .await
        .expect("ext join")
        .expect("ext task")
        .expect("ext result");
    let host_decision = tokio::time::timeout(DEADLINE, host_pending)
        .await
        .expect("host join")
        .expect("host task")
        .expect("host result");
    assert!(matches!(
        ext_decision,
        ExtensionUiResponse::Confirmed { confirmed: true }
    ));
    assert!(matches!(
        host_decision,
        ExtensionUiResponse::Confirmed { confirmed: false }
    ));

    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Cap retained child diagnostics while still draining pipes to EOF.
const PIPE_DIAG_CAP: usize = 64 * 1024;

/// Drain a pipe to EOF on a background thread, retaining at most [`PIPE_DIAG_CAP`] bytes.
fn spawn_pipe_drain(
    pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut retained = Vec::new();
        let mut reader = pipe;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match std::io::Read::read(&mut reader, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(PIPE_DIAG_CAP.saturating_sub(retained.len()));
                    if take > 0 {
                        retained.extend_from_slice(&chunk[..take]);
                    }
                    // Continue reading past the cap so the pipe drains to EOF.
                }
                Err(_) => break,
            }
        }
        retained
    })
}

fn join_pipe_text(handle: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&handle.join().unwrap_or_default()).into_owned()
}

/// Kill (best-effort), wait, join both readers, then panic with captured streams.
fn kill_wait_join_panic(
    mut child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    message: String,
) -> ! {
    let _ = child.kill();
    let _ = child.wait();
    let stdout = join_pipe_text(stdout);
    let stderr = join_pipe_text(stderr);
    panic!("{message}\nstdout={stdout}\nstderr={stderr}");
}

/// Bounded child run: concurrent stdout/stderr drains, poll exit, kill+wait on
/// deadline, then join readers (never join while the child may hold pipe ends).
fn finish_child_bounded(
    mut child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    deadline: std::time::Duration,
    label: &str,
) -> (i32, String, String) {
    use std::time::{Duration, Instant};

    let end = Instant::now() + deadline;
    let mut status = None;
    while Instant::now() < end {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                kill_wait_join_panic(
                    child,
                    stdout,
                    stderr,
                    format!("try_wait {label}: {error}"),
                );
            }
        }
    }
    if status.is_none() {
        kill_wait_join_panic(
            child,
            stdout,
            stderr,
            format!("{label} exceeded {deadline:?}; killed child"),
        );
    }
    // Exit observed via try_wait; Drop reaps without blocking forever.
    drop(child);

    let stdout = join_pipe_text(stdout);
    let stderr = join_pipe_text(stderr);
    (
        status.expect("child exit status").code().unwrap_or(1),
        stdout,
        stderr,
    )
}


fn run_rpi_binary(args: &[&str]) -> (i32, String, String) {
    use std::process::{Command, Stdio};

    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let mut child = Command::new(rpi_bin())
        .args(args)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join(".pi"))
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_OFFLINE", "1")
        .env("PI_FAUX_RESPONSE", "listen-cli-should-not-run")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi binary");
    let stdout = spawn_pipe_drain(child.stdout.take().expect("stdout pipe"));
    let stderr = spawn_pipe_drain(child.stderr.take().expect("stderr pipe"));
    finish_child_bounded(child, stdout, stderr, DEADLINE, "rpi binary")
}

/// Public binary: `--listen` + `--list-models` exits nonzero promptly.
#[test]
fn binary_listen_rejects_list_models_combination() {
    let (code, stdout, stderr) = run_rpi_binary(&[
        "--listen",
        "127.0.0.1:0",
        "--list-models",
        "--model",
        "faux/faux-1",
    ]);
    assert_ne!(code, 0, "stdout={stdout} stderr={stderr}");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("--listen") && combined.contains("--list-models"),
        "error must mention both flags: {combined}"
    );
    assert!(
        !combined.contains("listen-cli-should-not-run"),
        "must not emit faux prompt/model output: {combined}"
    );
    assert!(
        !combined.contains("Control plane listening"),
        "must not start the listener: {combined}"
    );
}
/// Public binary: `--listen` + `--export` rejects two competing top-level modes.
#[test]
fn binary_listen_rejects_export_combination() {
    let (code, stdout, stderr) = run_rpi_binary(&[
        "--listen",
        "127.0.0.1:0",
        "--export",
        "missing-session.jsonl",
    ]);
    assert_ne!(code, 0, "stdout={stdout} stderr={stderr}");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("--listen") && combined.contains("--export"),
        "error must mention both flags: {combined}"
    );
    assert!(
        !combined.contains("loading session"),
        "must reject before running export: {combined}"
    );
    assert!(
        !combined.contains("Control plane listening"),
        "must not start the listener: {combined}"
    );
}

/// Public binary: `--listen` rejects positional prompts instead of starting a
/// second terminal input path beside the Web service.
#[test]
fn binary_listen_rejects_positional_prompt() {
    let (code, stdout, stderr) = run_rpi_binary(&[
        "--listen",
        "127.0.0.1:0",
        "--model",
        "faux/faux-1",
        "prompt belongs on Web RPC",
    ]);
    assert_ne!(code, 0, "stdout={stdout} stderr={stderr}");
    let combined = format!("{stdout}\n{stderr}");
    assert!(combined.contains("Web-only"), "error must explain listener mode: {combined}");
    assert!(combined.contains("/web") && combined.contains("/rpc"), "error must name prompt surfaces: {combined}");
    assert!(!combined.contains("Control plane listening"), "invalid combination must not bind: {combined}");
}

#[cfg(unix)]
fn signal_and_collect_listener(
    mut child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
) -> (i32, String, String) {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("signal listener");
    finish_child_bounded(child, stdout, stderr, Duration::from_secs(20), "signaled listener")
}

fn http_rpc_binary(address: std::net::SocketAddr, command: &Value) -> Value {
    use std::io::{Read as _, Write as _};

    let body = serde_json::to_vec(command).expect("encode RPC command");
    let request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("connect binary listener RPC");
    stream.set_read_timeout(Some(Duration::from_secs(30))).expect("RPC read timeout");
    stream.write_all(request.as_bytes()).expect("write RPC headers");
    stream.write_all(&body).expect("write RPC body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read RPC response");
    let body = parse_body(&response).expect("parse RPC response body");
    serde_json::from_slice(&body).expect("parse RPC JSON")
}

#[cfg(unix)]
#[test]
fn binary_web_prompt_persists_and_restores_after_listener_restart() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Instant;

    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let port_probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe port");
    let address = port_probe.local_addr().expect("probe address");
    drop(port_probe);

    let spawn = |resume: bool| {
        let mut command = Command::new(rpi_bin());
        command
            .args([
                "--listen",
                &address.to_string(),
                // TLS is the default transport; this test drives the binary
                // over plain HTTP (http_rpc_binary), so opt out explicitly.
                "--listen-plaintext",
                "--model",
                "faux/faux-1",
                "--api-key",
                "faux",
                "--session-dir",
                sessions.path().to_str().expect("session path UTF-8"),
            ])
            .current_dir(cwd.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("PI_CODING_AGENT_DIR", home.path().join(".pi"))
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_OFFLINE", "1")
            .env("PI_FAUX_RESPONSE", "persisted Web assistant reply")
            .env_remove("PI_PROFILE")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if resume {
            command.arg("--continue");
        }
        let mut child = command.spawn().expect("spawn listener");
        let stdout = spawn_pipe_drain(child.stdout.take().expect("stdout pipe"));
        let stderr = spawn_pipe_drain(child.stderr.take().expect("stderr pipe"));
        (child, stdout, stderr)
    };

    let wait_until_ready = |child: &mut std::process::Child| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(value) = std::panic::catch_unwind(|| {
                http_rpc_binary(address, &json!({"type":"get_state","id":"ready"}))
            }) {
                if value["success"] == true {
                    break;
                }
            }
            assert!(child.try_wait().expect("poll listener").is_none(), "listener exited before ready");
            assert!(Instant::now() < deadline, "listener did not become ready");
            thread::sleep(Duration::from_millis(50));
        }
    };

    let (mut first, first_stdout, first_stderr) = spawn(false);
    wait_until_ready(&mut first);
    let prompt_text = "persist this Web-only conversation";
    let prompted = http_rpc_binary(
        address,
        &json!({"type":"prompt","id":"persist-prompt","message":prompt_text}),
    );
    assert_eq!(prompted["success"], true, "prompt response: {prompted}");
    let entries_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let entries = http_rpc_binary(address, &json!({"type":"get_entries","id":"persist-entries"}));
        let serialized = entries.to_string();
        if serialized.contains(prompt_text) && serialized.contains("persisted Web assistant reply") {
            break;
        }
        assert!(Instant::now() < entries_deadline, "Web prompt did not reach recorded entries: {entries}");
        thread::sleep(Duration::from_millis(50));
    }
    let (first_code, first_out, first_err) = signal_and_collect_listener(first, first_stdout, first_stderr);
    assert_eq!(first_code, 0, "first shutdown stdout={first_out} stderr={first_err}");

    let (mut second, second_stdout, second_stderr) = spawn(true);
    wait_until_ready(&mut second);
    let restored = http_rpc_binary(address, &json!({"type":"get_entries","id":"restored-entries"}));
    let serialized = restored.to_string();
    assert!(serialized.contains(prompt_text), "restored entries missing user prompt: {restored}");
    assert!(serialized.contains("persisted Web assistant reply"), "restored entries missing assistant reply: {restored}");
    let (second_code, second_out, second_err) =
        signal_and_collect_listener(second, second_stdout, second_stderr);
    assert_eq!(second_code, 0, "second shutdown stdout={second_out} stderr={second_err}");
}

#[tokio::test]
async fn ws_evicts_slow_reader_without_blocking_fresh_clients() {
    let app = faux_application("listen-ws-slow-reader").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let socket = tokio::net::TcpSocket::new_v4().expect("create slow-reader TCP socket");
    socket
        .set_recv_buffer_size(1024)
        .expect("shrink slow-reader TCP receive buffer");
    let stream = tokio::time::timeout(DEADLINE, socket.connect(addr))
        .await
        .expect("slow-reader TCP connect timed out")
        .expect("connect slow-reader TCP socket");
    let request = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("build slow-reader WS request");
    let slow = tokio::time::timeout(DEADLINE, tokio_tungstenite::client_async(request, stream))
        .await
        .expect("slow-reader WS handshake timed out")
        .expect("complete slow-reader WS handshake")
        .0;
    let mut slow = slow.into_inner();

    let payload = "x".repeat(128 * 1024);
    tokio::time::timeout(DEADLINE, async {
        for _ in 0..512 {
            extension_ui
                .request(
                    ExtensionUiContext {
                        instance: ExtensionInstanceId {
                            extension_id: "slow-reader-flood".into(),
                            generation: 1,
                        },
                        mode: ExtensionMode::Tui,
                    },
                    ExtensionUiRequest::Status {
                        key: "bounded-flood".into(),
                        text: Some(payload.clone()),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .expect("publish public status event");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded public event flood timed out");

    tokio::time::timeout(DEADLINE, async {
        let mut buffer = [0_u8; 8192];
        loop {
            match slow.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("slow WebSocket TCP socket was not closed within the deadline");

    let mut fresh = ws_connect(addr, None).await;
    fresh
        .send(WsMessage::Text(
            json!({"type":"get_state","id":"after-slow-reader"})
                .to_string()
                .into(),
        ))
        .await
        .expect("send get_state after slow-reader eviction");
    let response = tokio::time::timeout(DEADLINE, async {
        loop {
            match fresh.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse get_state response");
                    if value["type"] == "response" && value["id"] == "after-slow-reader" {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("fresh WebSocket failed: {error}"),
                None => panic!("fresh WebSocket closed before get_state response"),
            }
        }
    })
    .await
    .expect("fresh WebSocket get_state timed out");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);

    fresh.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// Long-running commands (process_wait) run off the WebSocket read/event
/// select: application events keep streaming while one is pending, and its
/// response arrives later, correlated by id (never serialized in front of the
/// events).
#[tokio::test]
async fn ws_forwards_events_while_long_command_runs() {
    let app = faux_application("listen-ws-nonblocking").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;

    // process_wait is non-inline: it blocks until the process exits. Spawn a
    // long-lived process and wait on it for far longer than the test deadline.
    let spawn = json!({
        "type": "process_spawn",
        "id": "nb-spawn",
        "spec": spawn_sleep_spec(app.cwd.path(), 30)
    })
    .to_string();
    ws.send(WsMessage::Text(spawn.into()))
        .await
        .expect("send process_spawn");
    let process_id = tokio::time::timeout(DEADLINE, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse spawn response");
                    if value["type"] == "response" && value["id"] == "nb-spawn" {
                        assert!(
                            value["success"].as_bool().unwrap_or(false),
                            "spawn failed: {value}"
                        );
                        return value["data"]["id"]
                            .as_str()
                            .expect("spawn response process id")
                            .to_owned();
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed: {error}"),
                None => panic!("WebSocket closed before process_spawn response"),
            }
        }
    })
    .await
    .expect("process_spawn response timed out");

    ws.send(WsMessage::Text(
        json!({
            "type": "process_wait",
            "id": "nb-wait",
            "processId": process_id,
            "timeoutMs": 60_000
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send process_wait");
    // Let the server pick up the wait before the event fires.
    tokio::time::sleep(Duration::from_millis(200)).await;

    app.application
        .set_todos(vec![TodoPhase {
            name: "during-long-command".into(),
            tasks: vec![],
        }])
        .expect("set todos publishes TodoUpdated");

    // The event must arrive BEFORE the process_wait response: the wait is
    // still pending (the process runs for 30s), so its response cannot be
    // produced until process_stop below.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut saw_event_first = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "todo_updated" {
                    assert_eq!(value["phases"][0]["name"], "during-long-command");
                    saw_event_first = true;
                    break;
                }
                if value["type"] == "response" && value["id"] == "nb-wait" {
                    panic!("process_wait response arrived before the application event");
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_event_first,
        "application event did not arrive while the long command ran"
    );

    // process_stop is inline and must work while the wait is still pending.
    ws.send(WsMessage::Text(
        json!({
            "type": "process_stop",
            "id": "nb-stop",
            "processId": process_id
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send process_stop");

    // The wait response arrives after the stop, still correlated by id.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut saw_wait = false;
    let mut saw_stop = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "response" && value["id"] == "nb-wait" {
                    assert_eq!(value["command"], "process_wait");
                    assert!(
                        value["success"].as_bool().unwrap_or(false),
                        "wait failed: {value}"
                    );
                    saw_wait = true;
                } else if value["type"] == "response" && value["id"] == "nb-stop" {
                    assert_eq!(value["command"], "process_stop");
                    assert!(
                        value["success"].as_bool().unwrap_or(false),
                        "stop failed: {value}"
                    );
                    saw_stop = true;
                }
                if saw_wait && saw_stop {
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(saw_wait, "process_wait response never arrived after process_stop");
    assert!(saw_stop, "process_stop response never arrived");

    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// A WebSocket connection's pending command set is bounded: the next
/// non-inline command is rejected immediately with the same "too many
/// concurrent RPC commands" response as the stdio/HTTP paths (id preserved
/// for client correlation), and every pending command still completes once
/// the load drains.
#[tokio::test]
async fn ws_rejects_commands_beyond_concurrency_limit() {
    let app = faux_application("listen-ws-saturation").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;

    // Matches super::rpc::MAX_CONCURRENT_COMMANDS, kept literal here like the
    // existing HTTP overload test.
    const MAX_CONCURRENT_COMMANDS: usize = 16;

    let spawn = json!({
        "type": "process_spawn",
        "id": "sat-spawn",
        "spec": spawn_sleep_spec(app.cwd.path(), 30)
    })
    .to_string();
    ws.send(WsMessage::Text(spawn.into()))
        .await
        .expect("send process_spawn");
    let process_id = tokio::time::timeout(DEADLINE, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse spawn response");
                    if value["type"] == "response" && value["id"] == "sat-spawn" {
                        return value["data"]["id"]
                            .as_str()
                            .expect("spawn response process id")
                            .to_owned();
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed: {error}"),
                None => panic!("WebSocket closed before process_spawn response"),
            }
        }
    })
    .await
    .expect("process_spawn response timed out");

    // Saturate the per-connection command set with long process_waits.
    for index in 0..MAX_CONCURRENT_COMMANDS {
        ws.send(WsMessage::Text(
            json!({
                "type": "process_wait",
                "id": format!("sat-wait-{index}"),
                "processId": process_id,
                "timeoutMs": 60_000
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send saturated process_wait");
    }
    // The next non-inline command must be rejected immediately, before any
    // pending wait can complete (the process still runs for 30s).
    ws.send(WsMessage::Text(
        json!({
            "type": "process_wait",
            "id": "sat-overflow",
            "processId": process_id,
            "timeoutMs": 60_000
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send overflow process_wait");

    let overflow = tokio::time::timeout(DEADLINE, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                    if value["type"] == "response" && value["id"] == "sat-overflow" {
                        return value;
                    }
                    if value["type"] == "response"
                        && value["id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("sat-wait-"))
                    {
                        panic!("a pending wait completed before the overflow rejection");
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed: {error}"),
                None => panic!("WebSocket closed before overflow rejection"),
            }
        }
    })
    .await
    .expect("overflow rejection timed out");
    assert_eq!(overflow["command"], "process_wait");
    assert_eq!(overflow["success"], false);
    assert!(
        overflow["error"].as_str().is_some_and(|error| error
            .contains("too many concurrent RPC commands (limit")),
        "overflow body: {overflow}"
    );

    // process_stop is inline and must bypass the saturated set.
    ws.send(WsMessage::Text(
        json!({
            "type": "process_stop",
            "id": "sat-stop",
            "processId": process_id
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send process_stop");

    // All pending waits resolve with their own ids once the process exits.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut pending: std::collections::BTreeSet<String> = (0..MAX_CONCURRENT_COMMANDS)
        .map(|index| format!("sat-wait-{index}"))
        .collect();
    let mut saw_stop = false;
    while tokio::time::Instant::now() < deadline && (!pending.is_empty() || !saw_stop) {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "response" {
                    if let Some(id) = value["id"].as_str() {
                        if pending.remove(id) {
                            assert_eq!(value["command"], "process_wait");
                            assert!(
                                value["success"].as_bool().unwrap_or(false),
                                "wait failed: {value}"
                            );
                            continue;
                        }
                        if id == "sat-stop" {
                            assert_eq!(value["command"], "process_stop");
                            assert!(
                                value["success"].as_bool().unwrap_or(false),
                                "stop failed: {value}"
                            );
                            saw_stop = true;
                        }
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(saw_stop, "process_stop response never arrived");
    assert!(
        pending.is_empty(),
        "pending waits never completed: {pending:?}"
    );

    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// Disconnecting mid-command must not strand the pending task or the
/// connection: the server answers the close handshake promptly (the read loop
/// is not blocked in the long command), aborts the pending wait without
/// killing the process, and stays healthy for fresh clients.
#[tokio::test]
async fn ws_disconnect_aborts_pending_command_tasks() {
    let app = faux_application("listen-ws-disconnect").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;

    let spawn = json!({
        "type": "process_spawn",
        "id": "disc-spawn",
        "spec": spawn_sleep_spec(app.cwd.path(), 30)
    })
    .to_string();
    ws.send(WsMessage::Text(spawn.into()))
        .await
        .expect("send process_spawn");
    let process_id = tokio::time::timeout(DEADLINE, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse spawn response");
                    if value["type"] == "response" && value["id"] == "disc-spawn" {
                        return value["data"]["id"]
                            .as_str()
                            .expect("spawn response process id")
                            .to_owned();
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed: {error}"),
                None => panic!("WebSocket closed before process_spawn response"),
            }
        }
    })
    .await
    .expect("process_spawn response timed out");

    ws.send(WsMessage::Text(
        json!({
            "type": "process_wait",
            "id": "disc-wait",
            "processId": process_id,
            "timeoutMs": 60_000
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send process_wait");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The close handshake must complete promptly. Before this fix the read
    // loop was blocked inside process_wait and would not answer the close for
    // the full 30s the process runs.
    tokio::time::timeout(Duration::from_secs(5), ws.close(None))
        .await
        .expect("server did not answer the close handshake while a command was pending")
        .ok();

    // The server aborted the pending wait (not the process) and remains
    // healthy for fresh clients.
    let mut fresh = ws_connect(addr, None).await;
    fresh.send(WsMessage::Text(
        json!({"type":"get_state","id":"after-disconnect"})
            .to_string()
            .into(),
    ))
    .await
    .expect("send get_state after disconnect");
    let response = tokio::time::timeout(DEADLINE, async {
        loop {
            match fresh.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse get_state response");
                    if value["type"] == "response" && value["id"] == "after-disconnect" {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("fresh WebSocket failed: {error}"),
                None => panic!("fresh WebSocket closed before get_state response"),
            }
        }
    })
    .await
    .expect("fresh WebSocket get_state timed out");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);

    // The aborted wait must not have killed the process: a short wait on the
    // fresh connection times out with the process still running.
    fresh.send(WsMessage::Text(
        json!({
            "type": "process_wait",
            "id": "disc-wait-2",
            "processId": process_id,
            "timeoutMs": 1
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send short process_wait");
    let short_wait = tokio::time::timeout(DEADLINE, async {
        loop {
            match fresh.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                    if value["type"] == "response" && value["id"] == "disc-wait-2" {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("fresh WebSocket failed: {error}"),
                None => panic!("fresh WebSocket closed before process_wait response"),
            }
        }
    })
    .await
    .expect("short process_wait response timed out");
    assert!(
        short_wait["error"]
            .as_str()
            .is_some_and(|error| error.contains("timed out waiting for process")),
        "expected the short wait to time out with the process still running: {short_wait}"
    );

    fresh.send(WsMessage::Text(
        json!({
            "type": "process_stop",
            "id": "disc-stop",
            "processId": process_id
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send process_stop");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut saw_stop = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, fresh.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "response" && value["id"] == "disc-stop" {
                    assert!(
                        value["success"].as_bool().unwrap_or(false),
                        "stop failed: {value}"
                    );
                    saw_stop = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(saw_stop, "process_stop response never arrived");

    fresh.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}
