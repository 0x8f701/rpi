use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use futures_util::FutureExt;
use reqwest::{Response, Url, header::HeaderMap};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::common::{
    apply_provider_headers, apply_provider_request, client, consume_sse, headers_map,
    insert_header, insert_header_map, is_aborted, notify_response, send_with_retry,
};
use crate::*;

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MistralToolChoice {
    Auto,
    None,
    Any,
    Required,
    Function { name: String },
}

#[derive(Debug, Clone, Default)]
pub struct MistralOptions {
    pub stream: StreamOptions,
    pub tool_choice: Option<MistralToolChoice>,
    pub prompt_mode: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl From<StreamOptions> for MistralOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

pub async fn stream_mistral(
    model: Model,
    context: Context,
    options: MistralOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let task_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        // Upstream initializes Mistral output to `stop`, not an internal pending state.
        output.stop_reason = StopReason::Stop;
        let result =
            run_mistral_stream(&task_stream, &model, &context, &options, &mut output).await;
        if let Err(error) = result {
            let aborted = is_aborted(&options.stream);
            let message = redact_mistral_secrets(&error.to_string(), &model, &options.stream);
            super::common::fail(&task_stream, output, message, aborted).await;
        }
    });
    stream
}

pub async fn stream_simple_mistral(
    model: Model,
    context: Context,
    mut options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let max_tokens = clamp_max_tokens_to_context(
        &model,
        &context,
        options.stream.max_tokens.unwrap_or(model.max_tokens),
    );
    options.stream.max_tokens = Some(max_tokens);

    let reasoning = options
        .reasoning
        .map(|level| clamp_thinking_level(&model, thinking_level_name(level)).to_owned());
    let should_reason = model.reasoning && reasoning.is_some();
    let prompt_mode =
        (should_reason && uses_prompt_mode_reasoning(&model)).then(|| "reasoning".to_owned());
    let reasoning_effort = if should_reason && uses_reasoning_effort(&model) {
        reasoning
            .as_deref()
            .map(|level| map_reasoning_effort(&model, level))
    } else {
        None
    };

    stream_mistral(
        model,
        context,
        MistralOptions {
            stream: options.stream,
            tool_choice: None,
            prompt_mode,
            reasoning_effort,
        },
    )
    .await
}

pub fn register_mistral() {
    let native: StreamFn = Arc::new(|model, context, options| {
        async move { stream_mistral(model, context, options.into()).await }.boxed()
    });
    let simple: SimpleStreamFn = Arc::new(|model, context, options| {
        async move { stream_simple_mistral(model, context, options).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_MISTRAL_CONVERSATIONS.into(),
            stream: native,
            stream_simple: simple,
            generate_image: None,
        },
        None,
    );
}

async fn run_mistral_stream(
    stream: &AssistantMessageEventStream,
    model: &Model,
    context: &Context,
    options: &MistralOptions,
    output: &mut AssistantMessage,
) -> Result<()> {
    let api_key = mistral_api_key(model, &options.stream)?;
    let payload = apply_provider_request(
        build_mistral_payload(model, context, options)?,
        model,
        &options.stream,
    )
    .await?;
    let url = mistral_chat_url(model, &options.stream)?;
    let headers = apply_provider_headers(
        mistral_request_headers(model, &options.stream, api_key)?,
        model,
        &options.stream,
    )
    .await?;
    let http = client(&options.stream)?;
    let response = send_with_retry(&options.stream, || {
        http.post(url.clone())
            .headers(headers.clone())
            .json(&payload)
    })
    .await?;
    notify_response(&options.stream, &response, model).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            mistral_error_body(response, &options.stream).await?
        ));
    }

    stream
        .push(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_stream = stream.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            event_stream.push(event).await;
        }
    });

    let mut state = MistralStreamState::default();
    let parse_result = consume_sse(response, &options.stream, |_, data| {
        if data == "[DONE]" {
            return Ok(());
        }
        let chunk: MistralChunk = serde_json::from_str(data)
            .map_err(|error| anyhow!("Invalid Mistral SSE JSON: {error}"))?;
        state.apply_chunk(chunk, output, model, &event_tx)
    })
    .await;
    if parse_result.is_ok() {
        state.finish(output, &event_tx)?;
    }
    drop(event_tx);
    forwarder
        .await
        .map_err(|error| anyhow!("Mistral event forwarder failed: {error}"))?;
    parse_result?;

    if is_aborted(&options.stream) {
        bail!("Request was aborted");
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        bail!("An unknown error occurred");
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

fn mistral_chat_url(model: &Model, options: &StreamOptions) -> Result<Url> {
    let base = resolve_base_url(&model.base_url, &options.env)?;
    let mut url =
        Url::parse(&base).map_err(|error| anyhow!("Invalid Mistral base URL: {error}"))?;
    // The installed SDK resolves its absolute `/v1/chat/completions` operation
    // path against serverURL, replacing any configured path component.
    url.set_path("/v1/chat/completions");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn mistral_api_key<'a>(model: &Model, options: &'a StreamOptions) -> Result<&'a str> {
    if let Some(api_key) = options
        .api_key
        .as_deref()
        .filter(|api_key| !api_key.trim().is_empty())
    {
        return Ok(api_key);
    }
    if effective_header(model, options, "authorization")
        .is_some_and(|value| !value.trim().is_empty())
    {
        Ok("")
    } else {
        Err(anyhow!("No API key for provider: {}", model.provider))
    }
}

fn effective_header<'a>(
    model: &'a Model,
    options: &'a StreamOptions,
    wanted: &str,
) -> Option<&'a str> {
    let mut result = model.headers.as_ref().and_then(|headers| {
        headers
            .iter()
            .find_map(|(name, value)| name.eq_ignore_ascii_case(wanted).then_some(value.as_str()))
    });
    for (name, value) in &options.headers {
        if name.eq_ignore_ascii_case(wanted) {
            result = Some(value.as_str());
        }
    }
    result
}

fn mistral_request_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "content-type", "application/json")?;
    insert_header(&mut headers, "accept", "text/event-stream")?;
    if !api_key.trim().is_empty() {
        insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    }
    if let Some(model_headers) = &model.headers {
        insert_header_map(&mut headers, model_headers)?;
    }
    insert_header_map(&mut headers, &options.headers)?;
    if should_use_prompt_caching(options) && !headers.contains_key("x-affinity") {
        if let Some(session_id) = options.session_id.as_deref() {
            insert_header(&mut headers, "x-affinity", session_id)?;
        }
    }
    Ok(headers)
}

fn build_mistral_payload(
    model: &Model,
    context: &Context,
    options: &MistralOptions,
) -> Result<Value> {
    let mut normalizer = MistralToolCallIdNormalizer::default();
    let messages = transform_messages(&context.messages, model, |id, _, _| {
        normalizer.normalize(id)
    });
    let mut chat_messages = to_chat_messages(&messages, supports_images(model))?;
    if !context.system_prompt.is_empty() {
        chat_messages.insert(0, json!({"role":"system", "content":context.system_prompt}));
    }

    let mut payload = Map::new();
    payload.insert("model".into(), Value::String(model.id.clone()));
    payload.insert("stream".into(), Value::Bool(true));
    payload.insert("messages".into(), Value::Array(chat_messages));
    if !context.tools.is_empty() {
        payload.insert(
            "tools".into(),
            Value::Array(to_function_tools(&context.tools)?),
        );
    }
    if let Some(temperature) = options.stream.temperature {
        payload.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.stream.max_tokens {
        payload.insert("max_tokens".into(), json!(max_tokens));
    }
    if let Some(tool_choice) = &options.tool_choice {
        payload.insert("tool_choice".into(), mistral_tool_choice(tool_choice));
    }
    if let Some(prompt_mode) = &options.prompt_mode {
        payload.insert("prompt_mode".into(), Value::String(prompt_mode.clone()));
    }
    if let Some(reasoning_effort) = &options.reasoning_effort {
        payload.insert(
            "reasoning_effort".into(),
            Value::String(reasoning_effort.clone()),
        );
    }
    if should_use_prompt_caching(&options.stream) {
        if let Some(session_id) = options.stream.session_id.as_deref() {
            payload.insert("prompt_cache_key".into(), Value::String(session_id.into()));
        }
    }
    Ok(Value::Object(payload))
}

fn supports_images(model: &Model) -> bool {
    model.input.iter().any(|kind| kind == "image")
}

fn should_use_prompt_caching(options: &StreamOptions) -> bool {
    options.cache_retention != CacheRetention::None
        && options
            .session_id
            .as_deref()
            .is_some_and(|session_id| !session_id.is_empty())
}

fn to_function_tools(tools: &[Tool]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            let strict = matches!(
                tool.constrained_sampling,
                Some(ConstrainedSampling::JsonSchema { .. })
            );
            Ok(json!({
                "type":"function",
                "function":{
                    "name":tool.name,
                    "description":tool.description,
                    "parameters":serde_json::to_value(&tool.parameters)?,
                    "strict":strict,
                }
            }))
        })
        .collect()
}

fn to_chat_messages(messages: &[Message], supports_images: bool) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let had_images = user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }));
                let content = content_parts(&user.content, supports_images);
                if !content.is_empty() {
                    result.push(json!({"role":"user", "content":content}));
                } else if had_images && !supports_images {
                    result.push(json!({
                        "role":"user",
                        "content":"(image omitted: model does not support images)"
                    }));
                }
            }
            Message::Assistant(assistant) => {
                let mut content = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            content.push(json!({"type":"text", "text":text}));
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            content.push(json!({
                                "type":"thinking",
                                "thinking":[{"type":"text", "text":thinking}]
                            }));
                        }
                        ContentBlock::ToolCall(call) => {
                            tool_calls.push(json!({
                                "id":call.id,
                                "type":"function",
                                "function":{
                                    "name":call.name,
                                    "arguments":serde_json::to_string(&call.arguments)?,
                                },
                                "index":0,
                            }));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() || !tool_calls.is_empty() {
                    let mut value = Map::new();
                    value.insert("role".into(), Value::String("assistant".into()));
                    if !content.is_empty() {
                        value.insert("content".into(), Value::Array(content));
                    }
                    if !tool_calls.is_empty() {
                        value.insert("tool_calls".into(), Value::Array(tool_calls));
                    }
                    // The SDK's outbound schema supplies this default.
                    value.insert("prefix".into(), Value::Bool(false));
                    result.push(Value::Object(value));
                }
            }
            Message::ToolResult(tool_result) => {
                let text = tool_result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentBlock::Image { .. }));
                let mut content = vec![json!({
                    "type":"text",
                    "text":build_tool_result_text(
                        &text,
                        has_images,
                        supports_images,
                        tool_result.is_error,
                    ),
                })];
                if supports_images {
                    for part in &tool_result.content {
                        if let ContentBlock::Image { data, mime_type } = part {
                            content.push(json!({
                                "type":"image_url",
                                "image_url":format!("data:{mime_type};base64,{data}"),
                            }));
                        }
                    }
                }
                result.push(json!({
                    "role":"tool",
                    "tool_call_id":tool_result.tool_call_id,
                    "name":tool_result.tool_name,
                    "content":content,
                }));
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("session messages are projected before Mistral conversion")
            }
        }
    }
    Ok(result)
}

fn content_parts(content: &[ContentBlock], supports_images: bool) -> Vec<Value> {
    content
        .iter()
        .filter_map(|item| match item {
            ContentBlock::Text { text, .. } => Some(json!({"type":"text", "text":text})),
            ContentBlock::Image { data, mime_type } if supports_images => Some(json!({
                "type":"image_url",
                "image_url":format!("data:{mime_type};base64,{data}"),
            })),
            _ => None,
        })
        .collect()
}

fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }
    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".into()
            } else {
                "(see attached image)".into()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".into()
        } else {
            "(image omitted: model does not support images)".into()
        };
    }
    if is_error {
        "[tool error] (no tool output)".into()
    } else {
        "(no tool output)".into()
    }
}

fn mistral_tool_choice(choice: &MistralToolChoice) -> Value {
    match choice {
        MistralToolChoice::Auto => Value::String("auto".into()),
        MistralToolChoice::None => Value::String("none".into()),
        MistralToolChoice::Any => Value::String("any".into()),
        MistralToolChoice::Required => Value::String("required".into()),
        MistralToolChoice::Function { name } => {
            json!({"type":"function", "function":{"name":name}})
        }
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

fn uses_reasoning_effort(model: &Model) -> bool {
    matches!(
        model.id.as_str(),
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

fn map_reasoning_effort(model: &Model, level: &str) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(level))
        .and_then(|value| value.clone())
        .unwrap_or_else(|| "high".into())
}

#[derive(Default)]
struct MistralToolCallIdNormalizer {
    forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.forward.get(id) {
            return existing.clone();
        }
        let mut attempt = 0_u64;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            match self.reverse.get(&candidate) {
                None => {
                    self.forward.insert(id.into(), candidate.clone());
                    self.reverse.insert(candidate.clone(), id.into());
                    return candidate;
                }
                Some(owner) if owner == id => return candidate,
                Some(_) => attempt += 1,
            }
        }
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: u64) -> String {
    let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed_base.to_owned()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beef_u32;
    let mut h2 = 0x41c6_ce57_u32;
    for code_unit in value.encode_utf16() {
        h1 = (h1 ^ u32::from(code_unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(code_unit)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value != 0 {
        let digit = (value % 36) as u8;
        digits.push(if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        });
        value /= 36;
    }
    digits.iter().rev().collect()
}

#[derive(Debug, Deserialize, Default)]
struct MistralChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    choices: Vec<MistralChoice>,
    usage: Option<MistralUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct MistralChoice {
    #[serde(default)]
    delta: MistralDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct MistralDelta {
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<MistralToolDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct MistralToolDelta {
    id: Option<String>,
    index: Option<usize>,
    #[serde(default)]
    function: MistralFunctionDelta,
}

#[derive(Debug, Deserialize, Default)]
struct MistralFunctionDelta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize, Default)]
struct MistralUsage {
    #[serde(default, alias = "promptTokens")]
    prompt_tokens: i64,
    #[serde(default, alias = "completionTokens")]
    completion_tokens: i64,
    #[serde(default, alias = "totalTokens")]
    total_tokens: i64,
    #[serde(default, alias = "promptTokensDetails")]
    prompt_tokens_details: Option<MistralPromptTokenDetails>,
    #[serde(default, alias = "promptTokenDetails")]
    prompt_token_details: Option<MistralPromptTokenDetails>,
    #[serde(default, alias = "numCachedTokens")]
    num_cached_tokens: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct MistralPromptTokenDetails {
    #[serde(default, alias = "cachedTokens")]
    cached_tokens: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentBlock {
    Text(usize),
    Thinking(usize),
}

#[derive(Debug)]
struct MistralToolSlot {
    key: String,
    content_index: usize,
    partial_args: String,
}

#[derive(Default)]
struct MistralStreamState {
    current: Option<CurrentBlock>,
    tools: Vec<MistralToolSlot>,
}

impl MistralStreamState {
    fn apply_chunk(
        &mut self,
        chunk: MistralChunk,
        output: &mut AssistantMessage,
        model: &Model,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        if output.response_id.is_none() && !chunk.id.is_empty() {
            output.response_id = Some(chunk.id);
        }
        if let Some(usage) = chunk.usage {
            apply_mistral_usage(usage, output, model);
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(());
        };
        if let Some(reason) = choice.finish_reason {
            output.stop_reason = map_chat_stop_reason(&reason);
        }
        if let Some(content) = choice.delta.content {
            self.apply_content(content, output, events)?;
        }
        for tool_call in choice.delta.tool_calls {
            self.append_tool(tool_call, output, events)?;
        }
        Ok(())
    }

    fn apply_content(
        &mut self,
        content: Value,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        match content {
            Value::String(text) => self.append_text(text, output, events),
            Value::Array(items) => {
                for item in items {
                    let Some(kind) = item.get("type").and_then(Value::as_str) else {
                        continue;
                    };
                    match kind {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                self.append_text(text.to_owned(), output, events)?;
                            }
                        }
                        "thinking" => {
                            let thinking = item
                                .get("thinking")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<String>();
                            if !thinking.is_empty() {
                                self.append_thinking(thinking, output, events)?;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            Value::Null => Ok(()),
            _ => Ok(()),
        }
    }

    fn append_text(
        &mut self,
        delta: String,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let index = match self.current {
            Some(CurrentBlock::Text(index)) => index,
            _ => {
                self.finish_current(output, events)?;
                let index = output.content.len();
                output.content.push(ContentBlock::text(""));
                self.current = Some(CurrentBlock::Text(index));
                send_event(
                    events,
                    AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: output.clone(),
                    },
                )?;
                index
            }
        };
        if let ContentBlock::Text { text, .. } = &mut output.content[index] {
            text.push_str(&delta);
        }
        send_event(
            events,
            AssistantMessageEvent::TextDelta {
                content_index: index,
                delta,
                partial: output.clone(),
            },
        )
    }

    fn append_thinking(
        &mut self,
        delta: String,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let index = match self.current {
            Some(CurrentBlock::Thinking(index)) => index,
            _ => {
                self.finish_current(output, events)?;
                let index = output.content.len();
                output.content.push(ContentBlock::thinking(""));
                self.current = Some(CurrentBlock::Thinking(index));
                send_event(
                    events,
                    AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: output.clone(),
                    },
                )?;
                index
            }
        };
        if let ContentBlock::Thinking { thinking, .. } = &mut output.content[index] {
            thinking.push_str(&delta);
        }
        send_event(
            events,
            AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta,
                partial: output.clone(),
            },
        )
    }

    fn append_tool(
        &mut self,
        tool_call: MistralToolDelta,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        self.finish_current(output, events)?;
        let source_index = tool_call.index.unwrap_or(0);
        let call_id = tool_call
            .id
            .filter(|id| id != "null")
            .unwrap_or_else(|| derive_mistral_tool_call_id(&format!("toolcall:{source_index}"), 0));
        let key = format!("{call_id}:{source_index}");
        let slot_position = self.tools.iter().position(|slot| slot.key == key);
        let position = match slot_position {
            Some(position) => position,
            None => {
                let content_index = output.content.len();
                output.content.push(ContentBlock::ToolCall(ToolCall {
                    id: call_id,
                    name: tool_call.function.name.clone(),
                    arguments: json!({}),
                    thought_signature: None,
                }));
                self.tools.push(MistralToolSlot {
                    key,
                    content_index,
                    partial_args: String::new(),
                });
                send_event(
                    events,
                    AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: output.clone(),
                    },
                )?;
                self.tools.len() - 1
            }
        };

        let args_delta = match tool_call.function.arguments {
            Value::String(arguments) => arguments,
            Value::Null => "{}".into(),
            arguments => serde_json::to_string(&arguments)?,
        };
        let slot = &mut self.tools[position];
        slot.partial_args.push_str(&args_delta);
        let arguments = parse_streaming_json(&slot.partial_args);
        if let ContentBlock::ToolCall(call) = &mut output.content[slot.content_index] {
            if call.name.is_empty() {
                call.name = tool_call.function.name;
            }
            call.arguments = arguments;
        }
        send_event(
            events,
            AssistantMessageEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: args_delta,
                partial: output.clone(),
            },
        )
    }

    fn finish_current(
        &mut self,
        output: &AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        match self.current.take() {
            Some(CurrentBlock::Text(index)) => {
                if let ContentBlock::Text { text, .. } = &output.content[index] {
                    send_event(
                        events,
                        AssistantMessageEvent::TextEnd {
                            content_index: index,
                            content: text.clone(),
                            partial: output.clone(),
                        },
                    )?;
                }
            }
            Some(CurrentBlock::Thinking(index)) => {
                if let ContentBlock::Thinking { thinking, .. } = &output.content[index] {
                    send_event(
                        events,
                        AssistantMessageEvent::ThinkingEnd {
                            content_index: index,
                            content: thinking.clone(),
                            partial: output.clone(),
                        },
                    )?;
                }
            }
            None => {}
        }
        Ok(())
    }

    fn finish(
        &mut self,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        self.finish_current(output, events)?;
        for slot in &self.tools {
            let arguments = parse_streaming_json(&slot.partial_args);
            if let ContentBlock::ToolCall(call) = &mut output.content[slot.content_index] {
                call.arguments = arguments;
                send_event(
                    events,
                    AssistantMessageEvent::ToolCallEnd {
                        content_index: slot.content_index,
                        tool_call: call.clone(),
                        partial: output.clone(),
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn send_event(
    events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    event: AssistantMessageEvent,
) -> Result<()> {
    events
        .send(event)
        .map_err(|_| anyhow!("Mistral event stream closed"))
}

fn apply_mistral_usage(usage: MistralUsage, output: &mut AssistantMessage, model: &Model) {
    let prompt_tokens = usage.prompt_tokens;
    let raw_cached = usage
        .prompt_tokens_details
        .map(|details| details.cached_tokens)
        .or_else(|| {
            usage
                .prompt_token_details
                .map(|details| details.cached_tokens)
        })
        .or(usage.num_cached_tokens)
        .unwrap_or(0);
    let cached = raw_cached.max(0).min(prompt_tokens);
    output.usage.input = (prompt_tokens - cached).max(0);
    output.usage.output = usage.completion_tokens;
    output.usage.cache_read = cached;
    output.usage.cache_write = 0;
    output.usage.total_tokens = if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        output.usage.input + output.usage.output + output.usage.cache_read
    };
    calculate_cost(model, &mut output.usage);
}

fn map_chat_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" | "model_length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        "error" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

fn parse_streaming_json(input: &str) -> Value {
    if input.trim().is_empty() {
        return json!({});
    }
    if let Some(value) = parse_json_with_repair(input) {
        return value;
    }
    let trimmed = input.trim_end();
    for cut in (0..=trimmed.len()).rev() {
        if !trimmed.is_char_boundary(cut) {
            continue;
        }
        let prefix = trimmed[..cut].trim_end_matches([',', ':', ' ']);
        if prefix.is_empty() {
            continue;
        }
        if let Some(candidate) = close_partial_json(prefix) {
            if let Some(value) = parse_json_with_repair(&candidate) {
                return value;
            }
        }
        if matches!(trimmed.as_bytes().get(cut), Some(b',')) {
            break;
        }
    }
    json!({})
}

fn close_partial_json(input: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for byte in input.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                if stack.pop() != Some(byte) {
                    return None;
                }
            }
            _ => {}
        }
    }
    let mut candidate = input.to_owned();
    if in_string {
        if escaped {
            candidate.push('\\');
        }
        candidate.push('"');
    }
    while let Some(closer) = stack.pop() {
        candidate.push(char::from(closer));
    }
    Some(candidate)
}

async fn mistral_error_body(response: Response, options: &StreamOptions) -> Result<String> {
    let status = response.status().as_u16();
    let body = match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
            body = response.text() => body.unwrap_or_default(),
        },
        None => response.text().await.unwrap_or_default(),
    };
    let body = body.trim();
    if body.is_empty() {
        Ok(format!("Mistral API error ({status})"))
    } else {
        Ok(format!(
            "Mistral API error ({status}): {}",
            truncate_utf16(body, MAX_MISTRAL_ERROR_BODY_CHARS)
        ))
    }
}

fn truncate_utf16(text: &str, max_chars: usize) -> String {
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    if utf16.len() <= max_chars {
        return text.into();
    }
    let prefix = String::from_utf16_lossy(&utf16[..max_chars]);
    format!("{prefix}... [truncated {} chars]", utf16.len() - max_chars)
}

fn redact_mistral_secrets(message: &str, model: &Model, options: &StreamOptions) -> String {
    let mut secrets = HashSet::new();
    if let Some(api_key) = options
        .api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        secrets.insert(api_key.to_owned());
    }
    for headers in model
        .headers
        .iter()
        .chain(std::iter::once(&options.headers))
    {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
            {
                let value = value.trim();
                if !value.is_empty() {
                    secrets.insert(value.to_owned());
                    if let Some((scheme, token)) = value.split_once(' ') {
                        if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
                            secrets.insert(token.trim().to_owned());
                        }
                    }
                }
            }
        }
    }
    let mut redacted = message.to_owned();
    let mut secrets = secrets.into_iter().collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        redacted = redacted.replace(&secret, "[REDACTED]");
    }
    redacted
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    fn model(base_url: String) -> Model {
        Model {
            id: "mistral-small-2603".into(),
            name: "Mistral Small".into(),
            api: API_MISTRAL_CONVERSATIONS.into(),
            provider: "mistral".into(),
            base_url,
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            context_window: 128_000,
            max_tokens: 8_192,
            ..Model::default()
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = socket.read(&mut buffer).await.expect("read request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..split]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= split + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).expect("utf8 request")
    }

    fn request_json(request: &str) -> Value {
        let (_, body) = request.split_once("\r\n\r\n").expect("request body");
        serde_json::from_str(body).expect("json request")
    }

    #[test]
    fn request_fixture_maps_messages_tools_images_thinking_and_cache() {
        let model = model("https://api.mistral.ai/custom/path".into());
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::thinking("prior reasoning"),
                ContentBlock::text("prior answer"),
                ContentBlock::ToolCall(ToolCall {
                    id: "Abc123xyz".into(),
                    name: "inspect".into(),
                    arguments: json!({"target":"image"}),
                    thought_signature: None,
                }),
            ],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 2,
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
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "Abc123xyz".into(),
                    tool_name: "inspect".into(),
                    content: vec![ContentBlock::text("ok")],
                    details: None,
                    added_tool_names: Vec::new(),
                    usage: None,
                    is_error: false,
                    timestamp: 3,
                }),
            ],
            tools: vec![Tool {
                name: "inspect".into(),
                description: "Inspect".into(),
                parameters: Schema::object(
                    HashMap::from([("target".into(), Schema::string())]),
                    vec!["target".into()],
                ),
                constrained_sampling: Some(ConstrainedSampling::json_schema(
                    ConstrainedSamplingStrictness::Require,
                )),
            }],
        };
        let payload = build_mistral_payload(
            &model,
            &context,
            &MistralOptions {
                stream: StreamOptions {
                    temperature: Some(0.2),
                    max_tokens: Some(512),
                    session_id: Some("session-1".into()),
                    ..StreamOptions::default()
                },
                tool_choice: Some(MistralToolChoice::Function {
                    name: "inspect".into(),
                }),
                prompt_mode: None,
                reasoning_effort: Some("high".into()),
            },
        )
        .expect("payload");

        assert_eq!(
            payload["messages"][0],
            json!({"role":"system", "content":"System"})
        );
        assert_eq!(
            payload["messages"][1]["content"][1]["image_url"],
            "data:image/png;base64,aW1n"
        );
        assert_eq!(payload["messages"][2]["content"][0]["type"], "thinking");
        assert_eq!(payload["messages"][2]["tool_calls"][0]["id"], "Abc123xyz");
        assert_eq!(payload["messages"][3]["tool_call_id"], "Abc123xyz");
        assert_eq!(payload["tools"][0]["function"]["strict"], true);
        assert_eq!(payload["tool_choice"]["function"]["name"], "inspect");
        assert_eq!(payload["reasoning_effort"], "high");
        assert_eq!(payload["prompt_cache_key"], "session-1");
    }

    #[test]
    fn cross_model_tool_ids_are_stable_nine_character_alphanumerics() {
        let model = model("https://api.mistral.ai".into());
        let original = "call_123|fc/456";
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: original.into(),
                name: "lookup".into(),
                arguments: json!({}),
                thought_signature: Some("foreign".into()),
            })],
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 1,
        };
        let context = Context {
            messages: vec![
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: original.into(),
                    tool_name: "lookup".into(),
                    content: vec![ContentBlock::text("ok")],
                    details: None,
                    added_tool_names: Vec::new(),
                    usage: None,
                    is_error: false,
                    timestamp: 2,
                }),
            ],
            ..Context::default()
        };
        let payload =
            build_mistral_payload(&model, &context, &MistralOptions::default()).expect("payload");
        let id = payload["messages"][0]["tool_calls"][0]["id"]
            .as_str()
            .expect("tool id");
        assert_eq!(id.len(), 9);
        assert!(id.chars().all(|ch| ch.is_ascii_alphanumeric()));
        assert_eq!(payload["messages"][1]["tool_call_id"], id);
    }

    #[test]
    fn simple_options_select_supported_reasoning_controls() {
        let effort_model = model("https://api.mistral.ai".into());
        assert!(uses_reasoning_effort(&effort_model));
        assert_eq!(map_reasoning_effort(&effort_model, "medium"), "high");
        let prompt_model = Model {
            id: "magistral-small".into(),
            reasoning: true,
            ..effort_model
        };
        assert!(uses_prompt_mode_reasoning(&prompt_model));
        assert!(!uses_reasoning_effort(&prompt_model));
    }

    #[tokio::test]
    async fn stream_fixture_maps_text_thinking_tool_usage_and_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let captured = Arc::new(Mutex::new(String::new()));
        let server_capture = captured.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            *server_capture.lock().await = read_request(&mut socket).await;
            let body = concat!(
                "data: {\"id\":\"chat-1\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"plan\"}]},{\"type\":\"text\",\"text\":\"Answer \"}]},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chat-1\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"Abc123xyz\",\"index\":0,\"function\":{\"name\":\"calc\",\"arguments\":\"{\\\"x\\\":\"}}]},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chat-1\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"Abc123xyz\",\"index\":0,\"function\":{\"name\":\"calc\",\"arguments\":\"2}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });

        let model = model(format!("http://{address}/ignored"));
        let events = stream_mistral(
            model,
            Context::default(),
            MistralOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    session_id: Some("session-1".into()),
                    ..StreamOptions::default()
                },
                ..MistralOptions::default()
            },
        )
        .await;
        let mut kinds = Vec::new();
        while let Some(event) = events.next().await {
            kinds.push(match event {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
                AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
                AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::ToolCallStart { .. } => "tool_start",
                AssistantMessageEvent::ToolCallDelta { .. } => "tool_delta",
                AssistantMessageEvent::ToolCallEnd { .. } => "tool_end",
                AssistantMessageEvent::Done { .. } => "done",
                AssistantMessageEvent::Error { .. } => "error",
            });
        }
        let result = events.result().await.expect("result");
        assert_eq!(result.response_id.as_deref(), Some("chat-1"));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.usage.input, 7);
        assert_eq!(result.usage.cache_read, 3);
        assert_eq!(result.usage.output, 4);
        assert_eq!(result.usage.total_tokens, 14);
        assert!((result.usage.cost.total - 0.000_016_5).abs() < f64::EPSILON);
        assert!(
            matches!(&result.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "plan")
        );
        assert!(matches!(&result.content[1], ContentBlock::Text { text, .. } if text == "Answer "));
        assert!(
            matches!(&result.content[2], ContentBlock::ToolCall(call) if call.arguments == json!({"x":2}))
        );
        assert_eq!(
            kinds,
            vec![
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "tool_start",
                "tool_delta",
                "tool_delta",
                "tool_end",
                "done",
            ]
        );

        let request = captured.lock().await;
        assert!(
            request.starts_with("POST /v1/chat/completions "),
            "{request}"
        );
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer sk-test"), "{request}");
        assert!(lower.contains("x-affinity: session-1"), "{request}");
        assert_eq!(request_json(&request)["prompt_cache_key"], "session-1");
    }

    #[tokio::test]
    async fn caller_authorization_header_overrides_key_case_insensitively() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let captured = Arc::new(Mutex::new(String::new()));
        let server_capture = captured.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            *server_capture.lock().await = read_request(&mut socket).await;
            let body = "data: {\"id\":\"x\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });
        let mut model = model(format!("http://{address}"));
        model.headers = Some(HashMap::from([(
            "Authorization".into(),
            "Bearer model-secret".into(),
        )]));
        let events = stream_mistral(
            model,
            Context::default(),
            MistralOptions {
                stream: StreamOptions {
                    headers: HashMap::from([(
                        "AUTHORIZATION".into(),
                        "Bearer caller-secret".into(),
                    )]),
                    ..StreamOptions::default()
                },
                ..MistralOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        assert_eq!(
            events.result().await.expect("result").stop_reason,
            StopReason::Stop
        );
        let request = captured.lock().await.to_ascii_lowercase();
        assert!(request.contains("authorization: bearer caller-secret"));
        assert!(!request.contains("model-secret"));
        assert_eq!(request.matches("authorization:").count(), 1);
    }

    #[tokio::test]
    async fn retries_retryable_status_then_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.expect("first accept");
            let _ = read_request(&mut first).await;
            first
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nretry-after-ms: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .expect("first response");
            let (mut second, _) = listener.accept().await.expect("second accept");
            let _ = read_request(&mut second).await;
            let body = "data: {\"id\":\"retry\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            second
                .write_all(response.as_bytes())
                .await
                .expect("second response");
        });
        let events = stream_mistral(
            model(format!("http://{address}")),
            Context::default(),
            MistralOptions {
                stream: StreamOptions {
                    api_key: Some("key".into()),
                    max_retries: 1,
                    ..StreamOptions::default()
                },
                ..MistralOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        assert_eq!(events.result().await.expect("result").text(), "ok");
    }

    #[tokio::test]
    async fn error_fixture_preserves_raw_body_truncation_and_redacts_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut socket).await;
            let body = format!("{{\"detail\":\"sk-secret:{}\"}}", "x".repeat(4_100));
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });
        let events = stream_mistral(
            model(format!("http://{address}")),
            Context::default(),
            MistralOptions {
                stream: StreamOptions {
                    api_key: Some("sk-secret".into()),
                    ..StreamOptions::default()
                },
                ..MistralOptions::default()
            },
        )
        .await;
        let event = events.next().await.expect("error event");
        let AssistantMessageEvent::Error { error, .. } = event else {
            panic!("expected error event");
        };
        let message = error.error_message.expect("message");
        assert!(message.starts_with("Mistral API error (400): {\"detail\":\"[REDACTED]:"));
        assert!(message.contains("... [truncated 123 chars]"), "{message}");
        assert!(!message.contains("sk-secret"));
    }

    #[tokio::test]
    async fn cancellation_ends_stream_as_aborted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let release = Arc::new(tokio::sync::Notify::new());
        let server_release = release.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                .await
                .expect("headers");
            let chunk = b"data: {\"id\":\"cancel\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
            let encoded = format!("{:x}\r\n", chunk.len());
            socket
                .write_all(encoded.as_bytes())
                .await
                .expect("chunk len");
            socket.write_all(chunk).await.expect("chunk");
            socket.write_all(b"\r\n").await.expect("chunk end");
            server_release.notified().await;
        });
        let token = tokio_util::sync::CancellationToken::new();
        let events = stream_mistral(
            model(format!("http://{address}")),
            Context::default(),
            MistralOptions {
                stream: StreamOptions {
                    api_key: Some("key".into()),
                    abort_signal: Some(token.clone()),
                    ..StreamOptions::default()
                },
                ..MistralOptions::default()
            },
        )
        .await;
        loop {
            let event = events.next().await.expect("pre-cancel event");
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                break;
            }
        }
        token.cancel();
        release.notify_waiters();
        let mut terminal = None;
        while let Some(event) = events.next().await {
            if matches!(event, AssistantMessageEvent::Error { .. }) {
                terminal = Some(event);
            }
        }
        assert!(matches!(
            terminal,
            Some(AssistantMessageEvent::Error {
                reason: StopReason::Aborted,
                ..
            })
        ));
        assert_eq!(
            events.result().await.expect("result").stop_reason,
            StopReason::Aborted
        );
    }

    #[test]
    fn missing_auth_error_never_includes_unrelated_headers() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([("X-Secret".into(), "must-not-leak".into())])),
            ..Model::default()
        };
        let error = mistral_api_key(&model, &StreamOptions::default()).expect_err("missing key");
        assert_eq!(error.to_string(), "No API key for provider: custom");
    }

    #[test]
    fn streaming_json_fixture_keeps_complete_prefixes() {
        assert_eq!(parse_streaming_json("{\"a\":1,\"b\":"), json!({"a":1}));
        assert_eq!(
            parse_streaming_json("{\"name\":\"Par"),
            json!({"name":"Par"})
        );
        assert_eq!(parse_streaming_json("{\"x\":2}"), json!({"x":2}));
    }

    #[test]
    fn url_replaces_configured_path_like_installed_sdk() {
        let url = mistral_chat_url(
            &model("https://example.com/custom/base?old=yes".into()),
            &StreamOptions::default(),
        )
        .expect("url");
        assert_eq!(url.as_str(), "https://example.com/v1/chat/completions");
    }

    #[test]
    fn response_headers_hook_shape_uses_public_header_map() {
        let map = HeaderMap::new();
        assert_eq!(headers_map(&map), HashMap::new());
    }
}
