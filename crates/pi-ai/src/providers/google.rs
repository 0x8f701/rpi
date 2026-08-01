use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::common::{
    apply_provider_headers, apply_provider_request, client, consume_sse, error_body, fail,
    notify_response, send_with_retry,
};
use crate::*;

const GOOGLE_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default)]
pub struct GoogleOptions {
    pub stream: StreamOptions,
    pub thinking: Option<GoogleThinkingConfig>,
    /// Function-calling mode preference: `auto`, `none`, or `any`.
    /// Mapped into `toolConfig.functionCallingConfig.mode` when tools are present.
    pub tool_choice: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GoogleThinkingConfig {
    pub enabled: bool,
    pub budget: Option<i64>,
    pub level: Option<String>,
}

pub async fn stream_google(
    model: Model,
    context: Context,
    options: GoogleOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let task_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        if let Err(error) =
            run_google_stream(&task_stream, &model, &context, &options, &mut output).await
        {
            let aborted = matches!(output.stop_reason, StopReason::Aborted)
                || options
                    .stream
                    .abort_signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
            fail(&task_stream, output, error.to_string(), aborted).await;
        }
    });
    stream
}

pub async fn stream_simple_google(
    model: Model,
    context: Context,
    mut options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    // Upstream buildBaseOptions: clamp(options?.maxTokens ?? model.maxTokens).
    let max_tokens = clamp_max_tokens_to_context(
        &model,
        &context,
        options.stream.max_tokens.unwrap_or(model.max_tokens),
    );
    options.stream.max_tokens = Some(max_tokens);
    let thinking = Some(match options.reasoning {
        None => GoogleThinkingConfig::default(),
        Some(level) => enabled_thinking(&model, level, options.thinking_budgets.as_ref()),
    });
    stream_google(
        model,
        context,
        GoogleOptions {
            stream: options.stream,
            thinking,
            tool_choice: None,
        },
    )
    .await
}

fn google_api_key<'a>(model: &Model, options: &'a StreamOptions) -> Result<&'a str> {
    if let Some(api_key) = options
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
    {
        return Ok(api_key);
    }
    let option_value = options.headers.iter().find_map(|(header, value)| {
        header
            .eq_ignore_ascii_case("x-goog-api-key")
            .then_some(value.as_str())
    });
    let model_value = model.headers.as_ref().and_then(|headers| {
        headers.iter().find_map(|(header, value)| {
            header
                .eq_ignore_ascii_case("x-goog-api-key")
                .then_some(value.as_str())
        })
    });
    if option_value
        .or(model_value)
        .is_some_and(|value| !value.trim().is_empty())
    {
        Ok("")
    } else {
        Err(anyhow!("No API key for provider: {}", model.provider))
    }
}

async fn run_google_stream(
    stream: &AssistantMessageEventStream,
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
    output: &mut AssistantMessage,
) -> Result<()> {
    let api_key = google_api_key(model, &options.stream)?;
    let payload = apply_provider_request(
        build_google_payload(model, context, options)?,
        model,
        &options.stream,
    )
    .await?;
    let configured_base = if model.base_url.trim().is_empty() {
        GOOGLE_DEFAULT_BASE_URL
    } else {
        model.base_url.as_str()
    };
    // Resolve Cloudflare `{CLOUDFLARE_*}` placeholders when present; plain URLs pass through.
    let base_url = resolve_base_url(configured_base, &options.stream.env)
        .map_err(|error| anyhow!(error.to_string()))?;
    let base_url = base_url.trim_end_matches('/');
    let url = format!(
        "{base_url}/models/{}:streamGenerateContent?alt=sse",
        model.id
    );
    let request_headers = apply_provider_headers(
        google_request_headers(model, &options.stream, api_key)?,
        model,
        &options.stream,
    )
    .await?;
    let http = client(&options.stream)?;
    let response = send_with_retry(&options.stream, || {
        let request = http
            .post(&url)
            .json(&payload);
        apply_google_request_headers(request, &request_headers)
    })
    .await?;
    notify_response(&options.stream, &response, model).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            error_body("Google", response, &options.stream).await?
        ));
    }

    stream
        .push(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let mut state = GoogleStreamState::default();
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
        // Bounded SSE JSON repair; unrecoverable payloads are skipped (upstream parity).
        let Some(value) = parse_json_with_repair(data) else {
            return Ok(());
        };
        let chunk: GoogleChunk = serde_json::from_value(value)?;
        if let Some(error) = chunk.error.as_ref() {
            bail!("got status: {}. {data}", error.status);
        }
        let mut events = Vec::new();
        state.apply_chunk(chunk, output, model, &mut events)?;
        for event in events {
            event_tx
                .send(event)
                .map_err(|_| anyhow!("Google event stream closed"))?;
        }
        Ok(())
    })
    .await;
    let mut events = Vec::new();
    state.end_current(output, &mut events);
    for event in events {
        event_tx
            .send(event)
            .map_err(|_| anyhow!("Google event stream closed"))?;
    }
    drop(event_tx);
    forwarder.await?;
    parse_result?;

    if output.stop_reason == StopReason::Pending {
        bail!("Google stream ended without a finish reason");
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

#[derive(Debug, Default)]
struct GoogleStreamState {
    current: Option<(usize, StreamingBlock)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingBlock {
    Text,
    Thinking,
}

impl GoogleStreamState {
    fn apply_chunk(
        &mut self,
        chunk: GoogleChunk,
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
        part: GooglePart,
        output: &mut AssistantMessage,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> Result<()> {
        let GooglePart {
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
                    if thought_signature.is_some() {
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
                    if thought_signature.is_some() {
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
                thought_signature: signature_for_call,
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

fn build_google_payload(
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
) -> Result<Value> {
    let mut payload = Map::new();
    payload.insert(
        "contents".into(),
        Value::Array(google_contents(model, context)),
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
        let mode = resolve_google_function_calling_mode(
            &context.tools,
            options.tool_choice.as_deref(),
            supports_google_strict_tool_sampling(&model.id),
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

fn google_contents(model: &Model, context: &Context) -> Vec<Value> {
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
                let parts = assistant_parts(message, same_model, model);
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
                let joined = text.join("\n");
                let response_value = if !joined.is_empty() {
                    joined
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
                unreachable!("provider transforms project session messages")
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

fn assistant_parts(message: &AssistantMessage, same_model: bool, model: &Model) -> Vec<Value> {
    content_parts(&message.content, same_model, model)
}

fn enabled_thinking(
    model: &Model,
    reasoning: ThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> GoogleThinkingConfig {
    // Clamp to model-supported levels first (upstream clampThinkingLevel), then map.
    let requested = match reasoning {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    };
    let mut effort = clamp_thinking_level(model, requested);
    // Upstream google streamSimple only coerces "off" → "high".
    if effort == "off" {
        effort = "high";
    }
    if is_gemini_3(&model.id) || is_gemma_4(&model.id) {
        GoogleThinkingConfig {
            enabled: true,
            level: google_thinking_level(effort, &model.id).map(str::to_owned),
            budget: None,
        }
    } else {
        GoogleThinkingConfig {
            enabled: true,
            budget: google_budget(&model.id, effort, custom),
            level: None,
        }
    }
}

/// Build the effective Google request header map once.
///
/// Precedence is case-insensitive last-wins:
/// provider `x-goog-api-key` default < model headers < caller `options.headers`.
/// Attribution defaults are applied first (telemetry stays disabled); caller/model
/// headers override them. The map is single-valued so reqwest cannot emit duplicate
/// `x-goog-api-key` lines from stacked `.header` appends.
fn google_request_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = merge_provider_attribution_headers(
        model,
        options.session_id.as_deref(),
        false,
        &HashMap::new(),
    )
    .unwrap_or_default();
    insert_header_case_insensitive(&mut headers, "content-type", "application/json");

    if !api_key.trim().is_empty() {
        insert_header_case_insensitive(&mut headers, "x-goog-api-key", api_key);
    }
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header_case_insensitive(&mut headers, name, value);
        }
    }
    for (name, value) in &options.headers {
        insert_header_case_insensitive(&mut headers, name, value);
    }

    let mut map = reqwest::header::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name =
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                anyhow!(
                    "Invalid Google request header name: {}",
                    name.to_ascii_lowercase()
                )
            })?;
        let mut header_value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            anyhow!(
                "Invalid Google request header value for {}",
                name.to_ascii_lowercase()
            )
        })?;
        if header_name.as_str() == "x-goog-api-key" {
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

/// Apply the pre-built effective header map once via HeaderMap insert/replace
/// semantics (not RequestBuilder `.header` append).
fn apply_google_request_headers(
    builder: reqwest::RequestBuilder,
    headers: &reqwest::header::HeaderMap,
) -> reqwest::RequestBuilder {
    builder.headers(headers.clone())
}

fn google_thinking_level<'a>(effort: &'a str, model_id: &str) -> Option<&'a str> {
    if is_gemini_3_pro(model_id) {
        return match effort {
            "minimal" | "low" => Some("LOW"),
            "medium" | "high" => Some("HIGH"),
            _ => None,
        };
    }
    if is_gemma_4(model_id) {
        return match effort {
            "minimal" | "low" => Some("MINIMAL"),
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

fn google_budget(model_id: &str, effort: &str, custom: Option<&ThinkingBudgets>) -> Option<i64> {
    if let Some(custom_budget) = custom.and_then(|budgets| match effort {
        "minimal" => budgets.minimal,
        "low" => budgets.low,
        "medium" => budgets.medium,
        "high" => budgets.high,
        _ => None,
    }) {
        return Some(custom_budget);
    }
    let values = if model_id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if model_id.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576]
    } else if model_id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return Some(-1);
    };
    match effort {
        "minimal" => Some(values[0]),
        "low" => Some(values[1]),
        "medium" => Some(values[2]),
        "high" => Some(values[3]),
        _ => None,
    }
}

fn disabled_thinking_config(model_id: &str) -> Value {
    if is_gemini_3_pro(model_id) {
        json!({"thinkingLevel": "LOW"})
    } else if is_gemini_3_flash(model_id) || is_gemma_4(model_id) {
        json!({"thinkingLevel": "MINIMAL"})
    } else {
        json!({"thinkingBudget": 0})
    }
}

fn is_gemini_3(model_id: &str) -> bool {
    is_gemini_3_pro(model_id) || is_gemini_3_flash(model_id)
}

fn is_gemini_3_pro(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.starts_with("gemini-3") && id.contains("-pro")
}

fn is_gemini_3_flash(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    (id.starts_with("gemini-3") && id.contains("-flash"))
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
}

fn is_gemma_4(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("gemma-4") || id.contains("gemma4")
}

fn gemini_major(model_id: &str) -> Option<u32> {
    let id = model_id.to_ascii_lowercase();
    let suffix = id
        .strip_prefix("gemini-live-")
        .or_else(|| id.strip_prefix("gemini-"))?;
    suffix.split('-').next()?.split('.').next()?.parse().ok()
}

fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
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

/// Picks `functionCallingConfig.mode`, or `None` to omit `toolConfig` entirely.
fn resolve_google_function_calling_mode(
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
    if matches!(choice, Some("none") | Some("any")) {
        return Ok(Some(map_tool_choice(choice.unwrap())));
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

/// Sanitize tool-call ids for Google targets that require them (`claude-*` / `gpt-oss-*`).
/// Non-`[A-Za-z0-9_-]` bytes become `_`; result is truncated to 64 chars.
fn normalize_tool_call_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(64).collect()
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
struct GoogleChunk {
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    candidates: Vec<GoogleCandidate>,
    usage_metadata: Option<GoogleUsage>,
    error: Option<GoogleError>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GoogleCandidate {
    #[serde(default)]
    content: GoogleCandidateContent,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GoogleCandidateContent {
    #[serde(default)]
    parts: Vec<GooglePart>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GooglePart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    thought_signature: Option<String>,
    function_call: Option<GoogleFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct GoogleFunctionCall {
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default = "empty_arguments")]
    args: Value,
}

fn empty_arguments() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GoogleUsage {
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

#[derive(Debug, Deserialize)]
struct GoogleError {
    #[serde(default)]
    status: String,
}

pub fn register_google() {
    let native: StreamFn = std::sync::Arc::new(|model, context, options| {
        async move {
            stream_google(
                model,
                context,
                GoogleOptions {
                    stream: options,
                    thinking: None,
                    tool_choice: None,
                },
            )
            .await
        }
        .boxed()
    });
    let simple: SimpleStreamFn = std::sync::Arc::new(|model, context, options| {
        async move { stream_simple_google(model, context, options).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_GOOGLE_GENERATIVE_AI.into(),
            stream: native,
            stream_simple: simple,
        },
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_unified_context_and_thinking() {
        let model = Model {
            id: "gemini-3-flash".into(),
            provider: "google".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            ..Model::default()
        };
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
                    api: API_GOOGLE_GENERATIVE_AI.into(),
                    provider: "google".into(),
                    model: "gemini-3-flash".into(),
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
                    details: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    usage: None,
                    timestamp: 3,
                }),
            ],
            tools: vec![ToolDefinition {
                name: "inspect".into(),
                description: "Inspect an image".into(),
                parameters: Schema::object(
                    std::collections::HashMap::from([("target".into(), Schema::string())]),
                    vec!["target".into()],
                ),
                constrained_sampling: None,
            }],
        };
        let payload = build_google_payload(
            &model,
            &context,
            &GoogleOptions {
                stream: StreamOptions {
                    temperature: Some(0.2),
                    max_tokens: Some(512),
                    ..StreamOptions::default()
                },
                thinking: Some(GoogleThinkingConfig {
                    enabled: true,
                    level: Some("HIGH".into()),
                    budget: None,
                }),
                tool_choice: None,
            },
        )
        .unwrap();

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

    fn plain_tool() -> ToolDefinition {
        ToolDefinition {
            name: "calc".into(),
            description: "d".into(),
            parameters: Schema::object(
                std::collections::HashMap::from([("x".into(), Schema::string())]),
                Vec::new(),
            ),
            constrained_sampling: None,
        }
    }

    fn strict_tool(strict: ConstrainedSamplingStrictness) -> ToolDefinition {
        let mut tool = plain_tool();
        tool.constrained_sampling = Some(ConstrainedSampling::json_schema(strict));
        tool
    }

    fn google_model(id: &str) -> Model {
        Model {
            id: id.into(),
            provider: "google".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            ..Model::default()
        }
    }

    fn tool_context(tool: ToolDefinition) -> Context {
        Context {
            messages: vec![Message::user_text("hi", 1)],
            tools: vec![tool],
            ..Context::default()
        }
    }

    fn function_calling_mode(payload: &Value) -> Option<&str> {
        payload
            .get("toolConfig")
            .and_then(|cfg| cfg.get("functionCallingConfig"))
            .and_then(|fcc| fcc.get("mode"))
            .and_then(Value::as_str)
    }

    #[test]
    fn omits_tool_config_for_plain_tools_without_choice() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(plain_tool()),
            &GoogleOptions::default(),
        )
        .unwrap();
        assert!(payload.get("toolConfig").is_none());
        assert!(payload.get("tools").is_some());
    }

    #[test]
    fn emits_auto_mode_for_explicit_auto_choice() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(plain_tool()),
            &GoogleOptions {
                tool_choice: Some("auto".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&payload), Some("AUTO"));
    }

    #[test]
    fn emits_none_mode_for_disabled_tool_choice() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(plain_tool()),
            &GoogleOptions {
                tool_choice: Some("none".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&payload), Some("NONE"));
    }

    #[test]
    fn emits_any_mode_for_forced_tool_choice() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(plain_tool()),
            &GoogleOptions {
                tool_choice: Some("any".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&payload), Some("ANY"));
    }

    #[test]
    fn emits_validated_mode_for_gemini_3_strict_tools() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Prefer)),
            &GoogleOptions::default(),
        )
        .unwrap();
        assert_eq!(function_calling_mode(&payload), Some("VALIDATED"));

        let with_auto = build_google_payload(
            &google_model("gemini-3-flash"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Require)),
            &GoogleOptions {
                tool_choice: Some("auto".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&with_auto), Some("VALIDATED"));
    }

    #[test]
    fn none_and_any_win_over_validated() {
        let none = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Require)),
            &GoogleOptions {
                tool_choice: Some("none".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&none), Some("NONE"));

        let any = build_google_payload(
            &google_model("gemini-3-pro"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Require)),
            &GoogleOptions {
                tool_choice: Some("any".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert_eq!(function_calling_mode(&any), Some("ANY"));
    }

    #[test]
    fn omits_validated_on_pre_gemini_3_prefer_strict() {
        let payload = build_google_payload(
            &google_model("gemini-2.5-pro"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Prefer)),
            &GoogleOptions::default(),
        )
        .unwrap();
        assert!(payload.get("toolConfig").is_none());
    }

    #[test]
    fn require_strict_fails_on_pre_gemini_3() {
        let err = build_google_payload(
            &google_model("gemini-2.5-pro"),
            &tool_context(strict_tool(ConstrainedSamplingStrictness::Require)),
            &GoogleOptions::default(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains(
                "Tool \"calc\" requires JSON-schema constrained sampling, but strict tools are unsupported."
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn google_projects_visible_bash_and_excludes_hidden_bash() {
        let model = google_model("gemini-3-flash");
        let context = Context {
            messages: vec![
                Message::BashExecution(crate::BashExecutionMessage {
                    command: "echo ok".into(),
                    output: "ok".into(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 1,
                    exclude_from_context: None,
                }),
                Message::BashExecution(crate::BashExecutionMessage {
                    command: "secret".into(),
                    output: "hidden".into(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 2,
                    exclude_from_context: Some(true),
                }),
            ],
            ..Context::default()
        };
        let contents = google_contents(&model, &context);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(
            contents[0]["parts"][0]["text"],
            "Ran `echo ok`\n```\nok\n```"
        );
    }

    #[test]
    fn omits_tool_config_when_no_tools() {
        let payload = build_google_payload(
            &google_model("gemini-3-pro"),
            &Context {
                messages: vec![Message::user_text("hi", 1)],
                ..Context::default()
            },
            &GoogleOptions {
                tool_choice: Some("any".into()),
                ..GoogleOptions::default()
            },
        )
        .unwrap();
        assert!(payload.get("tools").is_none());
        assert!(payload.get("toolConfig").is_none());
    }

    #[test]
    fn synthesizes_orphan_tool_results_before_google_contents() {
        let model = google_model("gemini-3-flash");
        let context = Context {
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "orphan-call".into(),
                        name: "inspect".into(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                    api: API_GOOGLE_GENERATIVE_AI.into(),
                    provider: "google".into(),
                    model: "gemini-3-flash".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 2,
                }),
                Message::user_text("continue", 4),
            ],
            ..Context::default()
        };
        let contents = google_contents(&model, &context);
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "inspect");
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["response"]["error"],
            "No result provided"
        );
        assert_eq!(contents[2]["parts"][0]["text"], "continue");
    }

    #[test]
    fn normalizes_cross_model_tool_ids_for_claude_via_google() {
        let model = Model {
            id: "claude-sonnet-via-google".into(),
            provider: "google".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            input: vec!["text".into()],
            ..Model::default()
        };
        let context = Context {
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call|with|pipes".into(),
                        name: "inspect".into(),
                        arguments: json!({"x": 1}),
                        thought_signature: Some("drop-me".into()),
                    })],
                    api: API_OPENAI_RESPONSES.into(),
                    provider: "openai".into(),
                    model: "gpt-test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call|with|pipes".into(),
                    tool_name: "inspect".into(),
                    content: vec![ContentBlock::text("ok")],
                    details: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    usage: None,
                    timestamp: 3,
                }),
            ],
            ..Context::default()
        };
        let contents = google_contents(&model, &context);
        assert_eq!(
            contents[0]["parts"][0]["functionCall"]["id"],
            "call_with_pipes"
        );
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["id"],
            "call_with_pipes"
        );
        assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
    }

    #[test]
    fn leaves_gemini_tool_ids_unchanged_for_cross_model_replay() {
        let model = google_model("gemini-3-pro");
        let context = Context {
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call|raw".into(),
                        name: "inspect".into(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                    api: API_OPENAI_COMPLETIONS.into(),
                    provider: "openai".into(),
                    model: "gpt-test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call|raw".into(),
                    tool_name: "inspect".into(),
                    content: vec![ContentBlock::text("ok")],
                    details: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    usage: None,
                    timestamp: 3,
                }),
            ],
            ..Context::default()
        };
        let contents = google_contents(&model, &context);
        // Gemini native does not require tool call ids on the wire.
        assert!(contents[0]["parts"][0]["functionCall"].get("id").is_none());
        assert!(
            contents[1]["parts"][0]["functionResponse"]
                .get("id")
                .is_none()
        );
    }

    #[test]
    fn clamps_thinking_level_before_google_mapping() {
        // xhigh is not in the default supported set, so clamp → high → HIGH.
        let model = Model {
            id: "gemini-3-flash".into(),
            provider: "google".into(),
            reasoning: true,
            ..Model::default()
        };
        let thinking = enabled_thinking(&model, ThinkingLevel::XHigh, None);
        assert!(thinking.enabled);
        assert_eq!(thinking.level.as_deref(), Some("HIGH"));
        assert!(thinking.budget.is_none());

        // Unsupported xhigh on 2.5 maps through clamp → high budget.
        let flash = Model {
            id: "gemini-2.5-flash".into(),
            provider: "google".into(),
            reasoning: true,
            ..Model::default()
        };
        let budgeted = enabled_thinking(&flash, ThinkingLevel::XHigh, None);
        assert_eq!(budgeted.budget, Some(24576));
    }

    #[test]
    fn clamp_max_tokens_to_context_is_applied_for_simple_defaults() {
        let model = Model {
            id: "gemini-3-pro".into(),
            provider: "google".into(),
            context_window: 10_000,
            max_tokens: 8_000,
            ..Model::default()
        };
        let context = Context {
            // Large system prompt forces the shared context clamp below max_tokens.
            system_prompt: "x".repeat(20_000),
            messages: vec![Message::user_text("hello", 1)],
            ..Context::default()
        };
        let clamped = clamp_max_tokens_to_context(&model, &context, model.max_tokens);
        assert!(clamped < model.max_tokens);
        assert!(clamped >= 1);
        assert_eq!(
            clamped,
            clamp_max_tokens_to_context(&model, &context, 8_000)
        );
    }

    #[test]
    fn normalize_tool_call_id_sanitizes_and_truncates() {
        assert_eq!(normalize_tool_call_id("a|b c/d"), "a_b_c_d");
        let long = "a".repeat(80);
        assert_eq!(normalize_tool_call_id(&long).len(), 64);
        assert_eq!(normalize_tool_call_id("already-ok_12"), "already-ok_12");
    }

    fn header_values(request: &reqwest::Request, name: &str) -> Vec<String> {
        request
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect()
    }

    fn build_google_http_request(
        model: &Model,
        options: &StreamOptions,
        api_key: &str,
    ) -> reqwest::Request {
        let headers = google_request_headers(model, options, api_key).expect("valid headers");
        apply_google_request_headers(
            client(options)
                .expect("client")
                .post("http://example.test/models/m:streamGenerateContent?alt=sse")
                .header("content-type", "application/json"),
            &headers,
        )
        .build()
        .expect("request")
    }

    #[test]
    fn request_headers_single_api_key_and_caller_override() {
        let model = Model {
            id: "gemini-test".into(),
            provider: "google".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            headers: Some(HashMap::from([
                ("X-Goog-Api-Key".into(), "model-key".into()),
                ("x-model-header".into(), "from-model".into()),
                ("X-Shared".into(), "model-shared".into()),
            ])),
            ..Model::default()
        };
        let options = StreamOptions {
            api_key: Some("resolved-key".into()),
            headers: HashMap::from([
                ("x-goog-api-key".into(), "caller-key".into()),
                ("X-Caller".into(), "from-caller".into()),
                ("x-shared".into(), "caller-shared".into()),
            ]),
            ..StreamOptions::default()
        };

        let request = build_google_http_request(&model, &options, "resolved-key");
        let api_keys = header_values(&request, "x-goog-api-key");
        assert_eq!(api_keys, vec!["caller-key".to_string()]);
        assert_eq!(
            request
                .headers()
                .get("x-model-header")
                .and_then(|value| value.to_str().ok()),
            Some("from-model")
        );
        assert_eq!(
            request
                .headers()
                .get("x-caller")
                .and_then(|value| value.to_str().ok()),
            Some("from-caller")
        );
        assert_eq!(
            request
                .headers()
                .get("x-shared")
                .and_then(|value| value.to_str().ok()),
            Some("caller-shared")
        );
    }

    #[test]
    fn request_headers_model_overrides_default_api_key_case_insensitively() {
        let model = Model {
            id: "gemini-test".into(),
            provider: "google".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            headers: Some(HashMap::from([(
                "X-GOOG-API-KEY".into(),
                "model-key".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions::default();
        let request = build_google_http_request(&model, &options, "resolved-key");
        assert_eq!(
            header_values(&request, "x-goog-api-key"),
            vec!["model-key".to_string()]
        );
    }

    #[test]
    fn request_headers_support_header_only_auth_without_empty_default() {
        let model = Model {
            id: "gemini-test".into(),
            provider: "custom".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            headers: Some(HashMap::from([(
                "X-Goog-Api-Key".into(),
                "model-secret".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("X-GOOG-API-KEY".into(), "caller-secret".into())]),
            ..StreamOptions::default()
        };
        let api_key = google_api_key(&model, &options).expect("header-owned auth");
        assert!(api_key.is_empty());
        let request = build_google_http_request(&model, &options, api_key);
        assert_eq!(
            header_values(&request, "x-goog-api-key"),
            vec!["caller-secret".to_string()]
        );
    }

    #[test]
    fn no_key_or_google_auth_header_returns_sanitized_error() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([
                ("X-Goog-Api-Key".into(), " ".into()),
                ("X-Secret".into(), "must-not-leak".into()),
            ])),
            ..Model::default()
        };
        let error = google_api_key(&model, &StreamOptions::default()).expect_err("missing auth");
        assert_eq!(error.to_string(), "No API key for provider: custom");
        assert!(!error.to_string().contains("must-not-leak"));
    }

    #[test]
    fn malformed_google_auth_header_returns_sanitized_error() {
        let model = Model {
            id: "gemini-test".into(),
            provider: "custom".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            headers: Some(HashMap::from([(
                "X-Goog-Api-Key".into(),
                "secret\ninvalid".into(),
            )])),
            ..Model::default()
        };
        let error = google_request_headers(&model, &StreamOptions::default(), "")
            .expect_err("malformed header rejected");
        assert!(error.to_string().contains("x-goog-api-key"));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn request_headers_preserve_attribution_without_duplicates() {
        let model = Model {
            id: "gemini-test".into(),
            provider: "opencode".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            headers: Some(HashMap::from([
                ("x-opencode-client".into(), "model-client".into()),
                ("x-model".into(), "model".into()),
            ])),
            ..Model::default()
        };
        let options = StreamOptions {
            session_id: Some("session-1".into()),
            headers: HashMap::from([
                ("x-opencode-client".into(), "caller-client".into()),
                ("x-request".into(), "caller".into()),
            ]),
            ..StreamOptions::default()
        };
        let request = build_google_http_request(&model, &options, "resolved-key");
        assert_eq!(
            header_values(&request, "x-goog-api-key"),
            vec!["resolved-key".to_string()]
        );
        assert_eq!(
            request
                .headers()
                .get("x-opencode-session")
                .and_then(|value| value.to_str().ok()),
            Some("session-1")
        );
        assert_eq!(
            request
                .headers()
                .get("x-opencode-client")
                .and_then(|value| value.to_str().ok()),
            Some("caller-client")
        );
        assert_eq!(
            request
                .headers()
                .get("x-model")
                .and_then(|value| value.to_str().ok()),
            Some("model")
        );
        assert_eq!(
            request
                .headers()
                .get("x-request")
                .and_then(|value| value.to_str().ok()),
            Some("caller")
        );
        assert!(!request.headers().contains_key("HTTP-Referer"));
        assert!(!request.headers().contains_key("X-OpenRouter-Title"));
    }

    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn streams_generate_content_and_stores_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = Arc::new(Mutex::new(String::new()));
        let captured = request.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0; 8192];
            let mut received = Vec::new();
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
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
            *captured.lock().await = String::from_utf8(received).unwrap();
            let body = concat!(
                "data: {\"responseId\":\"resp-1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let model = Model {
            id: "gemini-test".into(),
            name: "Gemini Test".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            provider: "google".into(),
            base_url: format!("http://{address}"),
            ..Model::default()
        };
        let context = Context {
            system_prompt: "Be concise".into(),
            messages: vec![Message::user_text("Say hello", 1)],
            ..Context::default()
        };
        let stream = stream_google(
            model,
            context,
            GoogleOptions {
                stream: StreamOptions {
                    api_key: Some("test-key".into()),
                    ..StreamOptions::default()
                },
                thinking: None,
                tool_choice: None,
            },
        )
        .await;
        let mut deltas = String::new();
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                deltas.push_str(&delta);
            }
        }
        let result = stream.result().await.unwrap();
        assert_eq!(deltas, "Hello world");
        assert_eq!(result.text(), "Hello world");
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.response_id.as_deref(), Some("resp-1"));
        assert_eq!(result.usage.total_tokens, 5);

        let request = request.lock().await;
        assert!(
            request
                .starts_with("POST /models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1\r\n")
        );
        let api_key_lines = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("x-goog-api-key:"))
            .collect::<Vec<_>>();
        assert_eq!(api_key_lines, vec!["x-goog-api-key: test-key"]);
        assert!(request.contains("\"systemInstruction\""));
        assert!(request.contains("\"contents\""));
        assert!(request.contains("Say hello"));
    }
}
