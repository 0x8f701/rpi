use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use pi_cli::models_config::{
    clear_runtime_api_key, filter_models_for_resolved_auth_async, load_custom_models_from,
    load_radius_catalog_from, resolve_auth_json_path, resolve_model_request_auth,
    resolve_model_request_auth_async, set_runtime_api_key,
};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const DUMMY_KEY: &str = "dummy-key-not-a-secret";

fn write_config(directory: &Path, content: &str) -> std::path::PathBuf {
    let path = directory.join("models.json");
    fs::write(&path, content).expect("write models config");
    path
}

fn write_auth(directory: &Path, content: &str) -> std::path::PathBuf {
    let path = directory.join("auth.json");
    fs::write(&path, content).expect("write auth config");
    path
}

async fn spawn_openai_mock() -> (String, Arc<AsyncMutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let captured = Arc::new(AsyncMutex::new(String::new()));
    let server_capture = captured.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept mock request");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = socket.read(&mut buffer).await.expect("read mock request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..split]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= split + 4 + content_length {
                    break;
                }
            }
        }
        *server_capture.lock().await = String::from_utf8(bytes).expect("UTF-8 request");
        let body = concat!(
            "data: {\"id\":\"chat-1\",\"model\":\"custom-wire\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write mock response");
    });
    (format!("http://{address}/v1/"), captured)
}

fn request_body(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("request header/body split");
    serde_json::from_str(body).expect("JSON request body")
}

fn custom_config(key: Option<&str>) -> String {
    let key = key.map_or_else(String::new, |value| format!(r#", "apiKey":"{value}""#));
    format!(
        r#"{{"providers":{{"custom":{{"baseUrl":"https://example.test/v1","api":"openai-completions"{key},"models":[{{"id":"model","name":"Model","contextWindow":131072,"maxTokens":8192}}]}}}}}}"#
    )
}
#[tokio::test]
async fn explicit_radius_store_restores_offline_catalog() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let store_path = directory.path().join("models-store.json");
    let document = serde_json::json!({
        "version": 1,
        "providers": {
            "radius": {
                "models": [{
                    "id": "offline-radius",
                    "name": "Offline Radius",
                    "api": "pi-messages",
                    "provider": "radius",
                    "baseUrl": "https://radius-stream.example.test/v1",
                    "reasoning": true,
                    "input": ["text"],
                    "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
                    "contextWindow": 32000,
                    "maxTokens": 4096
                }],
                "checkedAt": 42
            }
        }
    });
    fs::write(
        &store_path,
        serde_json::to_vec(&document).expect("serialize Radius store"),
    )
    .expect("write Radius store");

    let restored = load_radius_catalog_from(&store_path, false)
        .await
        .expect("load offline Radius catalog")
        .expect("stored Radius catalog");
    assert_eq!(restored.checked_at, Some(42));
    assert_eq!(restored.models[0].id, "offline-radius");
    assert_eq!(
        pi_ai::get_model("radius", "offline-radius")
            .expect("registered offline Radius model")
            .base_url,
        "https://radius-stream.example.test/v1"
    );
}

#[tokio::test]
async fn malformed_optional_radius_store_is_quarantined_once() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let store_path = directory.path().join("models-store.json");
    fs::write(&store_path, b"{}").expect("write malformed optional store");

    assert!(
        load_radius_catalog_from(&store_path, false)
            .await
            .expect("malformed optional store is recoverable")
            .is_none()
    );
    assert!(!store_path.exists(), "malformed store must be quarantined");
    let quarantined = fs::read_dir(directory.path())
        .expect("read store directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .filter(|name| name.to_string_lossy().starts_with("models-store.json.invalid-"))
        .count();
    assert_eq!(quarantined, 1, "one quarantined cache is retained");

    assert!(
        load_radius_catalog_from(&store_path, false)
            .await
            .expect("subsequent launch stays quiet")
            .is_none()
    );
}

#[test]
fn loads_custom_model_and_resolves_config_key() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some(DUMMY_KEY)));
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    assert_eq!(model.max_tokens, 8192);
    let auth = resolve_model_request_auth(&model, None, None).expect("resolve auth");
    assert_eq!(auth.api_key, DUMMY_KEY);
}

#[test]
fn config_templates_cover_literal_and_escape_boundaries() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"template-boundaries":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"prefix-$TOKEN-${TOKEN}-$$-$!-$-${1bad}-${UNCLOSED-$?","models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("template-boundaries", "model").expect("registered model");
    let env = HashMap::from([("TOKEN".to_owned(), "value".to_owned())]);
    let auth = resolve_model_request_auth(&model, None, Some(&env)).expect("resolve template");
    assert_eq!(auth.api_key, "prefix-value-value-$-!-$-${1bad}-${UNCLOSED-$?");
}

#[test]
fn empty_template_env_value_is_unset_and_error_is_sanitized() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"empty-template":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"${EMPTY_TOKEN}","models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("empty-template", "model").expect("registered model");
    let env = HashMap::from([("EMPTY_TOKEN".to_owned(), String::new())]);
    let error = resolve_model_request_auth(&model, None, Some(&env))
        .expect_err("empty variable must be unavailable");
    let message = format!("{error:#}");
    assert!(
        message.contains("environment variable EMPTY_TOKEN referenced by models.json is not set")
    );
    assert!(!message.contains("apiKey"));
}

#[test]
fn custom_model_token_boundaries_are_explicit_and_atomic() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"bounds":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"key","models":[{"id":"floor","contextWindow":1,"maxTokens":1},{"id":"independent","contextWindow":1,"maxTokens":2},{"id":"ceiling","contextWindow":9223372036854775807,"maxTokens":9223372036854775807},{"id":"context-only","contextWindow":7},{"id":"tokens-only","maxTokens":9},{"id":"defaults"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load valid boundaries");
    let floor = pi_ai::get_model("bounds", "floor").expect("floor model");
    assert_eq!((floor.context_window, floor.max_tokens), (1, 1));
    let independent = pi_ai::get_model("bounds", "independent").expect("independent bounds");
    assert_eq!((independent.context_window, independent.max_tokens), (1, 2));
    let ceiling = pi_ai::get_model("bounds", "ceiling").expect("ceiling model");
    assert_eq!(
        (ceiling.context_window, ceiling.max_tokens),
        (i64::MAX, i64::MAX)
    );
    let context_only = pi_ai::get_model("bounds", "context-only").expect("context model");
    assert_eq!(
        (context_only.context_window, context_only.max_tokens),
        (7, 16_384)
    );
    let tokens_only = pi_ai::get_model("bounds", "tokens-only").expect("tokens model");
    assert_eq!(
        (tokens_only.context_window, tokens_only.max_tokens),
        (128_000, 9)
    );
    let defaults = pi_ai::get_model("bounds", "defaults").expect("default model");
    assert_eq!(
        (defaults.context_window, defaults.max_tokens),
        (128_000, 16_384)
    );

    for (provider, field, value) in [
        ("zero-context", "contextWindow", 0),
        ("negative-context", "contextWindow", -1),
        ("zero-tokens", "maxTokens", 0),
        ("negative-tokens", "maxTokens", -1),
    ] {
        let content = format!(
            r#"{{"providers":{{"{provider}":{{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"key","models":[{{"id":"invalid","{field}":{value}}}]}}}}}}"#
        );
        fs::write(&path, content).expect("write invalid boundary");
        let error = load_custom_models_from(&path).expect_err("invalid boundary must fail");
        let message = format!("{error:#}");
        assert!(message.contains(provider), "{message}");
        assert!(message.contains("invalid"), "{message}");
        assert!(message.contains(&format!("invalid {field}")), "{message}");
        assert!(pi_ai::get_model(provider, "invalid").is_none());
        let preserved = pi_ai::get_model("bounds", "floor").expect("previous model preserved");
        assert_eq!((preserved.context_window, preserved.max_tokens), (1, 1));
    }
}

#[test]
fn compat_merge_preserves_all_nested_compat_objects() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"compat":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"key","compat":{"openRouterRouting":{"allow_fallbacks":true,"order":["provider-a"]},"vercelGatewayRouting":{"only":["provider-a"]},"chatTemplateKwargs":{"enable_thinking":false},"supportsStrictMode":true},"models":[{"id":"model","compat":{"openRouterRouting":{"require_parameters":true,"order":["provider-b"]},"vercelGatewayRouting":{"order":["provider-b"]},"chatTemplateKwargs":{"thinking_budget":4096},"supportsStrictMode":false}}]}}}"#,
    );
    load_custom_models_from(&path).expect("load compat config");
    let compat = pi_ai::get_model("compat", "model")
        .and_then(|model| model.compat)
        .expect("merged compat");
    assert_eq!(
        compat,
        serde_json::json!({
            "openRouterRouting": {"allow_fallbacks": true, "require_parameters": true, "order": ["provider-b"]},
            "vercelGatewayRouting": {"only": ["provider-a"], "order": ["provider-b"]},
            "chatTemplateKwargs": {"enable_thinking": false, "thinking_budget": 4096},
            "supportsStrictMode": false
        })
    );
}

#[test]
fn explicit_and_runtime_key_precedence_is_stable() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some(DUMMY_KEY)));
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    set_runtime_api_key("custom", "runtime-key");
    assert_eq!(
        resolve_model_request_auth(&model, None, None)
            .expect("runtime auth")
            .api_key,
        "runtime-key"
    );
    assert_eq!(
        resolve_model_request_auth(&model, Some("explicit-key"), None)
            .expect("explicit auth")
            .api_key,
        "explicit-key"
    );
    clear_runtime_api_key("custom");
}

#[test]
fn stored_api_key_uses_original_auth_schema_and_precedes_models_config() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("configured-key")));
    write_auth(
        directory.path(),
        r#"{"custom":{"type":"api_key","key":"stored-key","env":{"ACCOUNT_ID":"account"}}}"#,
    );
    load_custom_models_from(&path).expect("load config and auth");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    assert_eq!(
        resolve_model_request_auth(&model, None, Some(&HashMap::new()))
            .expect("stored auth")
            .api_key,
        "stored-key"
    );
}

#[test]
fn stored_api_key_interpolates_credential_env_at_request_time() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"custom":{"baseUrl":"https://example.test/v1","api":"openai-completions","authHeader":true,"headers":{"X-Account":"${ACCOUNT_ID}"},"models":[{"id":"model"}]}}}"#,
    );
    write_auth(
        directory.path(),
        r#"{"custom":{"type":"api_key","key":"prefix-${TOKEN}","env":{"TOKEN":"stored-token","ACCOUNT_ID":"stored-account"}}}"#,
    );
    load_custom_models_from(&path).expect("load config and auth");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    let first = resolve_model_request_auth(&model, None, Some(&HashMap::new()))
        .expect("credential env auth");
    assert_eq!(first.api_key, "prefix-stored-token");
    assert_eq!(
        first.headers.get("X-Account").map(String::as_str),
        Some("stored-account")
    );

    let request_env = HashMap::from([
        ("TOKEN".to_owned(), "request-token".to_owned()),
        ("ACCOUNT_ID".to_owned(), "request-account".to_owned()),
    ]);
    let second =
        resolve_model_request_auth(&model, None, Some(&request_env)).expect("request env auth");
    assert_eq!(second.api_key, "prefix-request-token");
    assert_eq!(
        second.headers.get("X-Account").map(String::as_str),
        Some("request-account")
    );
}

#[test]
fn explicit_runtime_stored_and_configured_key_precedence_is_stable() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("configured-key")));
    write_auth(
        directory.path(),
        r#"{"custom":{"type":"api_key","key":"stored-key"}}"#,
    );
    load_custom_models_from(&path).expect("load config and auth");
    let model = pi_ai::get_model("custom", "model").expect("registered model");

    assert_eq!(
        resolve_model_request_auth(&model, None, Some(&HashMap::new()))
            .expect("stored auth")
            .api_key,
        "stored-key"
    );
    set_runtime_api_key("custom", "runtime-key");
    assert_eq!(
        resolve_model_request_auth(&model, None, Some(&HashMap::new()))
            .expect("runtime auth")
            .api_key,
        "runtime-key"
    );
    assert_eq!(
        resolve_model_request_auth(&model, Some("explicit-key"), Some(&HashMap::new()))
            .expect("explicit auth")
            .api_key,
        "explicit-key"
    );
    clear_runtime_api_key("custom");
}

#[test]
fn auth_header_requires_a_resolved_key_and_debug_redacts_request_secrets() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"auth-header":{"baseUrl":"https://example.test/v1","api":"openai-completions","authHeader":true,"headers":{"X-Secret":"${HEADER_SECRET}"},"models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("auth-header", "model").expect("registered model");
    let env = HashMap::from([(
        "HEADER_SECRET".to_owned(),
        "redacted-header-value".to_owned(),
    )]);
    let missing = resolve_model_request_auth(&model, None, Some(&env))
        .expect_err("authHeader without key must fail");
    let missing_message = format!("{missing:#}");
    assert!(missing_message.contains("authHeader requires an API key"));
    assert!(!missing_message.contains("redacted-header-value"));

    let auth = resolve_model_request_auth(&model, Some("redacted-api-key"), Some(&env))
        .expect("resolve authHeader");
    let authorization = auth
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str());
    assert_eq!(authorization, Some("Bearer redacted-api-key"));
    let debug = format!("{auth:?}");
    assert!(debug.contains("has_api_key: true"));
    assert!(!debug.contains("redacted-api-key"));
    assert!(!debug.contains("redacted-header-value"));
    assert!(!debug.contains("X-Secret"));
}

#[tokio::test(flavor = "current_thread")]
async fn custom_model_reaches_configured_endpoint_with_request_env_headers() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let (base_url, captured) = spawn_openai_mock().await;
    let content = serde_json::json!({
        "providers": {
            "wire-custom": {
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": "${WIRE_KEY}",
                "headers": {"X-Provider": "provider", "X-Env": "${WIRE_HEADER}"},
                "models": [{"id": "custom-wire", "headers": {"x-provider": "model"}}]
            }
        }
    });
    let path = write_config(directory.path(), &content.to_string());
    load_custom_models_from(&path).expect("load wire config");
    let mut model = pi_ai::get_model("wire-custom", "custom-wire").expect("registered wire model");
    let env = HashMap::from([
        ("WIRE_KEY".to_owned(), "wire-test-key".to_owned()),
        ("WIRE_HEADER".to_owned(), "request-env-value".to_owned()),
    ]);
    let auth = resolve_model_request_auth(&model, None, Some(&env)).expect("resolve request auth");
    model.headers = Some(auth.headers);
    let events = pi_ai::stream_simple(
        model,
        pi_ai::Context::default(),
        pi_ai::SimpleStreamOptions {
            stream: pi_ai::StreamOptions {
                api_key: Some(auth.api_key),
                env,
                ..pi_ai::StreamOptions::default()
            },
            ..pi_ai::SimpleStreamOptions::default()
        },
    )
    .await;
    while events.next().await.is_some() {}
    let result = events.result().await.expect("final response");
    assert_eq!(result.text(), "hello");
    assert_eq!(result.stop_reason, pi_ai::StopReason::Stop);

    let request = captured.lock().await;
    assert!(request.starts_with("POST /v1/chat/completions "));
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("authorization: bearer wire-test-key"));
    assert!(lower.contains("x-env: request-env-value"));
    assert!(lower.contains("x-provider: model"));
    assert!(!lower.contains("x-provider: provider"));
    assert_eq!(lower.matches("x-provider:").count(), 1);
    let body = request_body(&request);
    assert_eq!(body["model"], "custom-wire");
    assert_eq!(body["stream"], true);
}

#[test]
fn request_headers_resolve_templates_and_auth_header_wins_case_insensitively() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"custom":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"${KEY}","authHeader":true,"headers":{"authorization":"Bearer stale","X-Env":"${HEADER}"},"models":[{"id":"model","headers":{"x-env":"${MODEL_HEADER}"}}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    let env = HashMap::from([
        ("KEY".to_owned(), "resolved-key".to_owned()),
        ("HEADER".to_owned(), "provider-value".to_owned()),
        ("MODEL_HEADER".to_owned(), "model-value".to_owned()),
    ]);
    let auth = resolve_model_request_auth(&model, None, Some(&env)).expect("resolve auth");
    assert_eq!(
        auth.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str()),
        Some("Bearer resolved-key")
    );
    assert_eq!(
        auth.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-env"))
            .map(|(_, value)| value.as_str()),
        Some("model-value")
    );
    assert_eq!(
        auth.headers
            .keys()
            .filter(|name| name.eq_ignore_ascii_case("authorization"))
            .count(),
        1
    );
}

#[test]
fn header_only_auth_uses_empty_key_and_missing_template_fails() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"custom":{"baseUrl":"https://example.test/v1","api":"openai-completions","headers":{"Authorization":"${HEADER_KEY}"},"models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    let missing = resolve_model_request_auth(&model, None, Some(&HashMap::new()))
        .expect_err("missing template must fail");
    assert!(format!("{missing:#}").contains("HEADER_KEY"));
    let env = HashMap::from([("HEADER_KEY".to_owned(), "Bearer header-only".to_owned())]);
    let auth = resolve_model_request_auth(&model, None, Some(&env)).expect("header auth");
    assert!(auth.api_key.is_empty());
}

#[test]
fn empty_final_header_value_is_dropped() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"blank-header":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"test-key","headers":{"X-Optional":"   "},"models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("blank-header", "model").expect("registered model");
    let auth = resolve_model_request_auth(&model, None, Some(&HashMap::new()))
        .expect("resolve auth");
    assert!(!auth.headers.keys().any(|name| name.eq_ignore_ascii_case("x-optional")));
}


#[test]
fn missing_file_reverts_custom_models_and_builtin_overrides() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"deepseek":{"baseUrl":"https://override.test"},"custom":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"key","models":[{"id":"model"}]}}}"#,
    );
    let builtin_url = pi_ai::builtin_models("deepseek")
        .first()
        .expect("deepseek builtin")
        .base_url
        .clone();
    load_custom_models_from(&path).expect("load config");
    assert!(pi_ai::get_model("custom", "model").is_some());
    assert!(
        pi_ai::get_models("deepseek")
            .iter()
            .all(|model| model.base_url == "https://override.test")
    );
    fs::remove_file(&path).expect("remove config");
    load_custom_models_from(&path).expect("reload missing config");
    assert!(pi_ai::get_model("custom", "model").is_none());
    assert!(
        pi_ai::get_models("deepseek")
            .iter()
            .all(|model| model.base_url == builtin_url)
    );
}

#[test]
fn absent_auth_file_preserves_models_json_auth() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("configured-key")));
    load_custom_models_from(&path).expect("load config without auth file");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    assert_eq!(
        resolve_model_request_auth(&model, None, Some(&HashMap::new()))
            .expect("configured auth")
            .api_key,
        "configured-key"
    );
}

#[test]
fn malformed_reload_keeps_previous_valid_snapshot() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some(DUMMY_KEY)));
    load_custom_models_from(&path).expect("load config");
    fs::write(&path, "{ malformed").expect("write malformed config");
    assert!(load_custom_models_from(&path).is_err());
    assert!(pi_ai::get_model("custom", "model").is_some());
}

#[test]
fn invalid_auth_header_type_is_rejected_atomically_without_secret_leakage() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("stable-key")));
    load_custom_models_from(&path).expect("load stable config");

    fs::write(
        &path,
        r#"{"providers":{"invalid-auth-header":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"must-not-leak","authHeader":"yes","models":[{"id":"model"}]}}}"#,
    )
    .expect("write invalid authHeader");
    let error = load_custom_models_from(&path).expect_err("invalid authHeader type must fail");
    let message = format!("{error:#}");
    assert!(message.contains("Failed to parse models.json"));
    assert!(!message.contains("must-not-leak"));
    assert!(pi_ai::get_model("invalid-auth-header", "model").is_none());
    let stable = pi_ai::get_model("custom", "model").expect("previous snapshot preserved");
    assert_eq!(
        resolve_model_request_auth(&stable, None, Some(&HashMap::new()))
            .expect("stable auth")
            .api_key,
        "stable-key"
    );
}

#[test]
fn command_valued_credentials_are_rejected_without_execution() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"custom":{"baseUrl":"https://example.test/v1","api":"openai-completions","apiKey":"!printf secret","models":[{"id":"model"}]}}}"#,
    );
    load_custom_models_from(&path).expect("load config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    let error =
        resolve_model_request_auth(&model, None, None).expect_err("command values rejected");
    assert!(error.to_string().contains("not supported"));
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn command_valued_stored_credentials_are_rejected_without_secret_leakage() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("configured-key")));
    write_auth(
        directory.path(),
        r#"{"custom":{"type":"api_key","key":"!printf must-not-leak"}}"#,
    );
    load_custom_models_from(&path).expect("load stored command credential");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    let error = resolve_model_request_auth(&model, None, Some(&HashMap::new()))
        .expect_err("stored commands must be rejected");
    let message = format!("{error:#}");
    assert!(message.contains("command-valued stored API key is not supported"));
    assert!(!message.contains("must-not-leak"));
}

#[test]
fn malformed_and_unsupported_auth_are_contextual_atomic_and_sanitized() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("configured-key")));
    write_auth(
        directory.path(),
        r#"{"custom":{"type":"api_key","key":"stable-stored-key"}}"#,
    );
    load_custom_models_from(&path).expect("load initial snapshot");
    let model = pi_ai::get_model("custom", "model").expect("registered model");

    fs::write(directory.path().join("auth.json"), "{ malformed-secret")
        .expect("write malformed auth");
    let malformed = load_custom_models_from(&path).expect_err("malformed auth must fail");
    let malformed_message = format!("{malformed:#}");
    assert!(malformed_message.contains("Failed to parse auth.json"));
    assert!(malformed_message.contains("auth.json"));
    assert!(!malformed_message.contains("malformed-secret"));
    assert_eq!(
        resolve_model_request_auth(&model, None, Some(&HashMap::new()))
            .expect("previous snapshot preserved")
            .api_key,
        "stable-stored-key"
    );

    fs::write(
        directory.path().join("auth.json"),
        r#"{"custom":{"type":"password","secret":"unsupported-secret"}}"#,
    )
    .expect("write unsupported auth");
    let unsupported = load_custom_models_from(&path).expect_err("unsupported auth must fail");
    let unsupported_message = format!("{unsupported:#}");
    assert!(unsupported_message.contains("Invalid auth.json credential for provider \"custom\""));
    assert!(unsupported_message.contains("credential type is not supported"));
    assert!(!unsupported_message.contains("unsupported-secret"));

    fs::write(
        directory.path().join("auth.json"),
        r#"{"custom":{"type":"api_key","key":"shape-secret","env":{"TOKEN":7}}}"#,
    )
    .expect("write invalid auth shape");
    let invalid = load_custom_models_from(&path).expect_err("invalid auth shape must fail");
    let invalid_message = format!("{invalid:#}");
    assert!(invalid_message.contains("all field \"env\" values must be strings"));
    assert!(!invalid_message.contains("shape-secret"));
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_reload_preserves_previous_sibling_oauth_path() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"github-copilot":{"models":[]}}}"#,
    );
    write_auth(
        directory.path(),
        r#"{"github-copilot":{"type":"oauth","refresh":"test-refresh-token","access":"stable-test-token","expires":9223372036854775807,"availableModelIds":["gpt-5.4"]}}"#,
    );
    load_custom_models_from(&path).expect("load valid OAuth snapshot");
    fs::write(&path, "{ malformed").expect("write malformed config");
    assert!(load_custom_models_from(&path).is_err());

    let model = pi_ai::get_model("github-copilot", "gpt-5.4").expect("preserved Copilot model");
    let auth = resolve_model_request_auth_async(&model, None, Some(&HashMap::new()))
        .await
        .expect("preserved sibling OAuth resolution");
    assert_eq!(auth.api_key, "stable-test-token");
    assert_eq!(auth.available_model_ids.as_deref(), Some(&["gpt-5.4".to_owned()][..]));
    let debug = format!("{auth:?}");
    assert!(!debug.contains("stable-test-token"));
    assert!(!debug.contains("gpt-5.4"));
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_config_uses_sibling_oauth_metadata_without_global_fallback() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(
        directory.path(),
        r#"{"providers":{"github-copilot":{"models":[]}}}"#,
    );
    write_auth(
        directory.path(),
        r#"{"github-copilot":{"type":"oauth","refresh":"test-refresh-token","access":"test-copilot-token","expires":9223372036854775807,"availableModelIds":["gpt-5.4"]}}"#,
    );
    load_custom_models_from(&path).expect("load explicit Copilot snapshot");
    let model = pi_ai::get_model("github-copilot", "gpt-5.4").expect("Copilot model");
    let auth = resolve_model_request_auth_async(&model, None, Some(&HashMap::new()))
        .await
        .expect("resolve sibling OAuth credential");
    assert_eq!(auth.api_key, "test-copilot-token");
    assert_eq!(auth.available_model_ids.as_deref(), Some(&["gpt-5.4".to_owned()][..]));
    let debug = format!("{auth:?}");
    assert!(!debug.contains("test-copilot-token"));
    assert!(!debug.contains("gpt-5.4"));
    let models = vec![
        model.clone(),
        pi_ai::get_model("github-copilot", "gpt-5.5").expect("second Copilot model"),
        pi_ai::get_model("openai", "gpt-5.4").expect("non-Copilot model"),
    ];
    let filtered = filter_models_for_resolved_auth_async(models, Some(&HashMap::new())).await;
    assert!(filtered.iter().any(|candidate| candidate.provider == "github-copilot" && candidate.id == "gpt-5.4"));
    assert!(!filtered.iter().any(|candidate| candidate.provider == "github-copilot" && candidate.id == "gpt-5.5"));
    assert!(filtered.iter().any(|candidate| candidate.provider == "openai"));
}

#[test]
fn clearing_runtime_key_before_new_resolution_uses_reloaded_config_key() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let path = write_config(directory.path(), &custom_config(Some("config-key-a")));
    load_custom_models_from(&path).expect("load first config");
    let model = pi_ai::get_model("custom", "model").expect("registered model");
    set_runtime_api_key("custom", "explicit-key-a");
    fs::write(&path, custom_config(Some("config-key-b"))).expect("write second config");
    load_custom_models_from(&path).expect("reload config");
    clear_runtime_api_key("custom");
    assert_eq!(
        resolve_model_request_auth(&model, None, None)
            .expect("resolve reloaded config")
            .api_key,
        "config-key-b"
    );
}
#[test]
fn home_less_resolution_never_uses_current_directory() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        pi_cli::models_config::resolve_models_json_path(None, None, None),
        None
    );
    assert_eq!(
        pi_cli::models_config::resolve_models_json_path(
            None,
            Some(std::path::PathBuf::from("<agent-dir>")),
            None,
        ),
        Some(std::path::PathBuf::from("<agent-dir>/models.json"))
    );
}

#[test]
fn auth_path_resolution_matches_models_path_resolution() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(resolve_auth_json_path(None, None, None), None);
    assert_eq!(
        resolve_auth_json_path(None, Some(std::path::PathBuf::from("<agent-dir>")), None,),
        Some(std::path::PathBuf::from("<agent-dir>/auth.json"))
    );
}

#[test]
fn models_and_auth_files_enforce_exact_bounded_reads_atomically() {
    const LIMIT: usize = 8 * 1024 * 1024;
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = TempDir::new().expect("temporary directory");
    let models_path = directory.path().join("models.json");
    let auth_path = directory.path().join("auth.json");

    let mut boundary_models = b"{}".to_vec();
    boundary_models.resize(LIMIT, b' ');
    fs::write(&models_path, boundary_models).expect("write boundary models config");
    let mut boundary_auth = b"{}".to_vec();
    boundary_auth.resize(LIMIT, b' ');
    fs::write(&auth_path, boundary_auth).expect("write boundary auth config");
    load_custom_models_from(&models_path).expect("exact-boundary config files are accepted");

    fs::write(&models_path, custom_config(Some("stable-bounded-key")))
        .expect("write stable models config");
    fs::write(&auth_path, "{} ").expect("write stable auth config");
    load_custom_models_from(&models_path).expect("load stable snapshot");
    let stable_model = pi_ai::get_model("custom", "model").expect("stable registered model");

    let models_secret = "oversized-models-secret";
    let mut oversized_models = models_secret.as_bytes().to_vec();
    oversized_models.resize(LIMIT + 1, b' ');
    fs::write(&models_path, oversized_models).expect("write oversized models config");
    let models_error = load_custom_models_from(&models_path).expect_err("oversized models rejected");
    let models_message = format!("{models_error:#}");
    assert!(models_message.contains("models.json"));
    assert!(models_message.contains("exceeds"));
    assert!(!models_message.contains(models_secret));
    assert_eq!(pi_ai::get_model("custom", "model"), Some(stable_model.clone()));
    assert_eq!(
        resolve_model_request_auth(&stable_model, None, Some(&HashMap::new()))
            .expect("stable auth after oversized models")
            .api_key,
        "stable-bounded-key"
    );

    fs::write(&models_path, custom_config(Some("replacement-key")))
        .expect("write replacement models config");
    let auth_secret = "oversized-auth-secret";
    let mut oversized_auth = auth_secret.as_bytes().to_vec();
    oversized_auth.resize(LIMIT + 1, b' ');
    fs::write(&auth_path, oversized_auth).expect("write oversized auth config");
    let auth_error = load_custom_models_from(&models_path).expect_err("oversized auth rejected");
    let auth_message = format!("{auth_error:#}");
    assert!(auth_message.contains("auth.json"));
    assert!(auth_message.contains("exceeds"));
    assert!(!auth_message.contains(auth_secret));
    assert_eq!(pi_ai::get_model("custom", "model"), Some(stable_model.clone()));
    assert_eq!(
        resolve_model_request_auth(&stable_model, None, Some(&HashMap::new()))
            .expect("stable auth after oversized auth")
            .api_key,
        "stable-bounded-key"
    );
}
