use pi_ai::*;
use pi_ai::providers::{PiMessagesOptions, stream_pi_messages};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, oneshot},
};

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = socket.read(&mut buffer).await.expect("read request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..split]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= split + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(request).expect("UTF-8 request")
}

async fn spawn_once(status: &str, body: String) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let captured = Arc::new(Mutex::new(String::new()));
    let target = captured.clone();
    let status = status.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        *target.lock().await = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    (format!("http://{address}"), captured)
}

fn usage() -> Value {
    json!({"input":4,"output":5,"cacheRead":1,"cacheWrite":2,"reasoning":3,"totalTokens":15,"cost":{"input":0.1,"output":0.2,"cacheRead":0.01,"cacheWrite":0.02,"total":0.33}})
}
fn model(base_url: String) -> Model {
    Model {
        id: "radius-model".into(),
        name: "Radius Model".into(),
        api: API_PI_MESSAGES.into(),
        provider: "radius".into(),
        base_url,
        reasoning: true,
        input: vec!["text".into(), "image".into()],
        context_window: 128000,
        max_tokens: 8192,
        ..Model::default()
    }
}

#[tokio::test]
async fn streams_native_replay_thinking_tools_usage_and_payload() {
    let body = [json!({"type":"start"}),json!({"type":"thinking_start","contentIndex":0}),json!({"type":"thinking_delta","contentIndex":0,"delta":"plan"}),json!({"type":"thinking_end","contentIndex":0,"content":"plan","contentSignature":"sig","redacted":true}),json!({"type":"text_start","contentIndex":1}),json!({"type":"text_delta","contentIndex":1,"delta":"hello"}),json!({"type":"text_end","contentIndex":1,"content":"hello","contentSignature":"text-sig"}),json!({"type":"toolcall_start","contentIndex":2,"id":"call-1","toolName":"lookup"}),json!({"type":"toolcall_delta","contentIndex":2,"delta":"{\"q\":\"rad"}),json!({"type":"toolcall_delta","contentIndex":2,"delta":"ius\"}"}),json!({"type":"toolcall_end","contentIndex":2,"toolCall":{"id":"call-1","name":"lookup","arguments":{"q":"radius"}}}),json!({"type":"done","reason":"toolUse","usage":usage(),"responseId":"resp-1"})].into_iter().map(|event| format!("data: {event}\n\n")).collect();
    let (base_url, captured) = spawn_once("200 OK", body).await;
    let context = Context {
        system_prompt: "system".into(),
        messages: vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "old".into(),
            tool_name: "lookup".into(),
            content: vec![ContentBlock::Image {
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            }],
            usage: None,
            details: Some(json!({"native":true})),
            added_tool_names: vec![],
            is_error: false,
            timestamp: 3,
        })],
        tools: vec![],
    };
    let stream = stream_pi_messages(
        model(base_url),
        context.clone(),
        PiMessagesOptions {
            stream: StreamOptions {
                api_key: Some("secret-key".into()),
                temperature: Some(0.2),
                max_tokens: Some(2048),
                cache_retention: CacheRetention::Long,
                session_id: Some("session".into()),
                ..Default::default()
            },
            reasoning: Some(ThinkingLevel::High),
            tool_choice: Some(json!("required")),
            debug: true,
        },
    );
    while stream.next().await.is_some() {}
    let output = stream.result().await.expect("result");
    assert_eq!(output.stop_reason, StopReason::ToolUse);
    assert_eq!(output.response_id.as_deref(), Some("resp-1"));
    assert_eq!(output.usage.total_tokens, 15);
    assert_eq!(output.usage.cost.total, 0.33);
    assert!(
        matches!(&output.content[0],ContentBlock::Thinking{thinking_signature:Some(signature),redacted:true,..} if signature=="sig")
    );
    assert!(
        matches!(&output.content[2],ContentBlock::ToolCall(call) if call.arguments==json!({"q":"radius"}))
    );
    let request = captured.lock().await.clone();
    assert!(request.starts_with("POST /messages?debug=1 "));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-key")
    );
    let payload: Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).expect("request body"))
            .expect("JSON payload");
    assert_eq!(
        payload["context"],
        serde_json::to_value(context).expect("context")
    );
    assert_eq!(payload["options"]["reasoning"], "high");
    assert_eq!(payload["options"]["cacheRetention"], "long");
    assert_eq!(payload["options"]["toolChoice"], "required");
}

#[tokio::test]
async fn retries_and_case_insensitive_header_override_wins() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer override-key")
            );
            assert!(!request.to_ascii_lowercase().contains("bearer default-key"));
            assert_eq!(
                request
                    .lines()
                    .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                    .count(),
                1
            );
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let response = if attempt == 0 {
                "HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                let body = format!(
                    "data: {}\n\n",
                    json!({"type":"done","reason":"stop","usage":usage()})
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response");
        }
    });
    let stream = stream_pi_messages(
        model(format!("http://{address}")),
        Context::default(),
        StreamOptions {
            api_key: Some("default-key".into()),
            headers: HashMap::from([("AUTHORIZATION".into(), "Bearer override-key".into())]),
            max_retries: 1,
            ..Default::default()
        }
        .into(),
    );
    while stream.next().await.is_some() {}
    assert_eq!(
        stream.result().await.expect("result").stop_reason,
        StopReason::Stop
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancellation_aborts_stalled_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (seen_tx, seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
            .await
            .expect("headers");
        let _ = seen_tx.send(());
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let token = tokio_util::sync::CancellationToken::new();
    let stream = stream_pi_messages(
        model(format!("http://{address}")),
        Context::default(),
        StreamOptions {
            api_key: Some("key".into()),
            abort_signal: Some(token.clone()),
            ..Default::default()
        }
        .into(),
    );
    seen_rx.await.expect("request seen");
    token.cancel();
    while stream.next().await.is_some() {}
    assert_eq!(
        stream.result().await.expect("result").stop_reason,
        StopReason::Aborted
    );
}

#[tokio::test]
async fn redacts_api_key_and_overridden_authorization_from_http_errors() {
    let (base_url, _) = spawn_once(
        "401 Unauthorized",
        json!({"error":{"message":"override-secret rejected default-secret","code":"bad_auth"}})
            .to_string(),
    )
    .await;
    let stream = stream_pi_messages(
        model(base_url),
        Context::default(),
        StreamOptions {
            api_key: Some("default-secret".into()),
            headers: HashMap::from([(
                "Authorization".into(),
                "Bearer override-secret".into(),
            )]),
            ..Default::default()
        }
        .into(),
    );
    while stream.next().await.is_some() {}
    let message = stream
        .result()
        .await
        .expect("result")
        .error_message
        .expect("error message");
    assert!(message.contains("[REDACTED]"));
    assert!(!message.contains("default-secret"));
    assert!(!message.contains("override-secret"));
}

#[tokio::test]
async fn async_provider_hooks_transform_request_headers_and_observe_public_response() {
    let body = format!(
        "data: {}\n\n",
        json!({"type":"done","reason":"stop","usage":usage()})
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let captured = Arc::new(Mutex::new(String::new()));
    let target = captured.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        *target.lock().await = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-public-observed: yes\r\nset-cookie: private=secret\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let sync_order = order.clone();
    let request_order = order.clone();
    let header_order = order.clone();
    let response_order = order.clone();
    let observed = Arc::new(std::sync::Mutex::new(None::<ProviderResponse>));
    let observed_response = observed.clone();
    let stream = stream_pi_messages(
        model(format!("http://{address}")),
        Context::default(),
        StreamOptions {
            api_key: Some("default-key".into()),
            headers: HashMap::from([
                ("X-Remove".into(), "remove-me".into()),
                ("X-Keep".into(), "original".into()),
            ]),
            on_payload: Some(Arc::new(move |mut payload, _| {
                sync_order.lock().expect("sync order").push("sync-payload");
                payload["syncHook"] = json!(true);
                Ok(payload)
            })),
            before_provider_request: Some(Arc::new(move |mut payload, _| {
                let request_order = request_order.clone();
                Box::pin(async move {
                    request_order
                        .lock()
                        .expect("request order")
                        .push("async-payload");
                    assert_eq!(payload["syncHook"], true);
                    payload["asyncHook"] = json!(true);
                    Ok(payload)
                })
            })),
            before_provider_headers: Some(Arc::new(move |mut headers, _| {
                let header_order = header_order.clone();
                Box::pin(async move {
                    header_order
                        .lock()
                        .expect("header order")
                        .push("async-headers");
                    assert_eq!(
                        headers.get("content-type").and_then(Option::as_deref),
                        Some("application/json")
                    );
                    headers.insert("x-keep".into(), Some("mutated".into()));
                    headers.insert("X-REMOVE".into(), None);
                    headers.insert("x-added".into(), Some("yes".into()));
                    Ok(headers)
                })
            })),
            after_provider_response: Some(Arc::new(move |response, _| {
                let response_order = response_order.clone();
                let observed_response = observed_response.clone();
                Box::pin(async move {
                    response_order
                        .lock()
                        .expect("response order")
                        .push("async-response");
                    *observed_response.lock().expect("observed response") = Some(response);
                    Ok(())
                })
            })),
            ..Default::default()
        }
        .into(),
    );
    while stream.next().await.is_some() {}
    assert_eq!(
        stream.result().await.expect("result").stop_reason,
        StopReason::Stop
    );

    assert_eq!(
        *order.lock().expect("hook order"),
        vec![
            "sync-payload",
            "async-payload",
            "async-headers",
            "async-response"
        ]
    );
    let request = captured.lock().await.clone();
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("x-keep: mutated"));
    assert!(lower.contains("x-added: yes"));
    assert!(!lower.contains("x-remove:"));
    let payload: Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).expect("request body"))
            .expect("JSON payload");
    assert_eq!(payload["syncHook"], true);
    assert_eq!(payload["asyncHook"], true);

    let response = observed
        .lock()
        .expect("observed response")
        .clone()
        .expect("response hook called");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("x-public-observed").map(String::as_str),
        Some("yes")
    );
    assert!(!response.headers.contains_key("set-cookie"));
}
