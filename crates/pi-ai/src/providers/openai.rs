use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::common::{
    apply_provider_headers, apply_provider_request, client, consume_sse, error_body, fail,
    insert_header, insert_header_map, is_aborted, notify_response, send_with_retry,
};
use crate::*;

#[derive(Debug, Clone, Default)]
pub struct OpenAIOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub tool_choice: Option<Value>,
}

impl From<StreamOptions> for OpenAIOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

fn compat_string<'a>(model: &'a Model, key: &str) -> Option<&'a str> {
    model.compat.as_ref()?.get(key)?.as_str()
}
fn compat_bool(model: &Model, key: &str) -> Option<bool> {
    model.compat.as_ref()?.get(key)?.as_bool()
}

fn max_tokens_field(model: &Model) -> MaxTokensField {
    match compat_string(model, "maxTokensField") {
        Some("max_tokens") => MaxTokensField::MaxTokens,
        Some("max_completion_tokens") => MaxTokensField::MaxCompletionTokens,
        _ if matches!(
            model.provider.as_str(),
            "moonshotai"
                | "moonshotai-cn"
                | "together"
                | "zai"
                | "zai-coding-cn"
                | "ant-ling"
                | "cloudflare-ai-gateway"
                | "nvidia"
        ) =>
        {
            MaxTokensField::MaxTokens
        }
        _ => MaxTokensField::MaxCompletionTokens,
    }
}

fn thinking_format(model: &Model) -> &str {
    compat_string(model, "thinkingFormat").unwrap_or_else(|| match model.provider.as_str() {
        "deepseek" => "deepseek",
        "zai" | "zai-coding-cn" => "zai",
        "qwen-token-plan" | "qwen-token-plan-cn" => "qwen",
        "openrouter" => "openrouter",
        "together" => "together",
        "ant-ling" => "ant-ling",
        _ => "openai",
    })
}

fn supports_reasoning_effort(model: &Model) -> bool {
    compat_bool(model, "supportsReasoningEffort").unwrap_or(!matches!(
        model.provider.as_str(),
        "zai"
            | "zai-coding-cn"
            | "moonshotai"
            | "moonshotai-cn"
            | "together"
            | "ant-ling"
            | "cloudflare-ai-gateway"
            | "nvidia"
            | "xai"
    ))
}

fn supports_finish_reason(model: &Model) -> bool {
    compat_bool(model, "supportsFinishReason").unwrap_or(true)
}

fn supports_strict_mode(model: &Model) -> bool {
    compat_bool(model, "supportsStrictMode").unwrap_or(!matches!(
        model.provider.as_str(),
        "moonshotai" | "moonshotai-cn" | "together" | "cloudflare-ai-gateway" | "nvidia"
    ))
}

fn supports_openai_grammar_tools(model: &Model) -> bool {
    compat_bool(model, "supportsOpenAIGrammarTools").unwrap_or(false)
}

fn requires_reasoning_content_on_assistant_messages(model: &Model) -> bool {
    compat_bool(model, "requiresReasoningContentOnAssistantMessages").unwrap_or_else(|| {
        model.provider == "deepseek" || model.base_url.contains("deepseek.com")
    })
}

fn supports_image_input(model: &Model) -> bool {
    model.input.iter().any(|input| input == "image")
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
        return Err(anyhow!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(false)
}

struct GrammarSampling<'a> {
    syntax: &'static str,
    definition: &'a str,
    input_property: &'a str,
}

fn infer_grammar_input_property(tool: &Tool) -> Result<&str> {
    if tool.parameters.schema_type.as_ref().and_then(Value::as_str) != Some("object")
        || tool.parameters.nullable
    {
        return Err(anyhow!(
            "grammar constrained sampling requires an object parameter schema"
        ));
    }
    if tool.parameters.required.len() != 1 {
        return Err(anyhow!(
            "grammar constrained sampling requires exactly one required string property"
        ));
    }
    let property = tool.parameters.required[0].as_str();
    let schema = tool.parameters.properties.get(property).ok_or_else(|| {
        anyhow!("grammar constrained sampling requires a properties entry for {property}")
    })?;
    if schema.schema_type.as_ref().and_then(Value::as_str) != Some("string") || schema.nullable {
        return Err(anyhow!(
            "grammar constrained sampling property {property} must have type string"
        ));
    }
    Ok(property)
}

fn resolve_grammar_sampling(tool: &Tool, supported: bool) -> Result<Option<GrammarSampling<'_>>> {
    let variants = match &tool.constrained_sampling {
        Some(ConstrainedSampling::Grammar { variants }) => variants,
        _ => return Ok(None),
    };
    if !supported {
        return Ok(None);
    }
    let (syntax, definition) = if let Some(definition) = variants
        .openai_lark
        .as_deref()
        .filter(|definition| !definition.trim().is_empty())
    {
        ("lark", definition)
    } else if let Some(definition) = variants
        .openai_regex
        .as_deref()
        .filter(|definition| !definition.trim().is_empty())
    {
        ("regex", definition)
    } else {
        return Err(anyhow!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        ));
    };
    let input_property = infer_grammar_input_property(tool).map_err(|error| {
        anyhow!(
            "Tool \"{}\" cannot use grammar constrained sampling: {}.",
            tool.name,
            error
        )
    })?;
    Ok(Some(GrammarSampling {
        syntax,
        definition,
        input_property,
    }))
}

fn grammar_tool_input_properties(
    tools: &[Tool],
    supported: bool,
) -> Result<HashMap<String, String>> {
    let mut properties = HashMap::new();
    for tool in tools {
        if let Some(grammar) = resolve_grammar_sampling(tool, supported)? {
            properties.insert(tool.name.clone(), grammar.input_property.to_owned());
        }
    }
    Ok(properties)
}

fn grammar_tool_input<'a>(call: &'a ToolCall, input_property: &str) -> Result<&'a str> {
    call.arguments
        .get(input_property)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "Grammar tool call \"{}\" requires argument \"{}\" to be a string.",
                call.name,
                input_property
            )
        })
}

fn convert_openai_tools(model: &Model, tools: &[Tool]) -> Result<Vec<Value>> {
    let supports_strict = supports_strict_mode(model);
    let supports_grammar = supports_openai_grammar_tools(model);
    tools
        .iter()
        .map(|tool| {
            if let Some(grammar) = resolve_grammar_sampling(tool, supports_grammar)? {
                return Ok(json!({
                    "type": "custom",
                    "custom": {
                        "name": tool.name,
                        "description": tool.description,
                        "format": {
                            "type": "grammar",
                            "grammar": {"syntax": grammar.syntax, "definition": grammar.definition},
                        },
                    },
                }));
            }
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict)?;
            let mut function = Map::from_iter([
                ("name".into(), json!(tool.name)),
                ("description".into(), json!(tool.description)),
                (
                    "parameters".into(),
                    serde_json::to_value(&tool.parameters).unwrap_or_else(|_| json!({})),
                ),
            ]);
            if supports_strict {
                function.insert("strict".into(), Value::Bool(strict));
            }
            Ok(json!({"type":"function", "function":function}))
        })
        .collect()
}

fn mapped_reasoning_effort<'a>(model: &'a Model, effort: &'a str) -> Option<&'a str> {
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(effort))
    {
        Some(Some(mapped)) => Some(mapped),
        Some(None) => None,
        None => Some(effort),
    }
}

fn effort_value<'a>(model: &'a Model, effort: &'a str) -> &'a str {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(effort))
        .and_then(Option::as_deref)
        .unwrap_or(effort)
}

fn off_effort_or_default<'a>(model: &'a Model, default: &'a str) -> Option<&'a str> {
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get("off"))
    {
        Some(Some(mapped)) => Some(mapped),
        Some(None) => None,
        None => Some(default),
    }
}

fn resolve_chat_template_kwarg(
    model: &Model,
    reasoning_effort: Option<&str>,
    value: &Value,
) -> Option<Value> {
    let Some(variable) = value.as_object() else {
        return Some(value.clone());
    };
    if reasoning_effort.is_none()
        && variable
            .get("omitWhenOff")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    if variable.get("$var").and_then(Value::as_str) == Some("thinking.enabled") {
        return Some(Value::Bool(reasoning_effort.is_some()));
    }
    let level = reasoning_effort.unwrap_or("off");
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(level))
    {
        Some(Some(mapped)) => Some(Value::String(mapped.clone())),
        Some(None) => None,
        None => reasoning_effort.map(|effort| Value::String(effort.to_owned())),
    }
}

fn build_chat_template_kwargs(
    model: &Model,
    reasoning_effort: Option<&str>,
) -> Option<Value> {
    let configured = model
        .compat
        .as_ref()?
        .get("chatTemplateKwargs")?
        .as_object()?;
    let kwargs = configured
        .iter()
        .filter_map(|(key, value)| {
            resolve_chat_template_kwarg(model, reasoning_effort, value)
                .map(|resolved| (key.clone(), resolved))
        })
        .collect::<Map<_, _>>();
    (!kwargs.is_empty()).then_some(Value::Object(kwargs))
}

fn supports_reasoning_off(model: &Model) -> bool {
    !matches!(
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off")),
        Some(None)
    )
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

fn sanitize_openai_tool_call_id_part(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(char::from(DIGITS[(value % 36) as usize]));
        value /= 36;
    }
    digits.iter().rev().collect()
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdeadbeef_u32;
    let mut h2 = 0x41c6ce57_u32;
    for unit in value.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ ch).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn normalize_openai_tool_call_id(model: &Model, id: &str) -> String {
    if let Some((call_id, item_id)) = id.split_once('|') {
        let call_id = sanitize_openai_tool_call_id_part(call_id);
        let item_id = sanitize_openai_tool_call_id_part(item_id);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if combined.len() <= 40 {
            return combined;
        }
        let hash = short_hash(id);
        let hash = &hash[..hash.len().min(8)];
        let prefix_len = (40_usize.saturating_sub(hash.len() + 1))
            .max(1)
            .min(call_id.len());
        return format!("{}_{}", &call_id[..prefix_len], hash);
    }
    if model.provider == "openai" {
        id.chars().take(40).collect()
    } else {
        id.to_owned()
    }
}

fn apply_reasoning_params(payload: &mut Value, model: &Model, reasoning_effort: Option<&str>) {
    match thinking_format(model) {
        "deepseek" if model.reasoning => {
            if reasoning_effort.is_some() {
                payload["thinking"] = json!({"type":"enabled"});
            } else if supports_reasoning_off(model) {
                payload["thinking"] = json!({"type":"disabled"});
            }
            if supports_reasoning_effort(model) {
                if let Some(effort) =
                    reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
                {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
        }
        "zai" if model.reasoning => {
            payload["thinking"] = if reasoning_effort.is_some() {
                json!({"type":"enabled", "clear_thinking":false})
            } else {
                json!({"type":"disabled"})
            };
            if supports_reasoning_effort(model) {
                if let Some(effort) =
                    reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
                {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
        }
        "qwen" if model.reasoning => {
            payload["enable_thinking"] = json!(reasoning_effort.is_some());
            if supports_reasoning_effort(model) {
                if let Some(effort) =
                    reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
                {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
        }
        "qwen-chat-template" if model.reasoning => {
            payload["chat_template_kwargs"] = json!({
                "enable_thinking": reasoning_effort.is_some(),
                "preserve_thinking": true,
            });
        }
        "chat-template" if model.reasoning => {
            if let Some(kwargs) = build_chat_template_kwargs(model, reasoning_effort) {
                payload["chat_template_kwargs"] = kwargs;
            }
        }
        "openrouter" if model.reasoning => {
            if let Some(effort) =
                reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
            {
                payload["reasoning"] = json!({"effort":effort});
            } else if reasoning_effort.is_none() && supports_reasoning_off(model) {
                payload["reasoning"] = json!({"effort":"none"});
            }
        }
        "ant-ling" if model.reasoning && reasoning_effort.is_some() => {
            if let Some(effort) = reasoning_effort
                .and_then(|effort| model.thinking_level_map.as_ref()?.get(effort))
                .and_then(Option::as_deref)
            {
                payload["reasoning"] = json!({"effort":effort});
            }
        }
        "together" if model.reasoning => {
            payload["reasoning"] = json!({"enabled":reasoning_effort.is_some()});
            if supports_reasoning_effort(model) {
                if let Some(effort) =
                    reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
                {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
        }
        "string-thinking" if model.reasoning => {
            if let Some(effort) = reasoning_effort {
                payload["thinking"] = json!(effort_value(model, effort));
            } else if let Some(off) = off_effort_or_default(model, "none") {
                payload["thinking"] = json!(off);
            }
        }
        _ => {
            if supports_reasoning_effort(model) {
                if let Some(effort) =
                    reasoning_effort.and_then(|effort| mapped_reasoning_effort(model, effort))
                {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
        }
    }
}

fn build_openai_payload(
    model: &Model,
    context: &Context,
    options: &OpenAIOptions,
) -> Result<Value> {
    let messages = openai_messages(model, context)?;
    let mut payload = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(max_tokens) = options.stream.max_tokens {
        payload[match max_tokens_field(model) {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        }] = json!(max_tokens);
    }
    if let Some(temperature) = options.stream.temperature {
        payload["temperature"] = json!(temperature);
    }
    apply_reasoning_params(&mut payload, model, options.reasoning_effort.as_deref());
    if let Some(tool_choice) = &options.tool_choice {
        payload["tool_choice"] = tool_choice.clone();
    }
    if !context.tools.is_empty() {
        payload["tools"] = Value::Array(convert_openai_tools(model, &context.tools)?);
    }
    Ok(payload)
}

pub fn register_openai_completions() {
    let stream_simple: SimpleStreamFn = Arc::new(|model, context, options| {
        async move { stream_simple_openai_completions(model, context, options).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_OPENAI_COMPLETIONS.into(),
            stream: Arc::new(|model, context, options| {
                async move { stream_openai_completions(model, context, options.into()).await }
                    .boxed()
            }),
            stream_simple,
        },
        None,
    );
}

pub async fn stream_simple_openai_completions(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let reasoning_effort = options
        .reasoning
        .map(|level| clamp_thinking_level(&model, thinking_level_name(level)).to_owned());
    let mut stream = options.stream;
    let max_tokens = stream.max_tokens.unwrap_or(model.max_tokens);
    stream.max_tokens = Some(clamp_max_tokens_to_context(&model, &context, max_tokens));
    stream_openai_completions(
        model,
        context,
        OpenAIOptions {
            stream,
            reasoning_effort,
            tool_choice: None,
        },
    )
    .await
}

pub async fn stream_openai_completions(
    model: Model,
    context: Context,
    options: OpenAIOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let task_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        let result = run_openai_stream(&task_stream, &model, &context, &options, &mut output).await;
        if let Err(error) = result {
            fail(
                &task_stream,
                output,
                error.to_string(),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
        task_stream
            .push(AssistantMessageEvent::Done {
                reason: output.stop_reason,
                message: output.clone(),
            })
            .await;
        task_stream.end(Some(output)).await;
    });
    stream
}

fn openai_chat_url(model: &Model, options: &StreamOptions) -> Result<String> {
    let base_url = resolve_base_url(&model.base_url, &options.env)?;
    Ok(format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ))
}

fn has_effective_openai_auth_header(
    model: &Model,
    options: &StreamOptions,
    names: &[&str],
) -> bool {
    names.iter().any(|name| {
        let mut value = None;
        if let Some(model_headers) = &model.headers {
            for (header, candidate) in model_headers {
                if header.eq_ignore_ascii_case(name) {
                    value = Some(candidate.as_str());
                }
            }
        }
        for (header, candidate) in &options.headers {
            if header.eq_ignore_ascii_case(name) {
                value = Some(candidate.as_str());
            }
        }
        value.is_some_and(|value| !value.trim().is_empty())
    })
}

fn openai_api_key<'a>(model: &Model, options: &'a StreamOptions) -> Result<&'a str> {
    if let Some(api_key) = options
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
    {
        return Ok(api_key);
    }
    let recognized = if model.provider == "cloudflare-ai-gateway" {
        &["cf-aig-authorization"][..]
    } else {
        &["authorization", "cf-aig-authorization"][..]
    };
    if has_effective_openai_auth_header(model, options, recognized) {
        Ok("")
    } else {
        Err(anyhow!("No API key for provider: {}", model.provider))
    }
}

fn openai_request_headers(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    api_key: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    // Install-telemetry attribution stays disabled until StreamOptions exposes a verified setting.
    if let Some(attribution) = merge_provider_attribution_headers(
        model,
        options.session_id.as_deref(),
        false,
        &HashMap::new(),
    ) {
        insert_header_map(&mut headers, &attribution)?;
    }
    insert_header(&mut headers, "accept", "text/event-stream")?;
    insert_header(&mut headers, "content-type", "application/json")?;
    if model.provider != "cloudflare-ai-gateway" && !api_key.trim().is_empty() {
        insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    }
    if let Some(model_headers) = &model.headers {
        insert_header_map(&mut headers, model_headers)?;
    }
    if let Some(copilot_headers) = crate::github_copilot::dynamic_headers(&model.provider, context)
    {
        insert_header_map(&mut headers, &copilot_headers)?;
    }
    insert_header_map(&mut headers, &options.headers)?;
    if model.provider == "cloudflare-ai-gateway" {
        headers.remove("authorization");
        headers.remove("x-api-key");
        if !api_key.trim().is_empty() {
            insert_header(
                &mut headers,
                "cf-aig-authorization",
                &format!("Bearer {api_key}"),
            )?;
        }
    }
    Ok(headers)
}

async fn run_openai_stream(
    stream: &AssistantMessageEventStream,
    model: &Model,
    context: &Context,
    options: &OpenAIOptions,
    output: &mut AssistantMessage,
) -> Result<()> {
    let api_key = openai_api_key(model, &options.stream)?;
    let grammar_input_properties =
        grammar_tool_input_properties(&context.tools, supports_openai_grammar_tools(model))?;
    let payload = apply_provider_request(
        build_openai_payload(model, context, options)?,
        model,
        &options.stream,
    )
    .await?;
    let url = openai_chat_url(model, &options.stream)?;
    let request_headers = apply_provider_headers(
        openai_request_headers(model, context, &options.stream, api_key)?,
        model,
        &options.stream,
    )
    .await?;
    let http = client(&options.stream)?;
    let response = send_with_retry(&options.stream, || {
        http.post(&url)
            .json(&payload)
            .headers(request_headers.clone())
    })
    .await?;
    notify_response(&options.stream, &response, model).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            error_body("OpenAI", response, &options.stream).await?
        ));
    }

    stream
        .push(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;
    let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
    let event_stream = stream.clone();
    let event_task = tokio::spawn(async move {
        while let Some(event) = event_receiver.recv().await {
            event_stream.push(event).await;
        }
    });
    let mut state = ChatStreamState::new(grammar_input_properties);
    let parse_result = consume_sse(response, &options.stream, |_, input| {
        if input == "[DONE]" {
            return Ok(());
        }
        let value =
            parse_json_with_repair(input).ok_or_else(|| anyhow!("Invalid OpenAI SSE JSON"))?;
        state.apply(serde_json::from_value(value)?, output, &event_sender)
    })
    .await;
    if parse_result.is_ok() {
        state.finish(output, &event_sender)?;
    }
    drop(event_sender);
    event_task
        .await
        .map_err(|error| anyhow!("OpenAI event forwarder failed: {error}"))?;
    parse_result?;

    if output.stop_reason == StopReason::Pending {
        if supports_finish_reason(model) {
            return Err(anyhow!("OpenAI stream ended without finish_reason"));
        }
        output.stop_reason = if output
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if output.stop_reason == StopReason::Error {
        return Err(anyhow!(
            "OpenAI stream ended with unsupported finish reason"
        ));
    }
    calculate_cost(model, &mut output.usage);
    Ok(())
}

fn openai_messages(model: &Model, context: &Context) -> Result<Vec<Value>> {
    let grammar_input_properties =
        grammar_tool_input_properties(&context.tools, supports_openai_grammar_tools(model))?;
    let mut messages = Vec::new();
    if !context.system_prompt.is_empty() {
        messages.push(json!({"role":"system", "content":context.system_prompt}));
    }
    let transformed = transform_messages(&context.messages, model, |id, target, _| {
        normalize_openai_tool_call_id(target, id)
    });
    let mut index = 0;
    while index < transformed.len() {
        match &transformed[index] {
            Message::User(user) => {
                messages.push(json!({"role":"user", "content":openai_content(&user.content)}));
            }
            Message::Assistant(assistant) => {
                let mut wire = Map::new();
                wire.insert("role".into(), json!("assistant"));
                let text = assistant.text();
                wire.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    let ContentBlock::ToolCall(call) = block else {
                        continue;
                    };
                    if let Some(input_property) = grammar_input_properties.get(&call.name) {
                        let input = grammar_tool_input(call, input_property)?;
                        tool_calls.push(json!({
                            "id": call.id,
                            "type": "custom",
                            "custom": {"name":call.name, "input":input},
                        }));
                    } else {
                        tool_calls.push(json!({
                            "id": call.id,
                            "type": "function",
                            "function": {"name":call.name, "arguments":serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into())},
                        }));
                    }
                }
                if !tool_calls.is_empty() {
                    wire.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                let reasoning_details = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => call
                            .thought_signature
                            .as_ref()
                            .and_then(|signature| serde_json::from_str::<Value>(signature).ok()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !reasoning_details.is_empty() {
                    wire.insert("reasoning_details".into(), Value::Array(reasoning_details));
                }
                let reasoning = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !reasoning.is_empty() {
                    wire.insert("reasoning_content".into(), json!(reasoning));
                }
                if requires_reasoning_content_on_assistant_messages(model)
                    && model.reasoning
                    && !wire.contains_key("reasoning_content")
                {
                    wire.insert("reasoning_content".into(), json!(""));
                }
                messages.push(Value::Object(wire));
            }
            Message::ToolResult(_) => {
                let mut image_parts = Vec::new();
                while index < transformed.len() {
                    let Message::ToolResult(tool_result) = &transformed[index] else {
                        break;
                    };
                    messages.push(json!({
                        "role":"tool",
                        "tool_call_id":tool_result.tool_call_id,
                        "content":tool_result_text(&tool_result.content),
                    }));
                    if supports_image_input(model) {
                        image_parts.extend(tool_result.content.iter().filter_map(|block| {
                            let ContentBlock::Image { data, mime_type } = block else {
                                return None;
                            };
                            Some(json!({
                                "type":"image_url",
                                "image_url":{"url":format!("data:{mime_type};base64,{data}")},
                            }))
                        }));
                    }
                    index += 1;
                }
                if !image_parts.is_empty() {
                    let mut content = Vec::with_capacity(image_parts.len() + 1);
                    content.push(json!({
                        "type":"text",
                        "text":"Attached image(s) from tool result:",
                    }));
                    content.extend(image_parts);
                    messages.push(json!({"role":"user", "content":content}));
                }
                continue;
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("provider transforms project session messages")
            }
        }
        index += 1;
    }
    Ok(messages)
}

fn openai_content(content: &[ContentBlock]) -> Value {
    let blocks = content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(json!({"type":"text", "text":text})),
        ContentBlock::Image { data, mime_type } => Some(json!({"type":"image_url", "image_url":{"url":format!("data:{mime_type};base64,{data}")}})),
        _ => None,
    }).collect::<Vec<_>>();
    Value::Array(blocks)
}

fn tool_result_text(content: &[ContentBlock]) -> String {
    let text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text
    } else if content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }))
    {
        "(see attached image)".into()
    } else {
        "(no tool output)".into()
    }
}

#[derive(Deserialize, Default)]
struct ChatChunk {
    id: Option<String>,
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}
#[derive(Deserialize, Default)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    finish_reason: Option<String>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}
#[derive(Deserialize, Default)]
struct ChatDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
    #[serde(default)]
    reasoning_details: Vec<Value>,
}
#[derive(Deserialize, Default)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
    custom: Option<CustomDelta>,
}
impl ToolCallDelta {
    fn name(&self) -> Option<&str> {
        self.function
            .as_ref()
            .and_then(|function| function.name.as_deref())
            .or_else(|| {
                self.custom
                    .as_ref()
                    .and_then(|custom| custom.name.as_deref())
            })
    }

    fn is_custom_call(&self) -> bool {
        self.custom.is_some() && self.function.is_none()
    }
}
#[derive(Deserialize, Default)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}
#[derive(Deserialize, Default)]
struct CustomDelta {
    name: Option<String>,
    #[serde(default)]
    input: String,
}
#[derive(Deserialize, Default)]
struct ChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    prompt_tokens_details: Option<PromptTokenDetails>,
    completion_tokens_details: Option<CompletionTokenDetails>,
}
#[derive(Deserialize, Default)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
}
#[derive(Deserialize, Default)]
struct CompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: i64,
}

#[derive(Default)]
struct ChatStreamState {
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    tools: Vec<ToolSlot>,
    pending_reasoning_details: HashMap<String, String>,
    grammar_input_properties: HashMap<String, String>,
}
struct ToolSlot {
    source_index: Option<usize>,
    id: Option<String>,
    name: String,
    content_index: usize,
    arguments: String,
    grammar: Option<GrammarInputBuffer>,
    args_started: bool,
}
struct GrammarInputBuffer {
    property: String,
    input: String,
    started: bool,
    closed: bool,
}

impl GrammarInputBuffer {
    fn new(property: String) -> Self {
        Self {
            property,
            input: String::new(),
            started: false,
            closed: false,
        }
    }

    fn arguments(&self) -> Value {
        Value::Object(Map::from_iter([(
            self.property.clone(),
            Value::String(self.input.clone()),
        )]))
    }

    fn append(&mut self, input_delta: &str, final_chunk: bool) -> Result<Option<String>> {
        if self.closed {
            if final_chunk && input_delta.is_empty() {
                return Ok(None);
            }
            return Err(anyhow!(
                "grammar tool input for property \"{}\" changed after it was closed",
                self.property
            ));
        }
        if !final_chunk && input_delta.is_empty() {
            return Ok(None);
        }

        let escaped_input = serde_json::to_string(input_delta)?;
        let mut delta = String::new();
        if !self.started {
            delta.push('{');
            delta.push_str(&serde_json::to_string(&self.property)?);
            delta.push_str(":\"");
            self.started = true;
        }
        delta.push_str(&escaped_input[1..escaped_input.len() - 1]);
        self.input.push_str(input_delta);
        if final_chunk {
            delta.push_str("\"}");
            self.closed = true;
        }
        Ok(Some(delta))
    }
}
fn apply_chat_usage(usage: ChatUsage, output: &mut AssistantMessage) {
    let (cached, cache_write) = usage.prompt_tokens_details.map_or((0, 0), |details| {
        (details.cached_tokens, details.cache_write_tokens)
    });
    let reasoning = usage
        .completion_tokens_details
        .map_or(0, |details| details.reasoning_tokens);
    let input = usage
        .prompt_tokens
        .saturating_sub(cached)
        .saturating_sub(cache_write);
    output.usage.input = input;
    output.usage.cache_read = cached;
    output.usage.cache_write = cache_write;
    output.usage.output = usage.completion_tokens;
    output.usage.reasoning = reasoning;
    output.usage.total_tokens = input + usage.completion_tokens + cached + cache_write;
}

impl ChatStreamState {
    fn new(grammar_input_properties: HashMap<String, String>) -> Self {
        Self {
            grammar_input_properties,
            ..Self::default()
        }
    }

    fn apply(
        &mut self,
        chunk: ChatChunk,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        if output.response_id.is_none() {
            output.response_id = chunk.id;
        }
        if output.response_model.is_none() {
            output.response_model = chunk.model;
        }
        let chunk_usage = chunk.usage;
        let Some(choice) = chunk.choices.into_iter().next() else {
            if let Some(usage) = chunk_usage {
                apply_chat_usage(usage, output);
            }
            return Ok(());
        };
        // Top-level usage takes precedence; choice-level usage is a fallback for
        // providers (e.g. some OpenAI-compatible gateways) that report usage inline
        // with the final choice rather than at the chunk root.
        if let Some(usage) = chunk_usage.or(choice.usage) {
            apply_chat_usage(usage, output);
        }
        if let Some(reason) = choice.finish_reason {
            output.raw_stop_reason = Some(reason.clone());
            output.stop_reason = match reason.as_str() {
                "stop" => StopReason::Stop,
                "length" => StopReason::Length,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                _ => StopReason::Error,
            };
        }
        if let Some(delta) = choice.delta.content {
            self.append_text(delta, output, events)?;
        }
        if let Some(delta) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
            self.append_thinking(delta, output, events)?;
        }
        for tool_delta in choice.delta.tool_calls {
            self.append_tool(tool_delta, output, events)?;
        }
        for detail in choice.delta.reasoning_details {
            self.append_reasoning_detail(detail, output);
        }
        Ok(())
    }

    fn append_text(
        &mut self,
        delta: String,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let index = match self.text_index {
            Some(index) => index,
            None => {
                let index = output.content.len();
                output.content.push(ContentBlock::text(""));
                events
                    .send(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
                self.text_index = Some(index);
                index
            }
        };
        if let ContentBlock::Text { text, .. } = &mut output.content[index] {
            text.push_str(&delta);
        }
        events
            .send(AssistantMessageEvent::TextDelta {
                content_index: index,
                delta,
                partial: output.clone(),
            })
            .map_err(|_| anyhow!("OpenAI event stream closed"))
    }

    fn append_thinking(
        &mut self,
        delta: String,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let index = match self.thinking_index {
            Some(index) => index,
            None => {
                let index = output.content.len();
                output.content.push(ContentBlock::thinking(""));
                events
                    .send(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
                self.thinking_index = Some(index);
                index
            }
        };
        if let ContentBlock::Thinking { thinking, .. } = &mut output.content[index] {
            thinking.push_str(&delta);
        }
        events
            .send(AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta,
                partial: output.clone(),
            })
            .map_err(|_| anyhow!("OpenAI event stream closed"))
    }

    fn append_tool(
        &mut self,
        delta: ToolCallDelta,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let name = delta.name().unwrap_or_default().to_owned();
        let is_custom = delta.is_custom_call();
        let slot_pos = self.match_slot(&delta, &name);
        if slot_pos == self.tools.len() {
            let grammar = is_custom.then(|| {
                GrammarInputBuffer::new(
                    self.grammar_input_properties
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| "input".into()),
                )
            });
            let arguments = grammar
                .as_ref()
                .map_or_else(|| json!({}), GrammarInputBuffer::arguments);
            let content_index = output.content.len();
            output.content.push(ContentBlock::ToolCall(ToolCall {
                id: delta.id.clone().unwrap_or_default(),
                name: name.clone(),
                arguments,
                thought_signature: None,
            }));
            events
                .send(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: output.clone(),
                })
                .map_err(|_| anyhow!("OpenAI event stream closed"))?;
            self.tools.push(ToolSlot {
                source_index: delta.index,
                id: delta.id.clone(),
                name: name.clone(),
                content_index,
                arguments: String::new(),
                grammar,
                args_started: false,
            });
        }

        let slot = &mut self.tools[slot_pos];
        let content_index = slot.content_index;
        let mut emitted_delta = None;
        if let ContentBlock::ToolCall(call) = &mut output.content[content_index] {
            if call.id.is_empty() {
                if let Some(id) = delta.id {
                    call.id = id;
                }
            }
            if call.name.is_empty() {
                call.name = name;
            }
            if let Some(signature) = self.pending_reasoning_details.remove(&call.id) {
                call.thought_signature = Some(signature);
            }
            if is_custom && slot.grammar.is_none() {
                slot.arguments.clear();
                slot.grammar = Some(GrammarInputBuffer::new(
                    self.grammar_input_properties
                        .get(&call.name)
                        .cloned()
                        .unwrap_or_else(|| "input".into()),
                ));
                call.arguments = slot
                    .grammar
                    .as_ref()
                    .map_or_else(|| json!({}), GrammarInputBuffer::arguments);
            }
            if let Some(function) = delta.function {
                if call.name.is_empty() {
                    if let Some(name) = function.name {
                        call.name = name;
                    }
                }
                if let Some(arguments) = function.arguments {
                    slot.arguments.push_str(&arguments);
                    call.arguments =
                        serde_json::from_str(&slot.arguments).unwrap_or_else(|_| json!({}));
                    emitted_delta = Some(arguments);
                    slot.args_started = true;
                }
            } else if let Some(custom) = delta.custom {
                if !custom.input.is_empty() {
                    let grammar = slot
                        .grammar
                        .as_mut()
                        .ok_or_else(|| anyhow!("OpenAI custom tool accumulator missing"))?;
                    emitted_delta = grammar.append(&custom.input, false)?;
                    call.arguments = grammar.arguments();
                    slot.args_started = true;
                }
            }
        }
        if let Some(delta) = emitted_delta {
            events
                .send(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta,
                    partial: output.clone(),
                })
                .map_err(|_| anyhow!("OpenAI event stream closed"))?;
        }
        Ok(())
    }

    /// Resolves which accumulated slot a tool-call delta belongs to.
    ///
    /// OpenAI proper tags every delta with `index`. Some compatible providers
    /// omit it; without a stabilizer those calls collapse into a single slot
    /// (every missing index deserialized to the same default) and distinct
    /// tool calls merge. The stabilizer, in priority order:
    ///
    /// 1. `index` present — match by source index (the OpenAI contract).
    /// 2. `id` present — match by tool-call id. Distinct ids never collapse, so
    ///    interleaved no-index calls stay separate.
    /// 3. `name` present (no id) — continue an open no-id slot with the same
    ///    name that has not yet received arguments; otherwise open a new slot.
    ///    The args guard prevents a second same-name call from merging into the
    ///    first once it has started streaming.
    /// 4. pure argument delta (no index/id/name) — continue the most recent
    ///    no-index slot (encounter sequencing).
    fn match_slot(&self, delta: &ToolCallDelta, name: &str) -> usize {
        if let Some(index) = delta.index {
            return self
                .tools
                .iter()
                .position(|slot| slot.source_index == Some(index))
                .unwrap_or(self.tools.len());
        }
        if let Some(id) = delta.id.as_deref().filter(|id| !id.is_empty()) {
            return self
                .tools
                .iter()
                .position(|slot| slot.id.as_deref() == Some(id))
                .unwrap_or(self.tools.len());
        }
        if !name.is_empty() {
            return self
                .tools
                .iter()
                .rposition(|slot| {
                    slot.source_index.is_none()
                        && slot.id.is_none()
                        && slot.name == name
                        && !slot.args_started
                })
                .unwrap_or(self.tools.len());
        }
        self.tools
            .iter()
            .rposition(|slot| slot.source_index.is_none())
            .unwrap_or(self.tools.len())
    }

    fn append_reasoning_detail(&mut self, detail: Value, output: &mut AssistantMessage) {
        let Some(id) = detail
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let encrypted = detail.get("type").and_then(Value::as_str) == Some("reasoning.encrypted")
            && detail
                .get("data")
                .and_then(Value::as_str)
                .is_some_and(|data| !data.is_empty());
        if !encrypted {
            return;
        }
        let Ok(signature) = serde_json::to_string(&detail) else {
            return;
        };
        if let Some(call) = output.content.iter_mut().find_map(|block| match block {
            ContentBlock::ToolCall(call) if call.id == id => Some(call),
            _ => None,
        }) {
            call.thought_signature = Some(signature);
        } else {
            self.pending_reasoning_details
                .insert(id.to_owned(), signature);
        }
    }

    fn finish(
        &mut self,
        output: &mut AssistantMessage,
        events: &mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        if let Some(index) = self.text_index {
            if let ContentBlock::Text { text, .. } = &output.content[index] {
                events
                    .send(AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: text.clone(),
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
            }
        }
        if let Some(index) = self.thinking_index {
            if let ContentBlock::Thinking { thinking, .. } = &output.content[index] {
                events
                    .send(AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: thinking.clone(),
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
            }
        }
        for slot in &mut self.tools {
            let content_index = slot.content_index;
            let trailing = if let Some(grammar) = slot.grammar.as_mut() {
                let trailing = grammar.append("", true)?;
                if let ContentBlock::ToolCall(call) = &mut output.content[content_index] {
                    call.arguments = grammar.arguments();
                }
                trailing
            } else {
                None
            };
            if let Some(delta) = trailing {
                events
                    .send(AssistantMessageEvent::ToolCallDelta {
                        content_index,
                        delta,
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
            }
            if let ContentBlock::ToolCall(call) = &output.content[content_index] {
                events
                    .send(AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call: call.clone(),
                        partial: output.clone(),
                    })
                    .map_err(|_| anyhow!("OpenAI event stream closed"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    #[tokio::test]
    async fn streams_chat_text_and_tool_call() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let request = Arc::new(Mutex::new(String::new()));
        let captured = request.clone();
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
            *captured.lock().await = String::from_utf8(bytes).expect("utf8 request");
            let body = concat!(
                "data: {\"id\":\"chat-1\",\"model\":\"mock\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                "data: [DONE]\n\n"
            );

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            base_url: format!("http://{address}"),
            ..Model::default()
        };
        let events = stream_openai_completions(
            model,
            Context::default(),
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        let result = events.result().await.expect("final result");
        assert_eq!(result.text(), "Hello");
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.usage.total_tokens, 5);
        let request = request.lock().await;
        assert!(request.starts_with("POST /chat/completions "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{request}"
        );
    }

    fn builtin_model(provider: &str, id: &str) -> Model {
        get_model(provider, id).unwrap_or_else(|| panic!("missing builtin model {provider}/{id}"))
    }

    fn payload(model: &Model, effort: Option<&str>, max_tokens: i64) -> Value {
        build_openai_payload(
            model,
            &Context::default(),
            &OpenAIOptions {
                stream: StreamOptions {
                    max_tokens: Some(max_tokens),
                    ..StreamOptions::default()
                },
                reasoning_effort: effort.map(str::to_owned),
                tool_choice: None,
            },
        )
        .expect("payload")
    }

    #[test]
    fn openai_projects_visible_bash_and_excludes_hidden_bash() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            ..Model::default()
        };
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
        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(
            messages,
            vec![
                json!({"role":"user", "content":[{"type":"text", "text":"Ran `echo ok`\n```\nok\n```"}]})
            ]
        );
    }

    #[test]
    fn deepseek_reasoning_payload_uses_thinking_shape() {
        let model = builtin_model("deepseek", "deepseek-v4-pro");
        let payload = payload(&model, Some("high"), 4096);
        assert_eq!(payload["thinking"], json!({"type":"enabled"}));
        assert_eq!(payload["reasoning_effort"], "high");
        assert_eq!(payload["max_completion_tokens"], 4096);
    }

    #[test]
    fn moonshot_payload_uses_max_tokens() {
        let model = builtin_model("moonshotai", "kimi-k2-0711-preview");
        let payload = payload(&model, None, 2048);
        assert_eq!(payload["max_tokens"], 2048);
        assert!(payload.get("max_completion_tokens").is_none());
    }

    #[test]
    fn qwen_reasoning_payload_enables_thinking() {
        let model = builtin_model("qwen-token-plan", "qwen3.6-plus");
        let payload = payload(&model, Some("medium"), 1024);
        assert_eq!(payload["enable_thinking"], true);
        assert!(payload.get("reasoning_effort").is_none());
    }

    #[test]
    fn openrouter_reasoning_payload_nests_effort() {
        let model = builtin_model("openrouter", "anthropic/claude-haiku-4.5");
        let payload = payload(&model, Some("high"), 1024);
        assert_eq!(payload["reasoning"], json!({"effort":"high"}));
        assert!(payload.get("reasoning_effort").is_none());
    }

    #[test]
    fn residual_reasoning_compat_modes_match_installed_pi() {
        let ant_ling = builtin_model("ant-ling", "Ring-2.6-1T");
        assert_eq!(
            payload(&ant_ling, Some("high"), 1024)["reasoning"],
            json!({"effort":"high"})
        );
        assert!(payload(&ant_ling, Some("medium"), 1024)
            .get("reasoning")
            .is_none());

        let string_thinking = Model {
            reasoning: true,
            thinking_level_map: Some(HashMap::from([
                ("off".into(), Some("disabled".into())),
                ("high".into(), Some("mapped-high".into())),
            ])),
            compat: Some(json!({"thinkingFormat":"string-thinking"})),
            ..Model::default()
        };
        assert_eq!(
            payload(&string_thinking, Some("high"), 1024)["thinking"],
            "mapped-high"
        );
        assert_eq!(payload(&string_thinking, None, 1024)["thinking"], "disabled");

        let chat_template = Model {
            reasoning: true,
            thinking_level_map: Some(HashMap::from([
                ("off".into(), Some("none".into())),
                ("high".into(), Some("mapped-high".into())),
            ])),
            compat: Some(json!({
                "thinkingFormat":"chat-template",
                "chatTemplateKwargs":{
                    "thinking":{"$var":"thinking.enabled"},
                    "reasoning_effort":{"$var":"thinking.effort","omitWhenOff":true},
                    "literal":"x",
                    "nullable":null
                }
            })),
            ..Model::default()
        };
        assert_eq!(
            payload(&chat_template, Some("high"), 1024)["chat_template_kwargs"],
            json!({
                "thinking":true,
                "reasoning_effort":"mapped-high",
                "literal":"x",
                "nullable":null
            })
        );
        assert_eq!(
            payload(&chat_template, None, 1024)["chat_template_kwargs"],
            json!({"thinking":false, "literal":"x", "nullable":null})
        );

        let qwen_chat_template = Model {
            reasoning: true,
            compat: Some(json!({"thinkingFormat":"qwen-chat-template"})),
            ..Model::default()
        };
        assert_eq!(
            payload(&qwen_chat_template, Some("high"), 1024)["chat_template_kwargs"],
            json!({"enable_thinking":true, "preserve_thinking":true})
        );
        assert_eq!(
            payload(&qwen_chat_template, None, 1024)["chat_template_kwargs"],
            json!({"enable_thinking":false, "preserve_thinking":true})
        );
    }

    #[test]
    fn deepseek_replay_emits_required_empty_reasoning_content() {
        let model = Model {
            id: "deepseek-chat".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "deepseek".into(),
            reasoning: true,
            ..Model::default()
        };
        let context = Context {
            messages: vec![Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::text("answer")],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                raw_stop_reason: Some("stop".into()),
                timestamp: 1,
            })],
            ..Context::default()
        };
        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(messages[0]["content"], "answer");
        assert_eq!(messages[0]["reasoning_content"], "");
    }

    #[test]
    fn image_tool_results_keep_tool_text_and_follow_with_user_images() {
        let model = Model {
            id: "vision".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "compatible".into(),
            input: vec!["text".into(), "image".into()],
            ..Model::default()
        };
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-image".into(),
                name: "view".into(),
                arguments: json!({}),
                thought_signature: None,
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: Some("tool_calls".into()),
            timestamp: 1,
        };
        let tool_result = ToolResultMessage {
            tool_call_id: "call-image".into(),
            tool_name: "view".into(),
            content: vec![
                ContentBlock::text("diagram"),
                ContentBlock::Image {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            usage: None,
            details: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 2,
        };
        let context = Context {
            messages: vec![Message::Assistant(assistant), Message::ToolResult(tool_result)],
            ..Context::default()
        };
        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1],
            json!({"role":"tool", "tool_call_id":"call-image", "content":"diagram"})
        );
        assert_eq!(
            messages[2],
            json!({
                "role":"user",
                "content":[
                    {"type":"text", "text":"Attached image(s) from tool result:"},
                    {"type":"image_url", "image_url":{"url":"data:image/png;base64,aW1hZ2U="}}
                ]
            })
        );
    }

    #[test]
    fn captured_reasoning_detail_is_replayed_with_tool_call() {
        let signature =
            json!({"type":"reasoning.encrypted","id":"call-1","data":"ciphertext"}).to_string();
        let context = Context {
            messages: vec![Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: json!({"key":"value"}),
                    thought_signature: Some(signature),
                })],
                api: API_OPENAI_COMPLETIONS.into(),
                provider: "openrouter".into(),
                model: "compatible".into(),
                response_model: None,
                response_id: None,
                diagnostics: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: Some("tool_calls".into()),
                timestamp: 0,
            })],
            ..Context::default()
        };
        let messages = openai_messages(
            &Model {
                id: "compatible".into(),
                api: API_OPENAI_COMPLETIONS.into(),
                provider: "openrouter".into(),
                ..Model::default()
            },
            &context,
        )
        .expect("messages");
        assert_eq!(
            messages[0]["reasoning_details"],
            json!([{"type":"reasoning.encrypted","id":"call-1","data":"ciphertext"}])
        );
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call-1");
    }

    #[test]
    fn native_openai_payload_stays_native() {
        let model = Model {
            id: "gpt-5".into(),
            provider: "openai".into(),
            reasoning: true,
            ..Model::default()
        };
        let payload = payload(&model, Some("medium"), 512);
        assert_eq!(payload["max_completion_tokens"], 512);
        assert_eq!(payload["reasoning_effort"], "medium");
        assert!(payload.get("max_tokens").is_none());
        assert!(payload.get("reasoning").is_none());
        assert!(payload.get("thinking").is_none());
        assert!(payload.get("enable_thinking").is_none());
    }

    fn constrained_tool(sampling: ConstrainedSampling) -> Tool {
        Tool {
            name: "query".into(),
            description: "query data".into(),
            parameters: Schema::object(
                HashMap::from([("query".into(), Schema::string())]),
                vec!["query".into()],
            ),
            constrained_sampling: Some(sampling),
        }
    }

    #[test]
    fn openai_json_schema_sampling_respects_capability_and_requirement() {
        let supported = Model {
            provider: "openai".into(),
            ..Model::default()
        };
        let context = Context {
            tools: vec![constrained_tool(ConstrainedSampling::json_schema(
                ConstrainedSamplingStrictness::Prefer,
            ))],
            ..Context::default()
        };
        let payload = build_openai_payload(&supported, &context, &OpenAIOptions::default())
            .expect("strict payload");
        assert_eq!(payload["tools"][0]["function"]["strict"], true);

        let unsupported = Model {
            provider: "moonshotai".into(),
            ..Model::default()
        };
        let preferred = build_openai_payload(&unsupported, &context, &OpenAIOptions::default())
            .expect("prefer fallback");
        assert!(preferred["tools"][0]["function"].get("strict").is_none());

        let required = Context {
            tools: vec![constrained_tool(ConstrainedSampling::json_schema(
                ConstrainedSamplingStrictness::Require,
            ))],
            ..Context::default()
        };
        let error = build_openai_payload(&unsupported, &required, &OpenAIOptions::default())
            .expect_err("require must fail without strict support");
        assert_eq!(
            error.to_string(),
            "Tool \"query\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    #[test]
    fn openai_grammar_sampling_uses_custom_tools_only_when_supported() {
        let context = Context {
            tools: vec![constrained_tool(ConstrainedSampling::grammar(
                GrammarVariants {
                    openai_lark: Some("start: /.+/".into()),
                    openai_regex: None,
                },
            ))],
            ..Context::default()
        };
        let supported = Model {
            compat: Some(json!({"supportsOpenAIGrammarTools":true})),
            ..Model::default()
        };
        let payload = build_openai_payload(&supported, &context, &OpenAIOptions::default())
            .expect("grammar payload");
        assert_eq!(payload["tools"][0]["type"], "custom");
        assert_eq!(
            payload["tools"][0]["custom"]["format"]["grammar"]["syntax"],
            "lark"
        );
        assert_eq!(
            payload["tools"][0]["custom"]["format"]["grammar"]["definition"],
            "start: /.+/"
        );

        let unsupported =
            build_openai_payload(&Model::default(), &context, &OpenAIOptions::default())
                .expect("grammar fallback");
        assert_eq!(unsupported["tools"][0]["type"], "function");

        let regex_context = Context {
            tools: vec![constrained_tool(ConstrainedSampling::grammar(
                GrammarVariants {
                    openai_lark: Some("  ".into()),
                    openai_regex: Some("[a-z]+".into()),
                },
            ))],
            ..Context::default()
        };
        let regex_payload =
            build_openai_payload(&supported, &regex_context, &OpenAIOptions::default())
                .expect("regex grammar payload");
        assert_eq!(
            regex_payload["tools"][0]["custom"]["format"]["grammar"],
            json!({"syntax":"regex", "definition":"[a-z]+"})
        );

        let missing = Context {
            tools: vec![constrained_tool(ConstrainedSampling::grammar(
                GrammarVariants::default(),
            ))],
            ..Context::default()
        };
        let error = build_openai_payload(&supported, &missing, &OpenAIOptions::default())
            .expect_err("supported grammar tool needs a usable variant");
        assert_eq!(
            error.to_string(),
            "Tool \"query\" cannot use grammar constrained sampling: no supported grammar variant was provided."
        );
    }

    #[test]
    fn replays_grammar_tool_calls_as_custom_input_and_keeps_functions() {
        let model = Model {
            compat: Some(json!({"supportsOpenAIGrammarTools":true})),
            ..Model::default()
        };
        let function_tool = Tool {
            name: "lookup".into(),
            description: "lookup data".into(),
            parameters: Schema::object(
                HashMap::from([("key".into(), Schema::string())]),
                vec!["key".into()],
            ),
            constrained_sampling: None,
        };
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "call-custom".into(),
                    name: "query".into(),
                    arguments: json!({"query":"name = \"Ada\""}),
                    thought_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call-function".into(),
                    name: "lookup".into(),
                    arguments: json!({"key":"value"}),
                    thought_signature: None,
                }),
            ],
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: Some("tool_calls".into()),
            timestamp: 0,
        };
        let context = Context {
            tools: vec![
                constrained_tool(ConstrainedSampling::grammar(GrammarVariants {
                    openai_lark: Some("start: /.+/".into()),
                    openai_regex: None,
                })),
                function_tool,
            ],
            messages: vec![Message::Assistant(assistant)],
            ..Context::default()
        };

        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(
            messages[0]["tool_calls"][0],
            json!({
                "id":"call-custom",
                "type":"custom",
                "custom":{"name":"query", "input":"name = \"Ada\""},
            })
        );
        assert_eq!(
            messages[0]["tool_calls"][1],
            json!({
                "id":"call-function",
                "type":"function",
                "function":{"name":"lookup", "arguments":"{\"key\":\"value\"}"},
            })
        );
    }

    #[test]
    fn grammar_replay_requires_the_configured_string_argument() {
        let model = Model {
            compat: Some(json!({"supportsOpenAIGrammarTools":true})),
            ..Model::default()
        };
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-custom".into(),
                name: "query".into(),
                arguments: json!({"query":42}),
                thought_signature: None,
            })],
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: Some("tool_calls".into()),
            timestamp: 0,
        };
        let context = Context {
            tools: vec![constrained_tool(ConstrainedSampling::grammar(
                GrammarVariants {
                    openai_lark: Some("start: /.+/".into()),
                    openai_regex: None,
                },
            ))],
            messages: vec![Message::Assistant(assistant)],
            ..Context::default()
        };
        let error = openai_messages(&model, &context).expect_err("invalid grammar replay");
        assert_eq!(
            error.to_string(),
            "Grammar tool call \"query\" requires argument \"query\" to be a string."
        );
    }

    #[test]
    fn transforms_orphaned_tool_calls_before_wire_conversion() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: json!({}),
                thought_signature: None,
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: Some("tool_calls".into()),
            timestamp: 1,
        };
        let context = Context {
            messages: vec![Message::Assistant(assistant), Message::user_text("next", 2)],
            ..Context::default()
        };
        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call-1");
        assert_eq!(
            messages[1],
            json!({"role":"tool", "tool_call_id":"call-1", "content":"No result provided"})
        );
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn normalizes_cross_api_tool_call_and_result_ids() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call+ab|it/em".into(),
                name: "lookup".into(),
                arguments: json!({}),
                thought_signature: Some("foreign".into()),
            })],
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            model: "gpt-4".into(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: Some("tool_calls".into()),
            timestamp: 1,
        };
        let result = ToolResultMessage {
            tool_call_id: "call+ab|it/em".into(),
            tool_name: "lookup".into(),
            content: vec![ContentBlock::text("ok")],
            usage: None,
            details: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 2,
        };
        let context = Context {
            messages: vec![Message::Assistant(assistant), Message::ToolResult(result)],
            ..Context::default()
        };
        let messages = openai_messages(&model, &context).expect("messages");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_ab_it_em");
        assert_eq!(messages[1]["tool_call_id"], "call_ab_it_em");
        assert!(messages[0].get("reasoning_details").is_none());
    }

    #[test]
    fn resolves_cloudflare_chat_url_or_names_missing_env() {
        let model = Model { base_url:"https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai".into(), ..Model::default() };
        let missing = openai_chat_url(&model, &StreamOptions::default())
            .expect_err("missing Cloudflare environment");
        assert!(missing.to_string().contains(CLOUDFLARE_ACCOUNT_ID_ENV));
        let options = StreamOptions {
            env: HashMap::from([
                (CLOUDFLARE_ACCOUNT_ID_ENV.into(), "account".into()),
                (CLOUDFLARE_GATEWAY_ID_ENV.into(), "gateway".into()),
            ]),
            ..StreamOptions::default()
        };
        assert_eq!(
            openai_chat_url(&model, &options).expect("resolved URL"),
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai/chat/completions"
        );
    }

    #[test]
    fn request_headers_are_case_insensitive_single_valued_and_caller_wins() {
        let model = Model {
            provider: "opencode".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            headers: Some(HashMap::from([
                ("authorization".into(), "Bearer model".into()),
                ("x-model".into(), "model".into()),
                ("x-opencode-client".into(), "model-client".into()),
            ])),
            ..Model::default()
        };
        let options = StreamOptions {
            session_id: Some("session".into()),
            headers: HashMap::from([
                ("AUTHORIZATION".into(), "Bearer request".into()),
                ("X-MODEL".into(), "request".into()),
                ("x-opencode-client".into(), "request-client".into()),
                ("x-request".into(), "request".into()),
            ]),
            ..StreamOptions::default()
        };
        let headers = openai_request_headers(&model, &Context::default(), &options, "provider-key")
            .expect("headers");
        let request = client(&options)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(request.headers().get_all("authorization").iter().count(), 1);
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer request")
        );
        assert_eq!(
            request
                .headers()
                .get("x-opencode-session")
                .and_then(|value| value.to_str().ok()),
            Some("session")
        );
        assert_eq!(
            request
                .headers()
                .get("x-opencode-client")
                .and_then(|value| value.to_str().ok()),
            Some("request-client")
        );
        assert_eq!(
            request
                .headers()
                .get("x-model")
                .and_then(|value| value.to_str().ok()),
            Some("request")
        );
        assert_eq!(
            request
                .headers()
                .get("x-request")
                .and_then(|value| value.to_str().ok()),
            Some("request")
        );
        assert!(!request.headers().contains_key("HTTP-Referer"));
    }

    #[test]
    fn header_only_auth_skips_openai_default_credential() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([(
                "AuThOrIzAtIoN".into(),
                "Bearer model-secret".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("AUTHORIZATION".into(), "Bearer caller-secret".into())]),
            ..StreamOptions::default()
        };
        let api_key = openai_api_key(&model, &options).expect("header-owned auth");
        assert!(api_key.is_empty());
        let headers = openai_request_headers(&model, &Context::default(), &options, api_key)
            .expect("headers");
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer caller-secret")
        );
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
    }

    #[test]
    fn no_key_or_openai_auth_header_returns_sanitized_error() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([
                ("Authorization".into(), " ".into()),
                ("X-Secret".into(), "must-not-leak".into()),
            ])),
            ..Model::default()
        };
        let error = openai_api_key(&model, &StreamOptions::default()).expect_err("missing auth");
        assert_eq!(error.to_string(), "No API key for provider: custom");
        assert!(!error.to_string().contains("must-not-leak"));
    }

    #[test]
    fn cloudflare_request_uses_gateway_authorization_only() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([
                ("Authorization".into(), "Bearer leaked".into()),
                ("X-API-KEY".into(), "leaked".into()),
                ("CF-AIG-AUTHORIZATION".into(), "Bearer caller".into()),
            ]),
            ..StreamOptions::default()
        };
        let headers = openai_request_headers(&model, &Context::default(), &options, "gateway-key")
            .expect("headers");
        let request = client(&options)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(
            request
                .headers()
                .get_all("cf-aig-authorization")
                .iter()
                .count(),
            1
        );
        assert_eq!(
            request
                .headers()
                .get("cf-aig-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer gateway-key")
        );
        assert!(!request.headers().contains_key("authorization"));
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn cloudflare_header_only_auth_is_not_overwritten_with_empty_key() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("CF-AIG-AUTHORIZATION".into(), "Bearer caller".into())]),
            ..StreamOptions::default()
        };
        let api_key = openai_api_key(&model, &options).expect("header-owned auth");
        assert!(api_key.is_empty());
        let headers = openai_request_headers(&model, &Context::default(), &options, api_key)
            .expect("headers");
        assert_eq!(
            headers
                .get("cf-aig-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer caller")
        );
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
    }

    #[test]
    fn openai_copilot_headers_and_bearer_auth_match_request_context() {
        let model = Model {
            provider: "github-copilot".into(),
            ..Model::default()
        };
        let context = Context {
            messages: vec![Message::User(UserMessage {
                content: vec![ContentBlock::Image {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                }],
                timestamp: 1,
            })],
            ..Context::default()
        };
        let options = StreamOptions::default();
        let headers =
            openai_request_headers(&model, &context, &options, "copilot-token").expect("headers");
        let request = client(&options)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer copilot-token")
        );
        assert_eq!(
            request
                .headers()
                .get("x-initiator")
                .and_then(|value| value.to_str().ok()),
            Some("user")
        );
        assert_eq!(
            request
                .headers()
                .get("openai-intent")
                .and_then(|value| value.to_str().ok()),
            Some("conversation-edits")
        );
        assert_eq!(
            request
                .headers()
                .get("copilot-vision-request")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn openai_tool_call_id_normalization_preserves_uniqueness_and_bounds() {
        let model = Model {
            provider: "github-copilot".into(),
            ..Model::default()
        };
        assert_eq!(
            normalize_openai_tool_call_id(&model, "call_1|A"),
            "call_1_A"
        );
        assert_eq!(
            normalize_openai_tool_call_id(&model, "call_1|B"),
            "call_1_B"
        );
        assert_eq!(
            normalize_openai_tool_call_id(
                &model,
                &format!("{}|{}", "c".repeat(20), "d".repeat(20))
            ),
            format!("{}_srm525dv", "c".repeat(20))
        );
    }

    #[tokio::test]
    async fn repairs_sse_chunk_json() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.expect("read mock request");
            let body = concat!(
                "data: {\"id\":\"chat-repaired\",\"choices\":[{\"delta\":{\"content\":\"Repaired\"}}],}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "compatible".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "compatible".into(),
            base_url: format!("http://{address}"),
            ..Model::default()
        };
        let events = stream_openai_completions(
            model,
            Context::default(),
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        let result = events.result().await.expect("final result");
        assert_eq!(result.text(), "Repaired");
        assert_eq!(result.stop_reason, StopReason::Stop);
    }

    #[tokio::test]
    async fn configured_provider_accepts_valid_stream_without_finish_reason() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.expect("read mock request");
            let body = concat!(
                "data: {\"id\":\"chat-compatible\",\"choices\":[{\"delta\":{\"content\":\"Complete\"}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "compatible".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "compatible".into(),
            base_url: format!("http://{address}"),
            compat: Some(json!({"supportsFinishReason":false})),
            ..Model::default()
        };
        let events = stream_openai_completions(
            model,
            Context::default(),
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        let result = events.result().await.expect("final result");
        assert_eq!(result.text(), "Complete");
        assert_eq!(result.stop_reason, StopReason::Stop);
    }

    #[tokio::test]
    async fn streams_custom_grammar_tool_input_as_json_argument_deltas() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.expect("read mock request");
            let body = concat!(
                "data: {\"id\":\"chat-custom\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-custom\",\"type\":\"custom\",\"custom\":{\"name\":\"query\",\"input\":\"name = \\\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"custom\":{\"input\":\"Ada\\n\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"custom\":{\"input\":\"Lovelace\\\"\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "compatible".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "compatible".into(),
            base_url: format!("http://{address}"),
            compat: Some(json!({"supportsOpenAIGrammarTools":true})),
            ..Model::default()
        };
        let context = Context {
            tools: vec![constrained_tool(ConstrainedSampling::grammar(
                GrammarVariants {
                    openai_lark: Some("start: /.+/".into()),
                    openai_regex: None,
                },
            ))],
            ..Context::default()
        };
        let events = stream_openai_completions(
            model,
            context,
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        let mut seen = Vec::new();
        while let Some(event) = events.next().await {
            seen.push(event);
        }
        let result = events.result().await.expect("final result");
        let call = result
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.id, "call-custom");
        assert_eq!(call.name, "query");
        assert_eq!(call.arguments, json!({"query":"name = \"Ada\nLovelace\""}));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert!(
            matches!(&seen[1], AssistantMessageEvent::ToolCallStart { partial, .. } if matches!(&partial.content[0], ContentBlock::ToolCall(call) if call.arguments == json!({"query":""})))
        );
        let deltas = seen
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            deltas,
            vec!["{\"query\":\"name = \\\"", "Ada\\n", "Lovelace\\\"", "\"}"]
        );
        assert!(seen.iter().any(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { tool_call, .. } if tool_call == call)));
    }

    #[tokio::test]
    async fn streamed_function_tool_with_custom_object_keeps_function_arguments() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.expect("read mock request");
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"key\\\":\"},\"custom\":{}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"value\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "compatible".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "compatible".into(),
            base_url: format!("http://{address}"),
            ..Model::default()
        };
        let events = stream_openai_completions(
            model,
            Context::default(),
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        let mut deltas = Vec::new();
        while let Some(event) = events.next().await {
            if let AssistantMessageEvent::ToolCallDelta { delta, .. } = event {
                deltas.push(delta);
            }
        }
        let result = events.result().await.expect("final result");
        let call = result
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.arguments, json!({"key":"value"}));
        assert_eq!(deltas, vec!["{\"key\":", "\"value\"}"]);
    }

    #[tokio::test]
    async fn captures_encrypted_reasoning_detail_on_tool_call() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.expect("read mock request");
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"id\":\"call-1\",\"data\":\"ciphertext\"}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"key\\\":\\\"value\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
        });
        let model = Model {
            id: "compatible".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "openrouter".into(),
            base_url: format!("http://{address}"),
            ..Model::default()
        };
        let events = stream_openai_completions(
            model,
            Context::default(),
            OpenAIOptions {
                stream: StreamOptions {
                    api_key: Some("sk-test".into()),
                    ..StreamOptions::default()
                },
                ..OpenAIOptions::default()
            },
        )
        .await;
        while events.next().await.is_some() {}
        let result = events.result().await.expect("final result");
        let call = result
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.arguments, json!({"key":"value"}));
        assert_eq!(
            call.thought_signature
                .as_deref()
                .map(|signature| serde_json::from_str::<Value>(signature).expect("signature json")),
            Some(json!({"type":"reasoning.encrypted","id":"call-1","data":"ciphertext"}))
        );
    }
    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).expect("chat chunk")
    }

    fn drive(chunks: Vec<ChatChunk>) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
        let mut state = ChatStreamState::new(HashMap::new());
        let mut output = AssistantMessage::pending(&Model::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        for chunk in chunks {
            state.apply(chunk, &mut output, &tx).expect("apply");
        }
        state.finish(&mut output, &tx).expect("finish");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (output, events)
    }

    fn signature(event: &AssistantMessageEvent) -> String {
        match event {
            AssistantMessageEvent::Start { .. } => "Start".into(),
            AssistantMessageEvent::TextStart { content_index, .. } => {
                format!("TextStart:{content_index}")
            }
            AssistantMessageEvent::TextDelta { content_index, .. } => {
                format!("TextDelta:{content_index}")
            }
            AssistantMessageEvent::TextEnd { content_index, .. } => {
                format!("TextEnd:{content_index}")
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                format!("ThinkingStart:{content_index}")
            }
            AssistantMessageEvent::ThinkingDelta { content_index, .. } => {
                format!("ThinkingDelta:{content_index}")
            }
            AssistantMessageEvent::ThinkingEnd { content_index, .. } => {
                format!("ThinkingEnd:{content_index}")
            }
            AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                format!("ToolCallStart:{content_index}")
            }
            AssistantMessageEvent::ToolCallDelta { content_index, .. } => {
                format!("ToolCallDelta:{content_index}")
            }
            AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
                format!("ToolCallEnd:{content_index}")
            }
            AssistantMessageEvent::Done { .. } => "Done".into(),
            AssistantMessageEvent::Error { .. } => "Error".into(),
        }
    }

    fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn no_index_interleaved_tool_calls_stay_distinct() {
        // Two tool calls streamed without `index`, interleaved across chunks. Each
        // delta carries its `id`, so the stabilizer keys by id and the calls must
        // not collapse into a single accumulator (the old default-index behavior).
        let (output, events) = drive(vec![
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-a","function":{"name":"foo","arguments":"{\"a\":"}}]}}]}"#,
            ),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-b","function":{"name":"bar","arguments":"{\"b\":"}}]}}]}"#,
            ),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-a","function":{"arguments":"1}"}}]}}]}"#,
            ),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-b","function":{"arguments":"2}"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
        ]);
        let calls = tool_calls(&output);
        assert_eq!(
            calls.len(),
            2,
            "no-index calls must not collapse: {calls:?}"
        );
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(calls[0].name, "foo");
        assert_eq!(calls[0].arguments, json!({"a":1}));
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(calls[1].name, "bar");
        assert_eq!(calls[1].arguments, json!({"b":2}));
        // ToolCallEnd events emit in provider source order (a then b).
        let ends = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ends, vec!["call-a", "call-b"]);
    }

    #[test]
    fn no_index_name_keyed_calls_stay_distinct_without_ids() {
        // No `index` and no `id`: the stabilizer keys by name. Two same-name calls
        // arrive sequentially; the args guard prevents the second from merging into
        // the first once it has started streaming arguments.
        let (output, _events) = drive(vec![
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"foo","arguments":"{\"a\":"}}]}}]}"#,
            ),
            chunk(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"1}"}}]}}]}"#),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"foo","arguments":"{\"b\":"}}]}}]}"#,
            ),
            chunk(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"2}"}}]}}]}"#),
            chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        ]);
        let calls = tool_calls(&output);
        assert_eq!(
            calls.len(),
            2,
            "same-name no-id calls must not collapse: {calls:?}"
        );
        assert_eq!(calls[0].arguments, json!({"a":1}));
        assert_eq!(calls[1].arguments, json!({"b":2}));
    }

    #[test]
    fn tool_call_event_order_is_deterministic_across_runs() {
        let sources = [
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-a","function":{"name":"foo","arguments":"{\"a\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call-b","function":{"name":"bar","arguments":"{\"b\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let parse = || {
            sources
                .iter()
                .map(|source| chunk(source))
                .collect::<Vec<_>>()
        };
        let reference = drive(parse()).1.iter().map(signature).collect::<Vec<_>>();
        assert_eq!(
            reference,
            vec![
                "TextStart:0",
                "TextDelta:0",
                "ToolCallStart:1",
                "ToolCallDelta:1",
                "ToolCallStart:2",
                "ToolCallDelta:2",
                "ToolCallDelta:1",
                "ToolCallDelta:2",
                "TextEnd:0",
                "ToolCallEnd:1",
                "ToolCallEnd:2",
            ]
        );
        for run in 0..5 {
            let signatures = drive(parse()).1.iter().map(signature).collect::<Vec<_>>();
            assert_eq!(
                signatures, reference,
                "run {run} diverged from reference ordering"
            );
        }
    }
    #[test]
    fn choice_level_usage_falls_back_when_top_level_absent() {
        let (output, _events) = drive(vec![chunk(
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop","usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}]}"#,
        )]);
        assert_eq!(output.usage.input, 5, "cached tokens subtracted from input");
        assert_eq!(output.usage.cache_read, 2);
        assert_eq!(output.usage.output, 3);
        assert_eq!(output.usage.reasoning, 1);
        assert_eq!(output.usage.total_tokens, 10);
    }

    #[test]
    fn usage_accounts_cache_write_tokens_and_cost() {
        let (mut output, _events) = drive(vec![chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":95,"prompt_tokens_details":{"cached_tokens":20,"cache_write_tokens":10}}}"#,
        )]);
        let model = Model {
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 3.0,
                tiers: vec![],
            },
            ..Model::default()
        };
        calculate_cost(&model, &mut output.usage);
        assert_eq!(output.usage.input, 70);
        assert_eq!(output.usage.cache_read, 20);
        assert_eq!(output.usage.cache_write, 10);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.usage.total_tokens, 105);
        assert_eq!(output.usage.cost.input, 70.0 / 1e6);
        assert_eq!(output.usage.cost.cache_read, 10.0 / 1e6);
        assert_eq!(output.usage.cost.cache_write, 30.0 / 1e6);
        assert_eq!(output.usage.cost.output, 10.0 / 1e6);
        assert!((output.usage.cost.total - 120.0 / 1e6).abs() < f64::EPSILON);
    }

    #[test]
    fn top_level_usage_takes_precedence_over_choice_level() {
        let (output, _events) = drive(vec![chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#,
        )]);
        assert_eq!(
            output.usage.total_tokens, 10,
            "top-level usage must win over choice-level"
        );
        assert_eq!(output.usage.input, 7);
        assert_eq!(output.usage.output, 3);
    }
}
