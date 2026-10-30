//! Codex transport proof contracts (§10.4, §17 of the sandbox design):
//! the provider executes exclusively over governed HTTP/SSE (`Sse` and the
//! runtime-default `Auto` both resolve to that path) through the injected
//! `HttpExecutor` instead of an ambient reqwest client. The removed
//! WebSocket modes no longer exist as selectable values.

use base64::Engine as _;
use std::sync::Arc;

use futures_util::{FutureExt as _, StreamExt};
use pi_ai::providers::{OpenAICodexResponsesOptions, stream_openai_codex_responses};
use pi_ai::{
    API_OPENAI_CODEX_RESPONSES, AssistantMessageEventStream, Context, HttpMethod, HttpRequest,
    HttpResponse, Message, Model, ResolvedHttpAuthority, ResolvedHttpHost, ResolvedHttpScheme,
    StopReason, StreamOptions, Transport,
};
use serde_json::{Value, json};

fn token(account: &str) -> String {
    let payload = json!({"https://api.openai.com/auth":{"chatgpt_account_id":account}});
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
    format!("h.{encoded}.s")
}

fn model(base_url: String) -> Model {
    Model {
        id: "codex".into(),
        name: "Codex".into(),
        api: API_OPENAI_CODEX_RESPONSES.into(),
        provider: "openai-codex".into(),
        base_url,
        reasoning: true,
        input: vec!["text".into()],
        context_window: 128000,
        max_tokens: 8192,
        ..Model::default()
    }
}

fn events(id: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":id}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"r"}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"plan"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"enc"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"m","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"hi"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"m","content":[{"type":"output_text","text":"hi"}]}}),
        json!({"type":"response.completed","response":{"id":id,"status":"completed","usage":{"input_tokens":12,"output_tokens":5,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}}}),
    ]
}

async fn drain(stream: &AssistantMessageEventStream) {
    while stream.next().await.is_some() {}
}

/// Records every request routed through the injected executor and answers
/// with a canned SSE transcript, so the provider path is exercised without
/// any real HTTP.
struct RecordingExecutor {
    requests: Arc<std::sync::Mutex<Vec<HttpRequest>>>,
}

impl pi_ai::HttpExecutor for RecordingExecutor {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<HttpResponse>> {
        async move {
            self.requests.lock().expect("requests lock").push(request);
            let mut body = String::new();
            for event in events("injected") {
                body.push_str(&format!("data: {event}\n\n"));
            }
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            );
            let stream = futures_util::stream::iter(vec![Ok(body.into_bytes())]);
            Ok(HttpResponse::new(
                http::StatusCode::OK,
                headers,
                Box::pin(stream),
            ))
        }
        .boxed()
    }
}

/// Answers with an HTTP error whose body stream fails mid-read, so the
/// provider must propagate the collection failure instead of swallowing it.
struct FailingBodyExecutor;

impl pi_ai::HttpExecutor for FailingBodyExecutor {
    fn execute<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<HttpResponse>> {
        async move {
            let stream = futures_util::stream::iter(vec![Err(anyhow::anyhow!(
                "simulated body read failure"
            ))]);
            Ok(HttpResponse::new(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                http::HeaderMap::new(),
                Box::pin(stream),
            ))
        }
        .boxed()
    }
}

#[tokio::test]
async fn provider_http_uses_injected_executor() {
    let recorder = Arc::new(RecordingExecutor {
        requests: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let executor: pi_ai::SharedHttpExecutor = recorder.clone();
    let stream = stream_openai_codex_responses(
        model("https://api.example.com/v1".to_owned()),
        Context {
            system_prompt: "sys".into(),
            messages: vec![Message::user_text("hi", 1)],
            tools: vec![],
        },
        OpenAICodexResponsesOptions {
            stream: StreamOptions {
                api_key: Some(token("account")),
                transport: Transport::Sse,
                http_executor: Some(executor),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    drain(&stream).await;
    let output = stream.result().await.expect("stream result");
    assert_eq!(output.response_id.as_deref(), Some("injected"));
    assert_eq!(output.stop_reason, StopReason::Stop);
    assert_eq!(
        recorder.requests.lock().expect("requests lock").len(),
        1,
        "exactly one request through the injected executor",
    );
    let request = recorder
        .requests
        .lock()
        .expect("requests lock")
        .pop()
        .expect("exactly one recorded request");
    let parts = request.into_parts();
    assert_eq!(parts.method, HttpMethod::Post);
    assert_eq!(parts.authority.scheme(), ResolvedHttpScheme::Https);
    assert_eq!(
        parts.authority.host(),
        &ResolvedHttpHost::Dns("api.example.com".to_owned())
    );
    assert_eq!(parts.authority.port(), 443);
    assert_eq!(parts.path_and_query, "/v1/codex/responses");
    let auth = parts
        .headers
        .iter()
        .find(|(name, _)| name.as_str() == "authorization")
        .expect("authorization header");
    let expected = format!("Bearer {}", token("account"));
    assert_eq!(
        auth.1.to_str().expect("authorization header text"),
        expected
    );
}

#[tokio::test]
async fn auto_transport_is_normalized_to_governed_sse() {
    // `Auto` (the runtime default transport) must resolve to the governed
    // HTTP/SSE path: the request goes through the injected executor exactly
    // once and completes normally, exactly like `Sse`.
    let recorder = Arc::new(RecordingExecutor {
        requests: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let executor: pi_ai::SharedHttpExecutor = recorder.clone();
    let stream = stream_openai_codex_responses(
        model("https://api.example.com/v1".to_owned()),
        Context {
            system_prompt: "sys".into(),
            messages: vec![Message::user_text("hi", 1)],
            tools: vec![],
        },
        OpenAICodexResponsesOptions {
            stream: StreamOptions {
                api_key: Some(token("account")),
                transport: Transport::Auto,
                http_executor: Some(executor),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    drain(&stream).await;
    let output = stream.result().await.expect("stream result");
    assert_eq!(output.response_id.as_deref(), Some("injected"));
    assert_eq!(output.stop_reason, StopReason::Stop);
    assert_eq!(
        recorder.requests.lock().expect("requests lock").len(),
        1,
        "Auto must execute exactly one governed SSE request",
    );
}

#[tokio::test]
async fn response_body_collection_failure_propagates() {
    // A body stream that errors mid-read must surface the collection
    // failure instead of silently degrading into an empty error body.
    let executor: pi_ai::SharedHttpExecutor = Arc::new(FailingBodyExecutor);
    let stream = stream_openai_codex_responses(
        model("https://api.example.com/v1".to_owned()),
        Context::default(),
        OpenAICodexResponsesOptions {
            stream: StreamOptions {
                api_key: Some(token("account")),
                transport: Transport::Sse,
                http_executor: Some(executor),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    drain(&stream).await;
    let output = stream.result().await.expect("stream result");
    assert_eq!(output.stop_reason, StopReason::Error);
    let message = output.error_message.expect("error message");
    assert!(message.contains("simulated body read failure"), "{message}");
}
