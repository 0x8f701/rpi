//! OpenAI Codex Responses provider over governed HTTP/SSE.

use std::{collections::HashMap, sync::Arc, time::Duration};

use ::http::{HeaderMap, StatusCode};
use anyhow::{Result, anyhow};
use base64::Engine as _;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::{common, responses};
use crate::*;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const RETRY_BASE_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

#[derive(Clone, Default)]
pub struct OpenAICodexResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<String>,
}

impl From<StreamOptions> for OpenAICodexResponsesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
enum CodexError {
    Api {
        code: Option<String>,
        message: String,
    },
    Protocol(String),
    Transport(String),
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { message, .. } | Self::Protocol(message) | Self::Transport(message) => {
                formatter.write_str(message)
            }
        }
    }
}
impl std::error::Error for CodexError {}
impl CodexError {
    fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

pub fn register_openai_codex_responses() {
    register_api_provider(
        ApiProvider {
            api: API_OPENAI_CODEX_RESPONSES.into(),
            stream: Arc::new(|model, context, options| {
                async move {
                    stream_openai_codex_responses(
                        model,
                        context,
                        OpenAICodexResponsesOptions::from(options),
                    )
                }
                .boxed()
            }),
            stream_simple: Arc::new(|model, context, options| {
                async move { stream_simple_openai_codex_responses(model, context, options) }.boxed()
            }),
            generate_image: None,
            resolve_http_authorities: Arc::new(|model, _| {
                let (authority, _) = resolve_http_endpoint(&resolve_codex_url(&model.base_url))?;
                Ok(std::collections::BTreeSet::from([authority]))
            }),
        },
        None,
    );
}

pub fn stream_simple_openai_codex_responses(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let requested_max = options.stream.max_tokens.unwrap_or(model.max_tokens);
    let mut stream = options.stream;
    stream.max_tokens = Some(clamp_max_tokens_to_context(&model, &context, requested_max));
    let reasoning_effort = options.reasoning.and_then(|level| {
        let clamped = clamp_thinking_level(&model, thinking_level_name(level));
        (clamped != "off").then(|| clamped.to_owned())
    });
    stream_openai_codex_responses(
        model,
        context,
        OpenAICodexResponsesOptions {
            stream,
            reasoning_effort,
            ..OpenAICodexResponsesOptions::default()
        },
    )
}

pub fn stream_openai_codex_responses(
    model: Model,
    context: Context,
    options: OpenAICodexResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let output_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        match run_codex(&model, &context, &options, &output_stream, &mut output).await {
            Ok(()) => {
                output_stream
                    .push(AssistantMessageEvent::Done {
                        reason: output.stop_reason,
                        message: output.clone(),
                    })
                    .await;
                output_stream.end(Some(output)).await;
            }
            Err(error) => {
                output.stop_reason = if common::is_aborted(&options.stream) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(if output.stop_reason == StopReason::Aborted {
                    "Request was aborted".into()
                } else {
                    sanitize_error(&error.to_string(), &model, &options.stream)
                });
                output_stream
                    .push(AssistantMessageEvent::Error {
                        reason: output.stop_reason,
                        error: output.clone(),
                    })
                    .await;
                output_stream.end(Some(output)).await;
            }
        }
    });
    stream
}

async fn run_codex(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    stream: &AssistantMessageEventStream,
    output: &mut AssistantMessage,
) -> Result<()> {
    // The governed HTTPS/SSE transport is the only provider transport; the
    // removed WebSocket modes no longer exist as selectable values.
    let token = options
        .stream
        .api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("No API key for provider: {}", model.provider))?;
    let account_id = extract_account_id(token)?;
    let cache_session_id = if options.stream.cache_retention == CacheRetention::None {
        None
    } else {
        options
            .stream
            .session_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(responses::clamp_prompt_cache_key)
    };
    let compat_model = codex_compat_model(model);
    let grammar_properties = responses::grammar_tool_input_properties(
        &context.tools,
        responses::get_responses_compat(&compat_model).supports_openai_grammar_tools,
    )?;
    let body = build_codex_body(model, context, options, cache_session_id.as_deref())?;
    let body = common::apply_provider_request(body, model, &options.stream).await?;
    if !body.is_object() {
        return Err(anyhow!("Codex payload hook must return a JSON object"));
    }
    let headers = common::apply_provider_headers(
        build_codex_headers(
            model,
            &options.stream,
            token,
            &account_id,
            cache_session_id.as_deref(),
        )?,
        model,
        &options.stream,
    )
    .await?;
    let (event_tx, mut event_rx) = unbounded_channel();
    let drain_stream = stream.clone();
    let drainer = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            drain_stream.push(event).await;
        }
    });
    let mut start_emitted = false;
    let result = process_sse(
        model,
        options,
        &body,
        &headers,
        &grammar_properties,
        output,
        &event_tx,
        stream,
        &mut start_emitted,
    )
    .await;
    drop(event_tx);
    let _ = drainer.await;
    result?;
    if common::is_aborted(&options.stream) {
        return Err(anyhow!("Request was aborted"));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(anyhow!(
            "Codex stream ended without a terminal response event"
        ));
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(anyhow!("Codex stream ended with an error status"));
    }
    Ok(())
}

fn build_codex_body(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    session_id: Option<&str>,
) -> Result<Value> {
    let compat_model = codex_compat_model(model);
    let mut input_context = context.clone();
    input_context.system_prompt.clear();
    let response_options = responses::OpenAIResponsesOptions {
        stream: options.stream.clone(),
        reasoning_effort: options.reasoning_effort.clone(),
        reasoning_summary: options
            .reasoning_summary
            .clone()
            .or_else(|| Some("auto".into())),
        service_tier: options.service_tier.clone(),
        tool_choice: options.tool_choice.as_ref().map(|choice| json!(choice)),
        responses_stateful_chain: false,
    };
    let mut body =
        responses::build_responses_params(&compat_model, &input_context, &response_options)?;
    let object = body.as_object_mut().expect("Responses body is an object");
    object.insert(
        "instructions".into(),
        json!(if context.system_prompt.is_empty() {
            "You are a helpful assistant."
        } else {
            context.system_prompt.as_str()
        }),
    );
    object.insert("store".into(), json!(false));
    object.insert("stream".into(), json!(true));
    object.insert(
        "text".into(),
        json!({ "verbosity": options.text_verbosity.as_deref().unwrap_or("low") }),
    );
    object.insert("include".into(), json!(["reasoning.encrypted_content"]));
    object.insert(
        "tool_choice".into(),
        json!(options.tool_choice.as_deref().unwrap_or("auto")),
    );
    object.insert("parallel_tool_calls".into(), json!(true));
    object.remove("max_output_tokens");
    object.remove("prompt_cache_retention");
    object.remove("prompt_cache_options");
    if options.reasoning_effort.is_none() {
        object.remove("reasoning");
    }
    if let Some(session_id) = session_id {
        object.insert("prompt_cache_key".into(), json!(session_id));
    } else {
        object.remove("prompt_cache_key");
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("strict") == Some(&Value::Bool(false))
            {
                tool.as_object_mut()
                    .expect("tool object")
                    .insert("strict".into(), Value::Null);
            }
        }
    }
    Ok(body)
}

fn codex_compat_model(model: &Model) -> Model {
    let mut model = model.clone();
    let mut compat = model
        .compat
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    compat
        .entry("supportsStrictMode")
        .or_insert(Value::Bool(true));
    model.compat = Some(Value::Object(compat));
    model
}

#[allow(clippy::too_many_arguments)]
async fn process_sse(
    model: &Model,
    options: &OpenAICodexResponsesOptions,
    body: &Value,
    headers: &HeaderMap,
    grammar_properties: &HashMap<String, String>,
    output: &mut AssistantMessage,
    event_tx: &UnboundedSender<AssistantMessageEvent>,
    output_stream: &AssistantMessageEventStream,
    start_emitted: &mut bool,
) -> Result<()> {
    let response = send_codex_sse_with_retry(
        &resolve_codex_url(&model.base_url),
        headers,
        serde_json::to_vec(body)?,
        model,
        &options.stream,
    )
    .await?;
    if !*start_emitted {
        *start_emitted = true;
        output_stream
            .push(AssistantMessageEvent::Start {
                partial: output.clone(),
            })
            .await;
    }
    let mut state =
        responses::StreamState::with_grammar_input_properties(grammar_properties.clone());
    consume_codex_sse(response, &options.stream, |value| {
        handle_codex_event(
            value,
            output,
            &mut state,
            model,
            options.service_tier.as_deref(),
            event_tx,
        )
    })
    .await?;
    if !state.saw_terminal {
        return Err(anyhow!(
            "Codex SSE stream ended before a terminal response event"
        ));
    }
    state.materialize(output);
    Ok(())
}

async fn send_codex_sse_with_retry(
    url: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
    model: &Model,
    options: &StreamOptions,
) -> Result<HttpResponse> {
    for attempt in 0..=options.max_retries {
        if common::is_aborted(options) {
            return Err(anyhow!("Request was aborted"));
        }
        let request = common::build_request(options, HttpMethod::Post, url, headers, body.clone())?;
        let response = match common::client(options)?.execute(request).await {
            Ok(response) => response,
            Err(error) if attempt < options.max_retries && !common::is_aborted(options) => {
                abortable_sleep(options, retry_backoff(attempt)).await?;
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        common::notify_response(
            options,
            response.status(),
            response.headers().clone(),
            model,
        )
        .await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let response_headers = response.headers().clone();
        let text = read_response_text(response).await?;
        if attempt < options.max_retries && codex_retryable(status, &text) {
            let delay = retry_after_delay(&response_headers)
                .map(|delay| validate_retry_delay(delay, options))
                .transpose()?
                .unwrap_or_else(|| retry_backoff(attempt));
            abortable_sleep(options, delay).await?;
            continue;
        }
        return Err(anyhow!(format_codex_http_error(status, &text)));
    }
    Err(anyhow!("Codex retry loop ended unexpectedly"))
}

async fn read_response_text(response: HttpResponse) -> Result<String> {
    Ok(String::from_utf8_lossy(&collect_response_body(response).await?).into_owned())
}

async fn abortable_sleep(options: &StreamOptions, delay: Duration) -> Result<()> {
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            _ = tokio::time::sleep(delay) => Ok(()),
        },
        None => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
    }
}

fn retry_backoff(attempt: usize) -> Duration {
    Duration::from_millis(RETRY_BASE_DELAY_MS.saturating_mul(1_u64 << attempt.min(20)))
}

fn codex_retryable(status: StatusCode, text: &str) -> bool {
    if terminal_rate_limit(text) {
        return false;
    }
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
        || regex::Regex::new(
            "(?i)rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused",
        )
        .expect("static regex")
        .is_match(text)
}

fn terminal_rate_limit(text: &str) -> bool {
    regex::Regex::new("(?i)GoUsageLimitError|FreeUsageLimitError|(?:Weekly|Monthly) usage limit(?: reached)?|usage_limit_reached|usage_not_included|available balance|insufficient_quota|out of budget|quota exceeded|billing limit")
        .expect("static regex").is_match(text)
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
    {
        return Some(Duration::from_millis(milliseconds.max(0.0) as u64));
    }
    let retry_after = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = retry_after.parse::<f64>() {
        if seconds.is_finite() {
            return Some(Duration::from_millis((seconds.max(0.0) * 1_000.0) as u64));
        }
    }
    let date = chrono::DateTime::parse_from_rfc2822(retry_after)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(Duration::from_millis(
        (date - chrono::Utc::now()).num_milliseconds().max(0) as u64,
    ))
}

fn validate_retry_delay(delay: Duration, options: &StreamOptions) -> Result<Duration> {
    let limit = options
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if limit > 0 && delay > Duration::from_millis(limit) {
        return Err(anyhow!(
            "Server requested {}s retry delay (max: {}s)",
            delay.as_millis().div_ceil(1_000),
            limit.div_ceil(1_000)
        ));
    }
    Ok(delay)
}

fn format_codex_http_error(status: StatusCode, text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let code = error
        .and_then(|value| value.get("code").or_else(|| value.get("type")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if text.trim().is_empty() {
                "Request failed"
            } else {
                text.trim()
            }
        });
    let is_usage_limit =
        terminal_rate_limit(text) || matches!(code, "usage_limit_reached" | "usage_not_included");
    if is_usage_limit {
        let plan = error
            .and_then(|value| value.get("plan_type"))
            .and_then(Value::as_str)
            .map(|plan| format!("; plan={}", plan.to_lowercase()))
            .unwrap_or_default();
        return format!("HTTP 429 usage/quota exhausted{plan}: {message}");
    }
    if status == StatusCode::TOO_MANY_REQUESTS || code == "rate_limit_exceeded" {
        return format!("HTTP 429 rate limited: {message}");
    }
    message.to_owned()
}

async fn consume_codex_sse<F>(
    response: HttpResponse,
    options: &StreamOptions,
    mut handle: F,
) -> Result<()>
where
    F: FnMut(Value) -> std::result::Result<bool, CodexError>,
{
    let mut bytes = response.into_body();
    let mut pending = String::new();
    loop {
        let next = match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
                chunk = bytes.next() => chunk,
            },
            None => bytes.next().await,
        };
        let Some(chunk) = next else { break };
        pending.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some((position, separator_len)) = sse_separator(&pending) {
            let event = pending[..position].to_owned();
            pending.drain(..position + separator_len);
            if let Some(data) = sse_data(&event) {
                if data == "[DONE]" {
                    continue;
                }
                let value = serde_json::from_str::<Value>(&data).map_err(|error| {
                    anyhow!(CodexError::Protocol(format!(
                        "Invalid Codex SSE JSON: {error}"
                    )))
                })?;
                if handle(value).map_err(anyhow::Error::new)? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn sse_separator(input: &str) -> Option<(usize, usize)> {
    input
        .find("\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| input.find("\n\n").map(|position| (position, 2)))
}

fn sse_data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event
        .lines()
        .filter_map(|line| {
            line.strip_suffix('\r')
                .unwrap_or(line)
                .strip_prefix("data:")
        })
        .map(str::trim_start)
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n").trim().to_owned())
}

fn handle_codex_event(
    mut event: Value,
    output: &mut AssistantMessage,
    state: &mut responses::StreamState,
    model: &Model,
    requested_service_tier: Option<&str>,
    event_tx: &UnboundedSender<AssistantMessageEvent>,
) -> std::result::Result<bool, CodexError> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if event_type == "error" {
        let nested = event.get("error").and_then(Value::as_object);
        let code = event
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| {
                nested
                    .and_then(|value| value.get("code"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        let message = event
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| {
                nested
                    .and_then(|value| value.get("message"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
            .or_else(|| code.clone())
            .unwrap_or_else(|| "Unknown Codex error".into());
        return Err(CodexError::Api {
            code,
            message: format!("Codex error: {message}"),
        });
    }
    if event_type == "response.failed" {
        let error = event.pointer("/response/error");
        let code = error
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = error
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Codex response failed")
            .to_owned();
        return Err(CodexError::Api { code, message });
    }
    let terminal = matches!(
        event_type.as_str(),
        "response.done" | "response.completed" | "response.incomplete"
    );
    if terminal {
        if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
            if !response.contains_key("status") && event_type == "response.incomplete" {
                response.insert("status".into(), json!("incomplete"));
            }
            if response.get("service_tier").and_then(Value::as_str) == Some("default")
                && matches!(requested_service_tier, Some("flex" | "priority"))
            {
                response.insert("service_tier".into(), json!(requested_service_tier));
            }
        }
        event
            .as_object_mut()
            .expect("event object")
            .insert("type".into(), json!("response.completed"));
    }
    responses::handle_event(
        None,
        &event.to_string(),
        output,
        state,
        model,
        requested_service_tier,
        event_tx,
    )
    .map_err(|error| CodexError::Protocol(error.to_string()))?;
    Ok(terminal)
}

fn build_codex_headers(
    model: &Model,
    options: &StreamOptions,
    token: &str,
    account_id: &str,
    request_id: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(model_headers) = &model.headers {
        common::insert_header_map(&mut headers, model_headers)?;
    }
    common::insert_header_map(&mut headers, &options.headers)?;
    common::insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
    common::insert_header(&mut headers, "chatgpt-account-id", account_id)?;
    common::insert_header(&mut headers, "originator", "rpi")?;
    common::insert_header(
        &mut headers,
        "user-agent",
        &format!("rpi ({}; {})", std::env::consts::OS, std::env::consts::ARCH),
    )?;
    common::insert_header(&mut headers, "openai-beta", "responses=experimental")?;
    common::insert_header(&mut headers, "accept", "text/event-stream")?;
    common::insert_header(&mut headers, "content-type", "application/json")?;
    if let Some(request_id) = request_id {
        common::insert_header(&mut headers, "session-id", request_id)?;
        common::insert_header(&mut headers, "x-client-request-id", request_id)?;
    }
    Ok(headers)
}

fn extract_account_id(token: &str) -> Result<String> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("Failed to extract accountId from token"))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .map_err(|_| anyhow!("Failed to extract accountId from token"))?;
    let value: Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow!("Failed to extract accountId from token"))?;
    value
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Failed to extract accountId from token"))
}

fn resolve_codex_url(base_url: &str) -> String {
    let normalized = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url.trim()
    }
    .trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn sanitize_error(message: &str, model: &Model, options: &StreamOptions) -> String {
    let mut sanitized = message.to_owned();
    let mut secrets = Vec::new();
    if let Some(token) = options.api_key.as_deref() {
        secrets.push(token);
    }
    if let Some(headers) = &model.headers {
        secrets.extend(headers.values().map(String::as_str));
    }
    secrets.extend(options.headers.values().map(String::as_str));
    for secret in secrets {
        if !secret.is_empty() {
            sanitized = sanitized.replace(secret, "[REDACTED]");
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_sse_framing_helpers_handle_crlf_lf_and_multiline_data() {
        // LF-separated events split on the double newline.
        assert_eq!(sse_separator("data: {\"a\":1}\n\nrest"), Some((13, 2)));
        // CRLF events split on the 4-byte separator (find locates the \n of
        // the first CRLF, so the position is the event length without CR).
        assert_eq!(sse_separator("data: {\"a\":1}\r\n\r\nrest"), Some((13, 4)));
        // No separator yet (partial chunk) yields None.
        assert_eq!(sse_separator("data: {\"a\":1}\n"), None);
        assert_eq!(sse_separator(""), None);

        // Single data line: prefix stripped, whitespace trimmed.
        assert_eq!(sse_data("data: {\"a\":1}"), Some("{\"a\":1}".to_owned()));
        // Multi-line data joins with newlines (SSE continuation lines).
        assert_eq!(
            sse_data("data: line-one\ndata: line-two"),
            Some("line-one\nline-two".to_owned())
        );
        // CRLF line endings inside an event are normalized away.
        assert_eq!(
            sse_data("data: crlf\r\ndata: second"),
            Some("crlf\nsecond".to_owned())
        );
        // Non-data lines (comments, event: names) are ignored.
        assert_eq!(sse_data(": keepalive\nevent: response.done"), None);
        assert_eq!(sse_data(""), None);
        // The [DONE] sentinel is data the consumer loop skips; framing only
        // needs to surface it verbatim.
        assert_eq!(sse_data("data: [DONE]"), Some("[DONE]".to_owned()));
    }

    #[test]
    fn usage_limits_skip_provider_retry_and_keep_fallback_markers() {
        let weekly = r#"{"error":{"code":"usage_limit_reached","message":"Weekly usage limit reached","plan_type":"Plus"}}"#;
        assert!(terminal_rate_limit(weekly));
        assert!(!codex_retryable(StatusCode::TOO_MANY_REQUESTS, weekly));
        assert!(!codex_retryable(StatusCode::SERVICE_UNAVAILABLE, weekly));
        let message = format_codex_http_error(StatusCode::TOO_MANY_REQUESTS, weekly);
        assert!(message.contains("HTTP 429"), "{message}");
        assert!(message.contains("usage/quota exhausted"), "{message}");
        assert!(message.contains("Weekly usage limit reached"), "{message}");

        let transient =
            r#"{"error":{"code":"rate_limit_exceeded","message":"rate limit, retry later"}}"#;
        assert!(codex_retryable(StatusCode::TOO_MANY_REQUESTS, transient));
        assert!(codex_retryable(StatusCode::SERVICE_UNAVAILABLE, transient));
        let message = format_codex_http_error(StatusCode::TOO_MANY_REQUESTS, transient);
        assert!(message.contains("HTTP 429 rate limited"), "{message}");
        assert!(!message.contains("usage/quota exhausted"), "{message}");
    }
}
