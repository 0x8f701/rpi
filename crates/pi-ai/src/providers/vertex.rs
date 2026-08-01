use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use futures_util::FutureExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::common::{
    apply_provider_headers, apply_provider_request, client, consume_sse, error_body, fail,
    notify_response, send_with_retry,
};
use crate::*;

pub const GOOGLE_CLOUD_ACCESS_TOKEN_ENV: &str = "GOOGLE_CLOUD_ACCESS_TOKEN";

const VERTEX_API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default)]
pub struct VertexOptions {
    pub stream: StreamOptions,
    pub thinking: Option<VertexThinkingConfig>,
    pub tool_choice: Option<String>,
    pub project: Option<String>,
    pub location: Option<String>,
    /// A short-lived OAuth access token supplied explicitly by the caller.
    /// This provider never reads ADC files or invokes credential helpers.
    pub access_token: Option<String>,
}
impl std::fmt::Debug for VertexOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VertexOptions")
            .field("stream", &self.stream)
            .field("thinking", &self.thinking)
            .field("tool_choice", &self.tool_choice)
            .field("project", &self.project)
            .field("location", &self.location)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct VertexThinkingConfig {
    pub enabled: bool,
    pub budget: Option<i64>,
    pub level: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VertexAuthKind {
    ApiKey,
    AccessToken,
}

pub async fn stream_vertex(
    model: Model,
    context: Context,
    options: VertexOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let task_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        if let Err(error) =
            run_vertex_stream(&task_stream, &model, &context, &options, &mut output).await
        {
            let aborted = matches!(output.stop_reason, StopReason::Aborted)
                || options
                    .stream
                    .abort_signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
            let message = sanitize_vertex_error(&error.to_string(), &model, &options);
            fail(&task_stream, output, message, aborted).await;
        }
    });
    stream
}

pub async fn stream_simple_vertex(
    model: Model,
    context: Context,
    mut options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    options.stream.max_tokens = Some(clamp_max_tokens_to_context(
        &model,
        &context,
        options.stream.max_tokens.unwrap_or(model.max_tokens),
    ));
    let thinking = Some(match options.reasoning {
        None => VertexThinkingConfig::default(),
        Some(level) => enabled_thinking(&model, level, options.thinking_budgets.as_ref()),
    });
    stream_vertex(
        model,
        context,
        VertexOptions {
            stream: options.stream,
            thinking,
            ..VertexOptions::default()
        },
    )
    .await
}

async fn run_vertex_stream(
    stream: &AssistantMessageEventStream,
    model: &Model,
    context: &Context,
    options: &VertexOptions,
    output: &mut AssistantMessage,
) -> Result<()> {
    let auth_kind = resolve_auth_kind(model, options)?;
    let project_location = match auth_kind {
        VertexAuthKind::ApiKey => None,
        VertexAuthKind::AccessToken => {
            Some((resolve_project(options)?, resolve_location(options)?))
        }
    };
    let payload = apply_provider_request(
        build_vertex_payload(model, context, options)?,
        model,
        &options.stream,
    )
    .await?;
    let url = build_vertex_url(
        model,
        auth_kind,
        project_location
            .as_ref()
            .map(|(project, location)| (project.as_str(), location.as_str())),
    )?;
    let request_headers = apply_provider_headers(
        vertex_request_headers(model, options, auth_kind)?,
        model,
        &options.stream,
    )
    .await?;
    let http = client(&options.stream)?;
    let response = send_with_retry(&options.stream, || {
        http.post(&url)
            .headers(request_headers.clone())
            .json(&payload)
    })
    .await?;
    notify_response(&options.stream, &response, model).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            error_body("Google Vertex", response, &options.stream).await?
        ));
    }

    stream
        .push(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let mut state = VertexStreamState::default();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_stream = stream.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            event_stream.push(event).await;
        }
    });
    let parse_result = consume_sse(response, &options.stream, |_, data| {
        if data == "[DONE]" {
            return Ok(());
        }
        let Some(value) = parse_json_with_repair(data) else {
            return Ok(());
        };
        let chunk: VertexChunk = serde_json::from_value(value)?;
        if let Some(error) = chunk.error.as_ref() {
            let message = error.message.as_deref().unwrap_or("Vertex stream error");
            let status = error.status.as_deref().unwrap_or("UNKNOWN");
            bail!("{message} ({status})");
        }
        let mut events = Vec::new();
        state.apply_chunk(chunk, output, model, &mut events)?;
        for event in events {
            event_tx
                .send(event)
                .map_err(|_| anyhow!("Google Vertex event stream closed"))?;
        }
        Ok(())
    })
    .await;
    let mut events = Vec::new();
    state.end_current(output, &mut events);
    for event in events {
        event_tx
            .send(event)
            .map_err(|_| anyhow!("Google Vertex event stream closed"))?;
    }
    drop(event_tx);
    forwarder.await?;
    parse_result?;

    if output.stop_reason == StopReason::Pending {
        bail!("Google Vertex stream ended without a finish reason");
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        let reason = output.raw_stop_reason.as_deref().unwrap_or("unknown error");
        bail!("Provider stopped with: {reason}");
    }
    stream
        .push(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output.clone(),
        })
        .await;
    stream.end(Some(output.clone())).await;
    Ok(())
}

fn resolve_auth_kind(model: &Model, options: &VertexOptions) -> Result<VertexAuthKind> {
    let caller_authorization = valid_header_secret(&options.stream.headers, "authorization");
    let caller_api_key = valid_header_secret(&options.stream.headers, "x-goog-api-key");
    if caller_authorization.is_some() && caller_api_key.is_some() {
        bail!("Google Vertex caller headers contain both authorization and x-goog-api-key")
    }
    if caller_authorization.is_some() {
        return Ok(VertexAuthKind::AccessToken);
    }
    if explicit_access_token(options).is_some() {
        return Ok(VertexAuthKind::AccessToken);
    }
    if explicit_api_key(options).is_some() || caller_api_key.is_some() {
        return Ok(VertexAuthKind::ApiKey);
    }

    let model_authorization = model
        .headers
        .as_ref()
        .and_then(|headers| valid_header_secret(headers, "authorization"));
    let model_api_key = model
        .headers
        .as_ref()
        .and_then(|headers| valid_header_secret(headers, "x-goog-api-key"));
    if model_authorization.is_some() && model_api_key.is_some() {
        bail!("Google Vertex model headers contain both authorization and x-goog-api-key")
    }
    if model_authorization.is_some() {
        return Ok(VertexAuthKind::AccessToken);
    }
    if model_api_key.is_some() {
        return Ok(VertexAuthKind::ApiKey);
    }
    Err(anyhow!(
        "Google Vertex requires an explicit API key or access token; pass api_key/access_token, {} in StreamOptions.env, or an authorization header",
        GOOGLE_CLOUD_ACCESS_TOKEN_ENV
    ))
}

fn explicit_api_key(options: &VertexOptions) -> Option<&str> {
    options
        .stream
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| valid_secret(value))
}

fn explicit_access_token(options: &VertexOptions) -> Option<&str> {
    options
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| valid_secret(value))
        .or_else(|| {
            options
                .stream
                .env
                .get(GOOGLE_CLOUD_ACCESS_TOKEN_ENV)
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| valid_secret(value))
        })
}

fn valid_secret(value: &str) -> bool {
    !value.is_empty()
        && value != GCP_VERTEX_CREDENTIALS_MARKER
        && !(value.starts_with('<') && value.ends_with('>'))
}

fn find_header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(header, value)| header.eq_ignore_ascii_case(name).then_some(value.as_str()))
}
fn valid_header_secret<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    find_header(headers, name)
        .map(str::trim)
        .filter(|value| valid_secret(value))
}

fn resolve_project(options: &VertexOptions) -> Result<String> {
    let value = options
        .project
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| explicit_env(&options.stream.env, "GOOGLE_CLOUD_PROJECT"))
        .or_else(|| explicit_env(&options.stream.env, "GCLOUD_PROJECT"))
        .ok_or_else(|| {
            anyhow!(
                "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT in StreamOptions.env or pass project in options."
            )
        })?;
    validate_resource_component("project ID", value)?;
    Ok(value.to_owned())
}

fn resolve_location(options: &VertexOptions) -> Result<String> {
    let value = options
        .location
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| explicit_env(&options.stream.env, "GOOGLE_CLOUD_LOCATION"))
        .ok_or_else(|| {
            anyhow!(
                "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION in StreamOptions.env or pass location in options."
            )
        })?;
    validate_resource_component("location", value)?;
    Ok(value.to_owned())
}

fn explicit_env<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn validate_resource_component(label: &str, value: &str) -> Result<()> {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        bail!("Vertex AI {label} contains invalid characters")
    }
}

fn build_vertex_url(
    model: &Model,
    auth_kind: VertexAuthKind,
    project_location: Option<(&str, &str)>,
) -> Result<String> {
    let model_path = vertex_model_path(&model.id)?;
    let configured_base = model.base_url.trim();
    let custom_base = (!configured_base.is_empty() && !configured_base.contains("{location}"))
        .then_some(configured_base);

    let (base, collection_scope) = if let Some(custom) = custom_base {
        let base = custom.trim_end_matches('/');
        let resolved = if base_url_includes_api_version(base) {
            base.to_owned()
        } else {
            format!("{base}/{VERTEX_API_VERSION}")
        };
        (resolved, true)
    } else {
        match auth_kind {
            VertexAuthKind::ApiKey => (
                format!("https://aiplatform.googleapis.com/{VERTEX_API_VERSION}"),
                true,
            ),
            VertexAuthKind::AccessToken => {
                let (_, location) = project_location
                    .ok_or_else(|| anyhow!("Vertex AI project/location were not resolved"))?;
                let endpoint = match location {
                    "global" => "https://aiplatform.googleapis.com".to_owned(),
                    "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
                    _ => format!("https://{location}-aiplatform.googleapis.com"),
                };
                (format!("{endpoint}/{VERTEX_API_VERSION}"), false)
            }
        }
    };

    let resource = if !collection_scope && !model_path.starts_with("projects/") {
        let (project, location) = project_location
            .ok_or_else(|| anyhow!("Vertex AI project/location were not resolved"))?;
        format!("projects/{project}/locations/{location}/{model_path}")
    } else {
        model_path
    };
    Ok(format!("{base}/{resource}:streamGenerateContent?alt=sse"))
}

fn vertex_model_path(model: &str) -> Result<String> {
    let model = model.trim();
    if model.is_empty() {
        bail!("Vertex AI model is required")
    }
    if model.contains("..") || model.contains('?') || model.contains('&') || model.contains('#') {
        bail!("Vertex AI model contains invalid characters")
    }
    if model.starts_with("publishers/")
        || model.starts_with("projects/")
        || model.starts_with("models/")
    {
        return Ok(model.to_owned());
    }
    if let Some((publisher, name)) = model.split_once('/') {
        if publisher.is_empty() || name.is_empty() || name.contains('/') {
            bail!("Vertex AI model contains an invalid publisher/model resource")
        }
        return Ok(format!("publishers/{publisher}/models/{name}"));
    }
    Ok(format!("publishers/google/models/{model}"))
}

fn base_url_includes_api_version(base_url: &str) -> bool {
    base_url.split(['/', '?', '#']).any(is_api_version_segment)
}

fn is_api_version_segment(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('v') else {
        return false;
    };
    let digit_count = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return false;
    }
    let suffix = &rest[digit_count..];
    suffix.is_empty()
        || suffix
            .strip_prefix("beta")
            .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
}

fn vertex_request_headers(
    model: &Model,
    options: &VertexOptions,
    auth_kind: VertexAuthKind,
) -> Result<HeaderMap> {
    let mut headers = merge_provider_attribution_headers(
        model,
        options.stream.session_id.as_deref(),
        false,
        &HashMap::new(),
    )
    .unwrap_or_default();
    insert_header_case_insensitive(&mut headers, "content-type", "application/json");

    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header_case_insensitive(&mut headers, name, value);
        }
    }
    match auth_kind {
        VertexAuthKind::ApiKey => {
            if let Some(api_key) = explicit_api_key(options) {
                insert_header_case_insensitive(&mut headers, "x-goog-api-key", api_key);
                remove_header_case_insensitive(&mut headers, "authorization");
            }
        }
        VertexAuthKind::AccessToken => {
            if let Some(access_token) = explicit_access_token(options) {
                insert_header_case_insensitive(
                    &mut headers,
                    "authorization",
                    &format!("Bearer {access_token}"),
                );
                remove_header_case_insensitive(&mut headers, "x-goog-api-key");
            }
        }
    }
    for (name, value) in &options.stream.headers {
        insert_header_case_insensitive(&mut headers, name, value);
    }
    match auth_kind {
        VertexAuthKind::ApiKey => remove_header_case_insensitive(&mut headers, "authorization"),
        VertexAuthKind::AccessToken => {
            remove_header_case_insensitive(&mut headers, "x-goog-api-key")
        }
    }

    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            anyhow!(
                "Invalid Google Vertex request header name: {}",
                name.to_ascii_lowercase()
            )
        })?;
        let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
            anyhow!(
                "Invalid Google Vertex request header value for {}",
                name.to_ascii_lowercase()
            )
        })?;
        if is_sensitive_header(&header_name) {
            header_value.set_sensitive(true);
        }
        map.insert(header_name, header_value);
    }
    Ok(map)
}

fn insert_header_case_insensitive(headers: &mut HashMap<String, String>, name: &str, value: &str) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
    headers.insert(name.to_owned(), value.to_owned());
}
fn remove_header_case_insensitive(headers: &mut HashMap<String, String>, name: &str) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
}

fn is_sensitive_header(name: &HeaderName) -> bool {
    name.as_str().eq_ignore_ascii_case("authorization")
        || name.as_str().eq_ignore_ascii_case("x-goog-api-key")
        || name.as_str().eq_ignore_ascii_case("x-api-key")
        || name.as_str().eq_ignore_ascii_case("cf-aig-authorization")
}

fn sanitize_vertex_error(message: &str, model: &Model, options: &VertexOptions) -> String {
    let mut secrets = Vec::new();
    if let Some(value) = options
        .stream
        .api_key
        .as_deref()
        .filter(|value| valid_secret(value.trim()))
    {
        secrets.push(value.trim().to_owned());
    }
    if let Some(value) = options
        .access_token
        .as_deref()
        .filter(|value| valid_secret(value.trim()))
    {
        secrets.push(value.trim().to_owned());
    }
    if let Some(value) = options
        .stream
        .env
        .get(GOOGLE_CLOUD_ACCESS_TOKEN_ENV)
        .filter(|value| valid_secret(value.trim()))
    {
        secrets.push(value.trim().to_owned());
    }
    if let Some(headers) = &model.headers {
        collect_sensitive_header_values(headers, &mut secrets);
    }
    collect_sensitive_header_values(&options.stream.headers, &mut secrets);
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
        .into_iter()
        .fold(message.to_owned(), |sanitized, secret| {
            if secret.is_empty() {
                sanitized
            } else {
                let sanitized = sanitized.replace(&format!("Bearer {secret}"), "Bearer [REDACTED]");
                sanitized.replace(&secret, "[REDACTED]")
            }
        })
}

fn collect_sensitive_header_values(headers: &HashMap<String, String>, secrets: &mut Vec<String>) {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("x-goog-api-key")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("cf-aig-authorization")
        {
            let value = value.trim();
            if !value.is_empty() {
                secrets.push(value.to_owned());
                if let Some(token) = value
                    .strip_prefix("Bearer ")
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                {
                    secrets.push(token.to_owned());
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct VertexStreamState {
    current: Option<(usize, StreamingBlock)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingBlock {
    Text,
    Thinking,
}

impl VertexStreamState {
    fn apply_chunk(
        &mut self,
        chunk: VertexChunk,
        output: &mut AssistantMessage,
        model: &Model,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> Result<()> {
        if output.response_id.is_none() {
            output.response_id = chunk.response_id;
        }
        if let Some(candidate) = chunk.candidates.into_iter().next() {
            for part in candidate.content.parts {
                self.apply_part(part, output, events)?;
            }
            if let Some(reason) = candidate.finish_reason {
                output.raw_stop_reason = Some(reason.clone());
                output.stop_reason = map_stop_reason(&reason)?;
                if output
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall(_)))
                {
                    output.stop_reason = StopReason::ToolUse;
                }
            }
        }
        if let Some(usage) = chunk.usage_metadata {
            output.usage = Usage {
                input: usage.prompt_token_count - usage.cached_content_token_count,
                output: usage.candidates_token_count + usage.thoughts_token_count,
                cache_read: usage.cached_content_token_count,
                cache_write: 0,
                reasoning: usage.thoughts_token_count,
                total_tokens: usage.total_token_count,
                ..Usage::default()
            };
            calculate_cost(model, &mut output.usage);
        }
        Ok(())
    }

    fn apply_part(
        &mut self,
        part: VertexPart,
        output: &mut AssistantMessage,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> Result<()> {
        let VertexPart {
            text,
            thought,
            thought_signature,
            function_call,
        } = part;
        let signature_for_call = thought_signature.clone();
        if let Some(text) = text {
            let kind = if thought {
                StreamingBlock::Thinking
            } else {
                StreamingBlock::Text
            };
            let index = self.ensure_block(kind, output, events);
            match &mut output.content[index] {
                ContentBlock::Text {
                    text: content,
                    text_signature,
                } => {
                    content.push_str(&text);
                    if thought_signature
                        .as_ref()
                        .is_some_and(|signature| !signature.is_empty())
                    {
                        *text_signature = thought_signature.clone();
                    }
                    events.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: text,
                        partial: output.clone(),
                    });
                }
                ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    ..
                } => {
                    thinking.push_str(&text);
                    if thought_signature
                        .as_ref()
                        .is_some_and(|signature| !signature.is_empty())
                    {
                        *thinking_signature = thought_signature.clone();
                    }
                    events.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: text,
                        partial: output.clone(),
                    });
                }
                _ => unreachable!("streaming block content kind is fixed"),
            }
        }
        if let Some(call) = function_call {
            self.end_current(output, events);
            let id = unique_tool_call_id(&call.id, &call.name, &output.content);
            let tool_call = ToolCall {
                id,
                name: call.name,
                arguments: call.args,
                thought_signature: signature_for_call.filter(|signature| !signature.is_empty()),
            };
            let index = output.content.len();
            output
                .content
                .push(ContentBlock::ToolCall(tool_call.clone()));
            events.push(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial: output.clone(),
            });
            events.push(AssistantMessageEvent::ToolCallDelta {
                content_index: index,
                delta: serde_json::to_string(&tool_call.arguments)?,
                partial: output.clone(),
            });
            events.push(AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call,
                partial: output.clone(),
            });
        }
        Ok(())
    }

    fn ensure_block(
        &mut self,
        kind: StreamingBlock,
        output: &mut AssistantMessage,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> usize {
        if let Some((index, current_kind)) = self.current {
            if current_kind == kind {
                return index;
            }
            self.end_current(output, events);
        }
        let index = output.content.len();
        match kind {
            StreamingBlock::Text => {
                output.content.push(ContentBlock::text(""));
                events.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: output.clone(),
                });
            }
            StreamingBlock::Thinking => {
                output.content.push(ContentBlock::thinking(""));
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: output.clone(),
                });
            }
        }
        self.current = Some((index, kind));
        index
    }

    fn end_current(&mut self, output: &AssistantMessage, events: &mut Vec<AssistantMessageEvent>) {
        let Some((index, kind)) = self.current.take() else {
            return;
        };
        match (&output.content[index], kind) {
            (ContentBlock::Text { text, .. }, StreamingBlock::Text) => {
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.clone(),
                    partial: output.clone(),
                });
            }
            (ContentBlock::Thinking { thinking, .. }, StreamingBlock::Thinking) => {
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.clone(),
                    partial: output.clone(),
                });
            }
            _ => unreachable!("streaming block content kind is fixed"),
        }
    }
}

fn build_vertex_payload(
    model: &Model,
    context: &Context,
    options: &VertexOptions,
) -> Result<Value> {
    let mut payload = Map::new();
    payload.insert(
        "contents".into(),
        Value::Array(vertex_contents(model, context)),
    );

    let mut generation = Map::new();
    if let Some(temperature) = options.stream.temperature {
        generation.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.stream.max_tokens {
        generation.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if model.reasoning {
        if let Some(thinking) = &options.thinking {
            generation.insert(
                "thinkingConfig".into(),
                if thinking.enabled {
                    let mut config =
                        Map::from_iter([("includeThoughts".into(), Value::Bool(true))]);
                    if let Some(level) = &thinking.level {
                        config.insert("thinkingLevel".into(), Value::String(level.clone()));
                    } else if let Some(budget) = thinking.budget {
                        config.insert("thinkingBudget".into(), json!(budget));
                    }
                    Value::Object(config)
                } else {
                    disabled_thinking_config(&model.id)
                },
            );
        }
    }
    payload.insert("generationConfig".into(), Value::Object(generation));

    if !context.system_prompt.is_empty() {
        payload.insert(
            "systemInstruction".into(),
            json!({"role": "user", "parts": [{"text": context.system_prompt}]}),
        );
    }
    if !context.tools.is_empty() {
        let declarations = context
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })
            })
            .collect::<Vec<_>>();
        payload.insert(
            "tools".into(),
            json!([{"functionDeclarations": declarations}]),
        );
        let mode = resolve_vertex_function_calling_mode(
            &context.tools,
            options.tool_choice.as_deref(),
            supports_vertex_strict_tool_sampling(&model.id),
        )?;
        if let Some(mode) = mode {
            payload.insert(
                "toolConfig".into(),
                json!({"functionCallingConfig": {"mode": mode}}),
            );
        }
    }
    Ok(Value::Object(payload))
}

fn vertex_contents(model: &Model, context: &Context) -> Vec<Value> {
    let messages = transform_messages(&context.messages, model, |id, target, _source| {
        if requires_tool_call_id(&target.id) {
            normalize_tool_call_id(id)
        } else {
            id.to_owned()
        }
    });
    let mut contents = Vec::new();
    for message in &messages {
        match message {
            Message::User(message) => {
                let parts = content_parts(&message.content, false, model);
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Message::Assistant(message) => {
                let same_model = message.provider == model.provider && message.model == model.id;
                let parts = content_parts(&message.content, same_model, model);
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Message::ToolResult(result) => {
                let mut text = Vec::new();
                let mut images = Vec::new();
                for content in &result.content {
                    match content {
                        ContentBlock::Text { text: value, .. } => text.push(value.as_str()),
                        ContentBlock::Image { data, mime_type }
                            if model.input.iter().any(|input| input == "image") =>
                        {
                            images
                                .push(json!({"inlineData": {"mimeType": mime_type, "data": data}}));
                        }
                        _ => {}
                    }
                }
                let response_value = if !text.is_empty() {
                    text.join("\n")
                } else if !images.is_empty() {
                    "(see attached image)".into()
                } else {
                    String::new()
                };
                let response_key = if result.is_error { "error" } else { "output" };
                let mut response = Map::from_iter([
                    ("name".into(), Value::String(result.tool_name.clone())),
                    (
                        "response".into(),
                        Value::Object(Map::from_iter([(
                            response_key.into(),
                            Value::String(response_value),
                        )])),
                    ),
                ]);
                if requires_tool_call_id(&model.id) {
                    response.insert("id".into(), Value::String(result.tool_call_id.clone()));
                }
                if !images.is_empty() && supports_nested_tool_images(&model.id) {
                    response.insert("parts".into(), Value::Array(images.clone()));
                }
                let part = json!({"functionResponse": response});
                if let Some(last) = contents.last_mut().and_then(Value::as_object_mut) {
                    let can_merge = last.get("role").and_then(Value::as_str) == Some("user")
                        && last
                            .get("parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts
                                    .iter()
                                    .any(|part| part.get("functionResponse").is_some())
                            });
                    if can_merge {
                        if let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut) {
                            parts.push(part);
                        }
                    } else {
                        contents.push(json!({"role": "user", "parts": [part]}));
                    }
                } else {
                    contents.push(json!({"role": "user", "parts": [part]}));
                }
                if !images.is_empty() && !supports_nested_tool_images(&model.id) {
                    let mut parts = vec![json!({"text": "Tool result image:"})];
                    parts.extend(images);
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("session messages are projected before Vertex conversion")
            }
        }
    }
    contents
}

fn content_parts(content: &[ContentBlock], same_model: bool, model: &Model) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text {
                text,
                text_signature,
            } => {
                let signature = valid_signature(same_model, text_signature.as_deref());
                if text.trim().is_empty() && signature.is_none() {
                    None
                } else {
                    let mut part = Map::from_iter([("text".into(), Value::String(text.clone()))]);
                    if let Some(signature) = signature {
                        part.insert("thoughtSignature".into(), Value::String(signature.into()));
                    }
                    Some(Value::Object(part))
                }
            }
            ContentBlock::Image { data, mime_type } => {
                Some(json!({"inlineData": {"mimeType": mime_type, "data": data}}))
            }
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } if same_model => {
                let signature = valid_signature(true, thinking_signature.as_deref());
                if thinking.trim().is_empty() && signature.is_none() {
                    None
                } else {
                    let mut part = Map::from_iter([
                        ("thought".into(), Value::Bool(true)),
                        ("text".into(), Value::String(thinking.clone())),
                    ]);
                    if let Some(signature) = signature {
                        part.insert("thoughtSignature".into(), Value::String(signature.into()));
                    }
                    Some(Value::Object(part))
                }
            }
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                Some(json!({"text": thinking}))
            }
            ContentBlock::ToolCall(call) => {
                let mut function = Map::from_iter([
                    ("name".into(), Value::String(call.name.clone())),
                    ("args".into(), call.arguments.clone()),
                ]);
                if requires_tool_call_id(&model.id) {
                    function.insert("id".into(), Value::String(call.id.clone()));
                }
                let mut part = Map::from_iter([("functionCall".into(), Value::Object(function))]);
                if let Some(signature) =
                    valid_signature(same_model, call.thought_signature.as_deref())
                {
                    part.insert("thoughtSignature".into(), Value::String(signature.into()));
                }
                Some(Value::Object(part))
            }
            _ => None,
        })
        .collect()
}

fn enabled_thinking(
    model: &Model,
    reasoning: ThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> VertexThinkingConfig {
    let requested = match reasoning {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    };
    let mut effort = clamp_thinking_level(model, requested);
    if effort == "off" {
        effort = "high";
    }
    if is_gemini_3(&model.id) {
        VertexThinkingConfig {
            enabled: true,
            level: vertex_thinking_level(effort, &model.id).map(str::to_owned),
            budget: None,
        }
    } else {
        VertexThinkingConfig {
            enabled: true,
            budget: vertex_budget(&model.id, effort, custom),
            level: None,
        }
    }
}

fn vertex_thinking_level<'a>(effort: &'a str, model_id: &str) -> Option<&'a str> {
    if is_gemini_3_pro(model_id) {
        return match effort {
            "minimal" | "low" => Some("LOW"),
            "medium" | "high" => Some("HIGH"),
            _ => None,
        };
    }
    match effort {
        "minimal" => Some("MINIMAL"),
        "low" => Some("LOW"),
        "medium" => Some("MEDIUM"),
        "high" => Some("HIGH"),
        _ => None,
    }
}

fn vertex_budget(model_id: &str, effort: &str, custom: Option<&ThinkingBudgets>) -> Option<i64> {
    if let Some(custom) = custom {
        let value = match effort {
            "minimal" => custom.minimal,
            "low" => custom.low,
            "medium" => custom.medium,
            "high" => custom.high,
            _ => None,
        };
        if value.is_some() {
            return value;
        }
    }
    if model_id.contains("2.5-pro") {
        return match effort {
            "minimal" => Some(128),
            "low" => Some(2048),
            "medium" => Some(8192),
            "high" => Some(32768),
            _ => None,
        };
    }
    if model_id.contains("2.5-flash") {
        return match effort {
            "minimal" => Some(128),
            "low" => Some(2048),
            "medium" => Some(8192),
            "high" => Some(24576),
            _ => None,
        };
    }
    Some(-1)
}

fn disabled_thinking_config(model_id: &str) -> Value {
    if is_gemini_3_pro(model_id) {
        json!({"thinkingLevel": "LOW"})
    } else if is_gemini_3_flash(model_id) {
        json!({"thinkingLevel": "MINIMAL"})
    } else {
        json!({"thinkingBudget": 0})
    }
}

fn is_gemini_3(model_id: &str) -> bool {
    is_gemini_3_pro(model_id) || is_gemini_3_flash(model_id)
}

fn gemini_3_family(model_id: &str) -> Option<&str> {
    let id = model_id.strip_prefix("gemini-3")?;
    if let Some(rest) = id.strip_prefix('-') {
        return Some(rest);
    }
    let versioned = id.strip_prefix('.')?;
    versioned.split_once('-').map(|(_, family)| family)
}

fn is_gemini_3_pro(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    gemini_3_family(&id).is_some_and(|family| family.starts_with("pro"))
}

fn is_gemini_3_flash(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    matches!(
        id.as_str(),
        "gemini-flash-latest" | "gemini-flash-lite-latest"
    ) || gemini_3_family(&id).is_some_and(|family| family.starts_with("flash"))
}

fn gemini_major(model_id: &str) -> Option<u32> {
    let id = model_id.to_ascii_lowercase();
    let suffix = id
        .strip_prefix("gemini-live-")
        .or_else(|| id.strip_prefix("gemini-"))?;
    suffix.split('-').next()?.split('.').next()?.parse().ok()
}

fn supports_vertex_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major(model_id).is_some_and(|major| major >= 3)
}

fn resolve_json_schema_strict_sampling(tool: &Tool, supports_strict: bool) -> Result<bool> {
    let strictness = match &tool.constrained_sampling {
        Some(ConstrainedSampling::JsonSchema { strict }) => *strict,
        _ => return Ok(false),
    };
    if supports_strict {
        return Ok(true);
    }
    if strictness == ConstrainedSamplingStrictness::Require {
        bail!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        );
    }
    Ok(false)
}

fn resolve_vertex_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<&'static str>> {
    let mut use_strict_mode = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict_mode)? {
            use_strict_mode = true;
            break;
        }
    }
    let choice = tool_choice.map(str::trim).filter(|value| !value.is_empty());
    if let Some(choice @ ("none" | "any")) = choice {
        return Ok(Some(map_tool_choice(choice)));
    }
    if use_strict_mode {
        return Ok(Some("VALIDATED"));
    }
    if let Some(choice) = choice {
        return Ok(Some(map_tool_choice(choice)));
    }
    Ok(None)
}

fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "auto" => "AUTO",
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

fn supports_nested_tool_images(model_id: &str) -> bool {
    gemini_major(model_id).is_none_or(|major| major >= 3)
}

fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-") || model_id.starts_with("gpt-oss-")
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn valid_signature(same_model: bool, signature: Option<&str>) -> Option<&str> {
    let signature = signature.filter(|_| same_model)?;
    let padding = signature.trim_end_matches('=').len();
    let padding_len = signature.len() - padding;
    (signature.len() % 4 == 0
        && padding_len <= 2
        && !signature.is_empty()
        && signature[..padding]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')))
    .then_some(signature)
}

fn unique_tool_call_id(provided: &str, name: &str, content: &[ContentBlock]) -> String {
    let duplicate = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(call) if call.id == provided));
    if !provided.is_empty() && !duplicate {
        return provided.into();
    }
    let count = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{name}_{}_{}", now_millis(), count)
}

fn map_stop_reason(reason: &str) -> Result<StopReason> {
    match reason {
        "STOP" => Ok(StopReason::Stop),
        "MAX_TOKENS" => Ok(StopReason::Length),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "NO_IMAGE" => Ok(StopReason::Error),
        _ => bail!("Unhandled stop reason: {reason}"),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VertexChunk {
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    candidates: Vec<VertexCandidate>,
    usage_metadata: Option<VertexUsage>,
    error: Option<VertexError>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VertexCandidate {
    #[serde(default)]
    content: VertexCandidateContent,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct VertexCandidateContent {
    #[serde(default)]
    parts: Vec<VertexPart>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VertexPart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    thought_signature: Option<String>,
    function_call: Option<VertexFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct VertexFunctionCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default = "empty_arguments")]
    args: Value,
}

fn empty_arguments() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VertexUsage {
    #[serde(default)]
    prompt_token_count: i64,
    #[serde(default)]
    candidates_token_count: i64,
    #[serde(default)]
    cached_content_token_count: i64,
    #[serde(default)]
    thoughts_token_count: i64,
    #[serde(default)]
    total_token_count: i64,
}

#[derive(Debug, Deserialize, Default)]
struct VertexError {
    status: Option<String>,
    message: Option<String>,
}

pub fn register_google_vertex() {
    let native: StreamFn = std::sync::Arc::new(|model, context, options| {
        async move {
            stream_vertex(
                model,
                context,
                VertexOptions {
                    stream: options,
                    ..VertexOptions::default()
                },
            )
            .await
        }
        .boxed()
    });
    let simple: SimpleStreamFn = std::sync::Arc::new(|model, context, options| {
        async move { stream_simple_vertex(model, context, options).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_GOOGLE_VERTEX.into(),
            stream: native,
            stream_simple: simple,
        },
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn vertex_model(base_url: String) -> Model {
        Model {
            id: "gemini-3-flash-preview".into(),
            name: "Gemini Vertex".into(),
            api: API_GOOGLE_VERTEX.into(),
            provider: "google-vertex".into(),
            base_url,
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.25,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            ..Model::default()
        }
    }

    #[test]
    fn vertex_options_debug_redacts_access_token() {
        let secret = "debug-must-not-leak";
        let options = VertexOptions {
            access_token: Some(secret.into()),
            ..VertexOptions::default()
        };
        let debug = format!("{options:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(secret));
    }

    fn vertex_options() -> VertexOptions {
        VertexOptions {
            stream: StreamOptions {
                env: HashMap::from([
                    (
                        GOOGLE_CLOUD_ACCESS_TOKEN_ENV.into(),
                        "adc-access-token".into(),
                    ),
                    ("GOOGLE_CLOUD_PROJECT".into(), "test-project".into()),
                    ("GOOGLE_CLOUD_LOCATION".into(), "us-central1".into()),
                ]),
                ..StreamOptions::default()
            },
            ..VertexOptions::default()
        }
    }

    #[test]
    fn builds_vertex_payload_with_messages_tools_images_and_thinking() {
        let model = vertex_model("https://{location}-aiplatform.googleapis.com".into());
        let context = Context {
            system_prompt: "System".into(),
            messages: vec![
                Message::User(UserMessage {
                    content: vec![
                        ContentBlock::text("look"),
                        ContentBlock::Image {
                            data: "aW1n".into(),
                            mime_type: "image/png".into(),
                        },
                    ],
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "inspect".into(),
                        arguments: json!({"target": "image"}),
                        thought_signature: Some("YWJjZA==".into()),
                    })],
                    api: API_GOOGLE_VERTEX.into(),
                    provider: "google-vertex".into(),
                    model: model.id.clone(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: Some("STOP".into()),
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call-1".into(),
                    tool_name: "inspect".into(),
                    content: vec![ContentBlock::text("ok")],
                    usage: None,
                    details: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    timestamp: 3,
                }),
            ],
            tools: vec![ToolDefinition {
                name: "inspect".into(),
                description: "Inspect an image".into(),
                parameters: Schema::object(
                    HashMap::from([("target".into(), Schema::string())]),
                    vec!["target".into()],
                ),
                constrained_sampling: None,
            }],
        };
        let payload = build_vertex_payload(
            &model,
            &context,
            &VertexOptions {
                stream: StreamOptions {
                    temperature: Some(0.2),
                    max_tokens: Some(512),
                    ..StreamOptions::default()
                },
                thinking: Some(VertexThinkingConfig {
                    enabled: true,
                    level: Some("HIGH".into()),
                    budget: None,
                }),
                ..VertexOptions::default()
            },
        )
        .expect("payload");

        assert_eq!(payload["systemInstruction"]["parts"][0]["text"], "System");
        assert_eq!(
            payload["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            payload["contents"][1]["parts"][0]["functionCall"]["name"],
            "inspect"
        );
        assert_eq!(
            payload["contents"][1]["parts"][0]["thoughtSignature"],
            "YWJjZA=="
        );
        assert_eq!(
            payload["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
            "ok"
        );
        assert_eq!(
            payload["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"][0],
            "target"
        );
        assert_eq!(
            payload["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
    }

    #[test]
    fn derives_standard_and_custom_vertex_urls() {
        let model = vertex_model("https://{location}-aiplatform.googleapis.com".into());
        assert_eq!(
            build_vertex_url(
                &model,
                VertexAuthKind::AccessToken,
                Some(("project-one", "us-central1")),
            )
            .expect("access-token url"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/project-one/locations/us-central1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            build_vertex_url(&model, VertexAuthKind::ApiKey, None).expect("api key url"),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );

        let custom = vertex_model("https://proxy.example.test/google/v1".into());
        assert_eq!(
            build_vertex_url(
                &custom,
                VertexAuthKind::AccessToken,
                Some(("project-one", "eu")),
            )
            .expect("custom url"),
            "https://proxy.example.test/google/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn api_key_does_not_require_project_or_location() {
        let model = vertex_model("https://{location}-aiplatform.googleapis.com".into());
        let options = VertexOptions {
            stream: StreamOptions {
                api_key: Some("vertex-api-key".into()),
                ..StreamOptions::default()
            },
            ..VertexOptions::default()
        };
        let auth = resolve_auth_kind(&model, &options).expect("auth");
        assert_eq!(auth, VertexAuthKind::ApiKey);
        assert_eq!(
            vertex_request_headers(&model, &options, auth)
                .expect("headers")
                .get("x-goog-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("vertex-api-key")
        );
    }

    #[test]
    fn missing_project_and_location_are_specific_and_do_not_echo_token() {
        let secret = "adc-super-secret";
        let mut options = VertexOptions {
            access_token: Some(secret.into()),
            ..VertexOptions::default()
        };
        let project_error = resolve_project(&options)
            .expect_err("missing project")
            .to_string();
        assert!(project_error.contains("project ID"));
        assert!(!project_error.contains(secret));

        options.project = Some("project-one".into());
        let location_error = resolve_location(&options)
            .expect_err("missing location")
            .to_string();
        assert!(location_error.contains("location"));
        assert!(!location_error.contains(secret));
    }

    #[test]
    fn caller_authorization_wins_case_insensitively() {
        let model = Model {
            headers: Some(HashMap::from([
                ("Authorization".into(), "Bearer model-token".into()),
                ("X-Goog-Api-Key".into(), "model-key".into()),
            ])),
            ..vertex_model(String::new())
        };
        let options = VertexOptions {
            access_token: Some("option-token".into()),
            stream: StreamOptions {
                api_key: Some("default-key".into()),
                headers: HashMap::from([("AUTHORIZATION".into(), "Bearer caller-token".into())]),
                ..StreamOptions::default()
            },
            ..VertexOptions::default()
        };
        let auth = resolve_auth_kind(&model, &options).expect("auth");
        assert_eq!(auth, VertexAuthKind::AccessToken);
        let headers = vertex_request_headers(&model, &options, auth).expect("headers");
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(headers["authorization"], "Bearer caller-token");
        assert!(headers["authorization"].is_sensitive());
        assert!(!headers.contains_key("x-goog-api-key"));
    }
    #[test]
    fn rejects_ambiguous_auth_headers_within_one_scope() {
        let model = vertex_model(String::new());
        let caller = VertexOptions {
            stream: StreamOptions {
                headers: HashMap::from([
                    ("Authorization".into(), "Bearer caller-token".into()),
                    ("X-Goog-Api-Key".into(), "caller-key".into()),
                ]),
                ..StreamOptions::default()
            },
            ..VertexOptions::default()
        };
        let error = resolve_auth_kind(&model, &caller).expect_err("ambiguous caller auth");
        assert!(error.to_string().contains("caller headers"));
        assert!(!error.to_string().contains("caller-token"));
        assert!(!error.to_string().contains("caller-key"));

        let model = Model {
            headers: Some(HashMap::from([
                ("Authorization".into(), "Bearer model-token".into()),
                ("X-Goog-Api-Key".into(), "model-key".into()),
            ])),
            ..vertex_model(String::new())
        };
        let error =
            resolve_auth_kind(&model, &VertexOptions::default()).expect_err("ambiguous model auth");
        assert!(error.to_string().contains("model headers"));
        assert!(!error.to_string().contains("model-token"));
        assert!(!error.to_string().contains("model-key"));
    }

    #[test]
    fn credential_markers_are_never_treated_as_api_keys() {
        let model = vertex_model(String::new());
        for marker in ["<authenticated>", GCP_VERTEX_CREDENTIALS_MARKER] {
            let options = VertexOptions {
                stream: StreamOptions {
                    api_key: Some(marker.into()),
                    ..StreamOptions::default()
                },
                ..VertexOptions::default()
            };
            let error = resolve_auth_kind(&model, &options).expect_err("marker is not auth");
            assert!(!error.to_string().contains(marker));
            let headers =
                vertex_request_headers(&model, &options, VertexAuthKind::ApiKey).expect("headers");
            assert!(!headers.contains_key("x-goog-api-key"));
        }
    }

    #[test]
    fn sanitizes_tokens_from_errors_and_header_values() {
        let model = Model {
            headers: Some(HashMap::from([(
                "X-Goog-Api-Key".into(),
                "model-secret".into(),
            )])),
            ..vertex_model(String::new())
        };
        let options = VertexOptions {
            access_token: Some("option-token".into()),
            stream: StreamOptions {
                api_key: Some("api-secret".into()),
                env: HashMap::from([(GOOGLE_CLOUD_ACCESS_TOKEN_ENV.into(), "env-token".into())]),
                headers: HashMap::from([("Authorization".into(), "Bearer caller-token".into())]),
                ..StreamOptions::default()
            },
            ..VertexOptions::default()
        };
        let sanitized = sanitize_vertex_error(
            "api-secret option-token env-token model-secret Bearer caller-token",
            &model,
            &options,
        );
        for secret in [
            "api-secret",
            "option-token",
            "env-token",
            "model-secret",
            "caller-token",
        ] {
            assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
        }
        assert!(sanitized.contains("[REDACTED]"));
    }

    async fn capture_one_request(
        response_status: &str,
        response_body: String,
    ) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let captured = Arc::new(Mutex::new(String::new()));
        let request = captured.clone();
        let response_status = response_status.to_owned();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = vec![0; 8192];
            let mut received = Vec::new();
            loop {
                let read = socket.read(&mut buffer).await.expect("read");
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&received[..header_end + 4]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            *request.lock().await = String::from_utf8(received).expect("request utf8");
            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });
        (format!("http://{address}"), captured)
    }

    #[tokio::test]
    async fn retries_retryable_vertex_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buffer = vec![0; 8192];
                let mut received = Vec::new();
                loop {
                    let read = socket.read(&mut buffer).await.expect("read");
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&buffer[..read]);
                    if let Some(header_end) =
                        received.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&received[..header_end + 4]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::trim)
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if received.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                server_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    socket
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nretry-after-ms: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .expect("first response");
                } else {
                    let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"retried\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("second response");
                }
            }
        });

        let model = vertex_model(format!("http://{address}/v1"));
        let mut options = vertex_options();
        options.stream.max_retries = 1;
        let stream = stream_vertex(model, Context::default(), options).await;
        while stream.next().await.is_some() {}
        let result = stream.result().await.expect("result");
        assert_eq!(result.text(), "retried");
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn streams_vertex_sse_thinking_tool_usage_and_auth_fixture() {
        let body = concat!(
            "data: {\"responseId\":\"resp-v\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"plan\",\"thought\":true,\"thoughtSignature\":\"YWJjZA==\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call-1\",\"name\":\"lookup\",\"args\":{\"q\":\"rust\"}},\"thoughtSignature\":\"ZWZnaA==\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"cachedContentTokenCount\":3,\"candidatesTokenCount\":4,\"thoughtsTokenCount\":2,\"totalTokenCount\":16}}\n\n"
        )
        .to_owned();
        let (base_url, captured) = capture_one_request("200 OK", body).await;
        let model = vertex_model(format!("{base_url}/v1"));
        let context = Context {
            system_prompt: "Be concise".into(),
            messages: vec![Message::user_text("Use lookup", 1)],
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: "Lookup".into(),
                parameters: Schema::object(HashMap::new(), Vec::new()),
                constrained_sampling: None,
            }],
        };
        let stream = stream_vertex(model, context, vertex_options()).await;
        let mut saw_start = false;
        let mut thinking = String::new();
        let mut tool_call = None;
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::Start { .. } => saw_start = true,
                AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(&delta),
                AssistantMessageEvent::ToolCallEnd {
                    tool_call: call, ..
                } => tool_call = Some(call),
                _ => {}
            }
        }
        let result = stream.result().await.expect("result");
        assert!(saw_start);
        assert_eq!(thinking, "plan");
        let call = tool_call.expect("tool call");
        assert_eq!(call.id, "call-1");
        assert_eq!(call.name, "lookup");
        assert_eq!(call.arguments, json!({"q": "rust"}));
        assert_eq!(call.thought_signature.as_deref(), Some("ZWZnaA=="));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.response_id.as_deref(), Some("resp-v"));
        assert_eq!(result.usage.input, 7);
        assert_eq!(result.usage.cache_read, 3);
        assert_eq!(result.usage.output, 6);
        assert_eq!(result.usage.reasoning, 2);
        assert_eq!(result.usage.total_tokens, 16);
        assert!((result.usage.cost.input - 0.000007).abs() < f64::EPSILON);
        assert!((result.usage.cost.cache_read - 0.00000075).abs() < f64::EPSILON);
        assert!((result.usage.cost.output - 0.000012).abs() < f64::EPSILON);

        let request = captured.lock().await;
        assert!(request.starts_with(
            "POST /v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse HTTP/1.1\r\n"
        ));
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer adc-access-token"))
        );
        assert!(!request.to_ascii_lowercase().contains("x-goog-api-key:"));
        assert!(request.contains("\"generationConfig\""));
        assert!(request.contains("\"systemInstruction\""));
    }

    #[tokio::test]
    async fn sanitizes_secret_echoed_by_provider_error() {
        let secret = "never-print-this-token";
        let response = format!(
            "{{\"error\":{{\"message\":\"bad token {secret}\",\"code\":\"UNAUTHENTICATED\"}}}}"
        );
        let (base_url, _) = capture_one_request("401 Unauthorized", response).await;
        let model = vertex_model(format!("{base_url}/v1"));
        let mut options = vertex_options();
        options.access_token = Some(secret.into());
        options.stream.env.remove(GOOGLE_CLOUD_ACCESS_TOKEN_ENV);
        let stream = stream_vertex(model, Context::default(), options).await;
        let mut error_message = None;
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::Error { error, .. } = event {
                error_message = error.error_message;
            }
        }
        let error = error_message.expect("error message");
        assert!(!error.contains(secret), "secret leaked: {error}");
        assert!(error.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn cancelled_request_reports_aborted_without_sending() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let model = vertex_model("http://127.0.0.1:9/v1".into());
        let mut options = vertex_options();
        options.stream.abort_signal = Some(token);
        let stream = stream_vertex(model, Context::default(), options).await;
        let event = stream.next().await.expect("terminal event");
        match event {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(reason, StopReason::Aborted);
                assert_eq!(error.stop_reason, StopReason::Aborted);
                assert_eq!(error.error_message.as_deref(), Some("Request was aborted"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(stream.next().await.is_none());
    }
}
