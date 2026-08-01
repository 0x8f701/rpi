use super::common;
use crate::*;
use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

// OpenAI Responses rejects max_output_tokens below this floor (#6265).
const MIN_OUTPUT_TOKENS: i64 = 16;

const RESPONSES_TOOL_CALL_PROVIDERS: [&str; 4] = [
    "openai",
    "openai-codex",
    "azure-openai-responses",
    "opencode",
];

/// Provider-native options for the OpenAI Responses API.
#[derive(Clone, Default)]
pub struct OpenAIResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    /// OpenAI service_tier request param ("auto"|"default"|"flex"|"priority").
    pub service_tier: Option<String>,
    /// Responses API tool_choice param, sent verbatim when set.
    pub tool_choice: Option<Value>,
}

impl From<StreamOptions> for OpenAIResponsesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Default::default()
        }
    }
}

/// `register_openai_responses` registers the openai-responses api provider.
pub fn register_openai_responses() {
    register_api_provider(
        ApiProvider {
            api: API_OPENAI_RESPONSES.into(),
            stream: Arc::new(|m, c, o| {
                async move { stream_openai_responses(m, c, OpenAIResponsesOptions::from(o)) }
                    .boxed()
            }),
            stream_simple: Arc::new(|m, c, o| {
                async move { stream_simple_openai_responses(m, c, o) }.boxed()
            }),
        },
        None,
    );
}

/// `stream_simple_openai_responses` maps unified reasoning/max-tokens to Responses options.
pub fn stream_simple_openai_responses(
    model: Model,
    ctx: Context,
    opts: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let opts = build_simple_responses_options(&model, &ctx, opts);
    stream_openai_responses(model, ctx, opts)
}

pub(crate) fn build_simple_responses_options(
    model: &Model,
    ctx: &Context,
    opts: SimpleStreamOptions,
) -> OpenAIResponsesOptions {
    let requested_max_tokens = opts.stream.max_tokens.unwrap_or(model.max_tokens);
    let mut stream = opts.stream;
    stream.max_tokens = Some(clamp_max_tokens_to_context(
        model,
        ctx,
        requested_max_tokens,
    ));
    let reasoning_effort = opts.reasoning.and_then(|level| {
        let clamped = clamp_thinking_level(model, reasoning_effort_for(level));
        (clamped != "off").then(|| clamped.to_string())
    });
    OpenAIResponsesOptions {
        stream,
        reasoning_effort,
        ..Default::default()
    }
}

fn has_effective_responses_auth_header(
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

fn responses_api_key<'a>(model: &Model, options: &'a StreamOptions) -> Result<&'a str> {
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
    if has_effective_responses_auth_header(model, options, recognized) {
        Ok("")
    } else {
        Err(anyhow!("No API key for provider: {}", model.provider))
    }
}

/// `stream_openai_responses` streams from an OpenAI Responses API (`{base_url}/responses`).
pub fn stream_openai_responses(
    model: Model,
    ctx: Context,
    opts: OpenAIResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let s = stream.clone();
    tokio::spawn(async move {
        let mut out = AssistantMessage::pending(&model);

        let key = match responses_api_key(&model, &opts.stream) {
            Ok(key) => key,
            Err(error) => {
                common::fail(&s, out, error.to_string(), false).await;
                return;
            }
        };
        let base_url = match responses_base_url(&model, &opts.stream) {
            Ok(url) => url,
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };
        let url = format!("{base_url}/responses");

        let grammar_input_properties = match grammar_tool_input_properties(
            &ctx.tools,
            get_responses_compat(&model).supports_openai_grammar_tools,
        ) {
            Ok(properties) => properties,
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };
        let params = match build_responses_params(&model, &ctx, &opts) {
            Ok(p) => p,
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };
        let body = match common::apply_provider_request(params, &model, &opts.stream).await {
            Ok(b) => b,
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };
        let http = match common::client(&opts.stream) {
            Ok(c) => c,
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };

        let request_headers = match responses_request_headers(&model, &ctx, &opts.stream, key) {
            Ok(headers) => match common::apply_provider_headers(headers, &model, &opts.stream).await {
                Ok(headers) => headers,
                Err(e) => {
                    common::fail(&s, out, e.to_string(), false).await;
                    return;
                }
            },
            Err(e) => {
                common::fail(&s, out, e.to_string(), false).await;
                return;
            }
        };
        let model_ref = &model;
        let opts_ref = &opts.stream;
        let resp = match common::send_with_retry(&opts.stream, || {
            http.post(&url).json(&body).headers(request_headers.clone())
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                common::fail(&s, out, e.to_string(), common::is_aborted(&opts.stream)).await;
                return;
            }
        };
        if let Err(e) = common::notify_response(&opts.stream, &resp, &model).await {
            common::fail(&s, out, e.to_string(), false).await;
            return;
        }
        if !resp.status().is_success() {
            let (msg, aborted) =
                match common::error_body("OpenAI Responses", resp, &opts.stream).await {
                    Ok(m) => (m, common::is_aborted(&opts.stream)),
                    Err(e) => (e.to_string(), true),
                };
            common::fail(&s, out, msg, aborted).await;
            return;
        }

        s.push(AssistantMessageEvent::Start {
            partial: out.clone(),
        })
        .await;

        // Bridge: the SSE handler is sync (consume_sse requires a FnMut returning
        // Result<()>), but EventStream::push is async. An unbounded channel forwards
        // events to a drainer task that pushes them with proper awaits, preserving
        // real-time streaming while honoring consume_sse's contract.
        let (tx, mut rx) = unbounded_channel::<AssistantMessageEvent>();
        let s_drain = s.clone();
        let drainer = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                s_drain.push(ev).await;
            }
        });

        let mut state = StreamState::with_grammar_input_properties(grammar_input_properties);
        let requested_service_tier = opts.service_tier.clone();
        let stream_err = common::consume_sse(resp, &opts.stream, |name, data| {
            handle_event(
                name,
                data,
                &mut out,
                &mut state,
                &model,
                requested_service_tier.as_deref(),
                &tx,
            )
        })
        .await;
        drop(tx);
        let _ = drainer.await;

        if let Err(e) = stream_err {
            common::fail(&s, out, e.to_string(), common::is_aborted(&opts.stream)).await;
            return;
        }
        if common::is_aborted(&opts.stream) {
            common::fail(&s, out, "Request was aborted".to_string(), true).await;
            return;
        }
        if !state.saw_terminal {
            common::fail(
                &s,
                out,
                "OpenAI Responses stream ended before a terminal response event".to_string(),
                false,
            )
            .await;
            return;
        }
        if out.stop_reason == StopReason::Pending {
            common::fail(
                &s,
                out,
                "OpenAI Responses stream ended without a stop reason".to_string(),
                false,
            )
            .await;
            return;
        }
        if out.stop_reason == StopReason::Error || out.stop_reason == StopReason::Aborted {
            common::fail(&s, out, "An unknown error occurred".to_string(), false).await;
            return;
        }
        state.materialize(&mut out);
        s.push(AssistantMessageEvent::Done {
            reason: out.stop_reason,
            message: out.clone(),
        })
        .await;
        s.end(Some(out)).await;
    });
    stream
}

// ---- request building ----

fn responses_base_url(model: &Model, options: &StreamOptions) -> Result<String> {
    let base_url = if model.base_url.trim().is_empty() {
        "https://api.openai.com/v1"
    } else {
        model.base_url.trim()
    };
    Ok(resolve_base_url(base_url, &options.env)?
        .trim_end_matches('/')
        .to_string())
}

fn responses_request_headers(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    api_key: &str,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    // Telemetry-gated defaults remain deferred until the caller exposes a verified setting.
    if let Some(attribution) = merge_provider_attribution_headers(
        model,
        options.session_id.as_deref(),
        false,
        &HashMap::new(),
    ) {
        common::insert_header_map(&mut headers, &attribution)?;
    }
    common::insert_header(&mut headers, "content-type", "application/json")?;
    common::insert_header(&mut headers, "accept", "text/event-stream")?;
    if model.provider != "cloudflare-ai-gateway" && !api_key.trim().is_empty() {
        common::insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    }
    if let Some(model_headers) = &model.headers {
        common::insert_header_map(&mut headers, model_headers)?;
    }
    if let Some(copilot_headers) = crate::github_copilot::dynamic_headers(&model.provider, context)
    {
        common::insert_header_map(&mut headers, &copilot_headers)?;
    }
    common::insert_header_map(&mut headers, &options.headers)?;
    if model.provider == "cloudflare-ai-gateway" {
        headers.remove("authorization");
        headers.remove("x-api-key");
        if !api_key.trim().is_empty() {
            common::insert_header(
                &mut headers,
                "cf-aig-authorization",
                &format!("Bearer {api_key}"),
            )?;
        }
    }
    Ok(headers)
}

pub(crate) fn build_responses_params(
    model: &Model,
    req: &Context,
    opts: &OpenAIResponsesOptions,
) -> Result<Value> {
    let compat = get_responses_compat(model);
    let placement = split_deferred_tools(&req.tools, &req.messages, compat.supports_tool_search);
    let transformed = transform_messages(&req.messages, model, normalize_responses_tool_call_id);
    let input = convert_input(
        model,
        req,
        &transformed,
        &placement.deferred_by_name,
        &compat,
    )?;
    let mut params = serde_json::Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("input".into(), input);
    params.insert("stream".into(), json!(true));
    params.insert("store".into(), json!(false));

    let retention = resolve_cache_retention(opts.stream.cache_retention, &opts.stream.env);
    // Prompt caching: route same-session requests to a stable cache key so OpenAI
    // can reuse the cached system-prompt + tool prefix (latency/cost win).
    if retention != CacheRetention::None {
        if let Some(session_id) = opts.stream.session_id.as_deref().filter(|s| !s.is_empty()) {
            params.insert(
                "prompt_cache_key".into(),
                json!(clamp_prompt_cache_key(session_id)),
            );
        }
    }
    // pi sets prompt_cache_retention independent of sessionId.
    if retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    // Models with explicit prompt caching must be told to stop caching
    // implicitly when the caller asked for no retention at all.
    if retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode {
        params.insert("prompt_cache_options".into(), json!({ "mode": "explicit" }));
    }

    if let Some(mt) = opts.stream.max_tokens {
        if mt != 0 {
            params.insert("max_output_tokens".into(), json!(mt.max(MIN_OUTPUT_TOKENS)));
        }
    }
    if let Some(t) = opts.stream.temperature {
        params.insert("temperature".into(), json!(t));
    }
    if let Some(st) = &opts.service_tier {
        if !st.is_empty() {
            params.insert("service_tier".into(), json!(st));
        }
    }
    // Only immediate tools go in body.tools; deferred definitions are emitted as
    // client tool_search items at their tool-result markers inside convert_input.
    if !placement.immediate.is_empty() {
        params.insert(
            "tools".into(),
            json!(convert_tools(&placement.immediate, &compat, false)?),
        );
    }
    if let Some(tc) = &opts.tool_choice {
        params.insert("tool_choice".into(), tc.clone());
    }
    if model.reasoning {
        let effort = opts.reasoning_effort.as_deref().filter(|e| !e.is_empty());
        let summary = opts.reasoning_summary.as_deref().filter(|s| !s.is_empty());
        if effort.is_some() || summary.is_some() {
            let e = effort
                .map(|value| mapped_reasoning_effort(model, value))
                .unwrap_or("medium");
            let sm = summary.unwrap_or("auto");
            params.insert("reasoning".into(), json!({ "effort": e, "summary": sm }));
            // store:false needs encrypted reasoning so the item can be replayed inline.
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if let Some(effort) = reasoning_off_effort(model) {
            params.insert("reasoning".into(), json!({ "effort": effort }));
        }
        // xAI returns encrypted reasoning only when asked; request it for every
        // reasoning-capable xai model regardless of which branch fired.
        if model.provider == "xai" {
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    Ok(Value::Object(params))
}

#[derive(Clone, Copy)]
pub(crate) struct ResponsesCompat {
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_long_cache_retention: bool,
    pub(crate) supports_strict_mode: bool,
    pub(crate) supports_openai_grammar_tools: bool,
    pub(crate) supports_tool_search: bool,
    pub(crate) supports_explicit_prompt_cache_mode: bool,
}

pub(crate) fn get_responses_compat(model: &Model) -> ResponsesCompat {
    let mut c = ResponsesCompat {
        supports_developer_role: true,
        supports_long_cache_retention: true,
        supports_strict_mode: false,
        supports_openai_grammar_tools: false,
        supports_tool_search: false,
        supports_explicit_prompt_cache_mode: false,
    };
    let Some(compat) = model.compat.as_ref() else {
        return c;
    };
    if let Some(v) = compat.get("supportsDeveloperRole").and_then(Value::as_bool) {
        c.supports_developer_role = v;
    }
    if let Some(v) = compat
        .get("supportsLongCacheRetention")
        .and_then(Value::as_bool)
    {
        c.supports_long_cache_retention = v;
    }
    if let Some(v) = compat.get("supportsStrictMode").and_then(Value::as_bool) {
        c.supports_strict_mode = v;
    }
    if let Some(v) = compat
        .get("supportsOpenAIGrammarTools")
        .and_then(Value::as_bool)
    {
        c.supports_openai_grammar_tools = v;
    }
    if let Some(v) = compat.get("supportsToolSearch").and_then(Value::as_bool) {
        c.supports_tool_search = v;
    }
    if let Some(v) = compat
        .get("supportsExplicitPromptCacheMode")
        .and_then(Value::as_bool)
    {
        c.supports_explicit_prompt_cache_mode = v;
    }
    c
}

struct DeferredToolSplit<'a> {
    immediate: Vec<&'a ToolDefinition>,
    deferred_by_name: HashMap<String, &'a ToolDefinition>,
}

/// Split tools into immediate prefix definitions and transcript-loaded deferred
/// definitions. A tool is deferred only when a tool result marks it via
/// `added_tool_names` and it was not already used by an assistant before that
/// marker. When `enabled` is false every unique tool is immediate.
fn split_deferred_tools<'a>(
    tools: &'a [ToolDefinition],
    messages: &[Message],
    enabled: bool,
) -> DeferredToolSplit<'a> {
    // Dedup by name; a later definition replaces the value but keeps the first
    // occurrence's position (JS Map.set semantics).
    let mut order: Vec<String> = Vec::with_capacity(tools.len());
    let mut unique: HashMap<String, &'a ToolDefinition> = HashMap::with_capacity(tools.len());
    for tool in tools {
        if !unique.contains_key(&tool.name) {
            order.push(tool.name.clone());
        }
        unique.insert(tool.name.clone(), tool);
    }

    if !enabled {
        return DeferredToolSplit {
            immediate: order
                .into_iter()
                .filter_map(|name| unique.remove(&name))
                .collect(),
            deferred_by_name: HashMap::new(),
        };
    }

    let mut deferred_names = HashSet::new();
    let mut used_names = HashSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        used_names.insert(call.name.clone());
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in &result.added_tool_names {
                    if !used_names.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            Message::User(_)
            | Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred_by_name = HashMap::new();
    for name in order {
        let Some(tool) = unique.remove(&name) else {
            continue;
        };
        if deferred_names.contains(&name) {
            deferred_by_name.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    DeferredToolSplit {
        immediate,
        deferred_by_name,
    }
}

fn resolve_json_schema_strict_sampling(
    tool: &ToolDefinition,
    supports_strict: bool,
) -> Result<bool> {
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

fn infer_grammar_input_property(tool: &ToolDefinition) -> Result<&str> {
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

fn resolve_grammar_sampling(
    tool: &ToolDefinition,
    supported: bool,
) -> Result<Option<GrammarSampling<'_>>> {
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

pub(crate) fn grammar_tool_input_properties(
    tools: &[ToolDefinition],
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

fn convert_tools(
    tools: &[&ToolDefinition],
    compat: &ResponsesCompat,
    defer_loading: bool,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        if let Some(grammar) = resolve_grammar_sampling(t, compat.supports_openai_grammar_tools)? {
            let mut tool = serde_json::Map::from_iter([
                ("type".into(), json!("custom")),
                ("name".into(), json!(t.name)),
                ("description".into(), json!(t.description)),
                (
                    "format".into(),
                    json!({
                        "type": "grammar",
                        "syntax": grammar.syntax,
                        "definition": grammar.definition,
                    }),
                ),
            ]);
            if defer_loading {
                tool.insert("defer_loading".into(), json!(true));
            }
            out.push(Value::Object(tool));
            continue;
        }
        let strict = resolve_json_schema_strict_sampling(t, compat.supports_strict_mode)?;
        let params: Value = serde_json::to_value(&t.parameters)
            .unwrap_or_else(|_| json!({ "type": "object", "properties": {} }));
        let mut tool = serde_json::Map::new();
        tool.insert("type".into(), json!("function"));
        tool.insert("name".into(), json!(t.name));
        tool.insert("description".into(), json!(t.description));
        tool.insert("parameters".into(), params);
        if defer_loading {
            tool.insert("defer_loading".into(), json!(true));
        }
        if compat.supports_strict_mode {
            tool.insert("strict".into(), json!(strict));
        }
        out.push(Value::Object(tool));
    }
    Ok(out)
}

fn resolve_cache_retention(r: CacheRetention, env: &HashMap<String, String>) -> CacheRetention {
    match r {
        CacheRetention::None => CacheRetention::None,
        CacheRetention::Long => CacheRetention::Long,
        CacheRetention::Short => {
            let from_env = env
                .get("PI_CACHE_RETENTION")
                .cloned()
                .or_else(|| std::env::var("PI_CACHE_RETENTION").ok());
            if from_env.as_deref() == Some("long") {
                CacheRetention::Long
            } else {
                CacheRetention::Short
            }
        }
    }
}

/// Keep the cache key within OpenAI's accepted length, clamping by Unicode
/// code points (port of clampOpenAIPromptCacheKey).
pub(crate) fn clamp_prompt_cache_key(key: &str) -> String {
    let count = key.chars().count();
    if count <= 64 {
        key.to_string()
    } else {
        key.chars().take(64).collect()
    }
}

/// flex halves cost, priority doubles it (×2.5 for the exact model id "gpt-5.5").
fn service_tier_cost_multiplier(model: &Model, service_tier: &str) -> f64 {
    match service_tier {
        "flex" => 0.5,
        "priority" if model.id == "gpt-5.5" => 2.5,
        "priority" => 2.0,
        _ => 1.0,
    }
}

fn apply_responses_service_tier_pricing(usage: &mut Usage, service_tier: &str, model: &Model) {
    let multiplier = service_tier_cost_multiplier(model, service_tier);
    if (multiplier - 1.0).abs() < f64::EPSILON {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

fn normalize_id_part(value: &str) -> String {
    value
        .encode_utf16()
        .take(64)
        .map(|unit| {
            if unit <= 0x7f {
                let byte = unit as u8;
                if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
                    return byte as char;
                }
            }
            '_'
        })
        .collect::<String>()
        .trim_end_matches('_')
        .to_string()
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beefu32;
    let mut h2 = 0x41c6_ce57u32;
    for unit in value.encode_utf16() {
        h1 = (h1 ^ u32::from(unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(unit)).wrapping_mul(1_597_334_677);
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
            b'0' + digit
        } else {
            b'a' + digit - 10
        });
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base36 digits are ASCII")
}

fn normalize_responses_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    if !RESPONSES_TOOL_CALL_PROVIDERS.contains(&model.provider.as_str()) || !id.contains('|') {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call_id = normalize_id_part(parts.next().unwrap_or(""));
    let item_id = parts.next().unwrap_or("");
    let foreign = source.provider != model.provider || source.api != model.api;
    let mut item_id = if foreign {
        format!("fc_{}", short_hash(item_id))
    } else {
        normalize_id_part(item_id)
    };
    if !item_id.starts_with("fc_") {
        item_id = normalize_id_part(&format!("fc_{item_id}"));
    }
    format!("{call_id}|{item_id}")
}

fn convert_input(
    model: &Model,
    req: &Context,
    messages: &[Message],
    deferred_by_name: &HashMap<String, &ToolDefinition>,
    compat: &ResponsesCompat,
) -> Result<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut loaded_tool_names: HashSet<String> = HashSet::new();
    let grammar_input_properties =
        grammar_tool_input_properties(&req.tools, compat.supports_openai_grammar_tools)?;
    if !req.system_prompt.is_empty() {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        items.push(json!({ "role": role, "content": req.system_prompt }));
    }
    let image_input = model.input.iter().any(|m| m == "image");
    // Index every transformed transcript message so unsigned assistant text
    // fallbacks stay unique across turns (upstream openai_responses.go).
    let mut msg_index = 0usize;
    for m in messages {
        match m {
            Message::User(u) => {
                let mut content: Vec<Value> = Vec::new();
                for c in &u.content {
                    match c {
                        ContentBlock::Text { text, .. } => {
                            content.push(json!({ "type": "input_text", "text": text }));
                        }
                        ContentBlock::Image { data, mime_type } => {
                            content.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    items.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(a) => {
                let is_different_model =
                    a.model != model.id && a.provider == model.provider && a.api == model.api;
                let mut text_block_index = 0usize;
                for c in &a.content {
                    match c {
                        ContentBlock::Thinking {
                            thinking_signature, ..
                        } => {
                            if let Some(signature) =
                                thinking_signature.as_deref().filter(|s| !s.is_empty())
                            {
                                let reasoning_item = serde_json::from_str::<Value>(signature)
                                    .map_err(|e| {
                                        anyhow!("Invalid Responses reasoning signature: {e}")
                                    })?;
                                items.push(reasoning_item);
                            }
                        }
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            // First text block: msg_pi_{msg_index}; later blocks
                            // append _{text_block_index} (Go openai_responses.go).
                            let fallback_id = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let (parsed_id, phase) = text_signature
                                .as_deref()
                                .and_then(parse_text_signature)
                                .unwrap_or((String::new(), None));
                            // OpenAI caps item ids at 64 UTF-16 units (JS .length).
                            let id = if parsed_id.is_empty() {
                                fallback_id
                            } else if parsed_id.encode_utf16().count() > 64 {
                                format!("msg_{}", short_hash(&parsed_id))
                            } else {
                                parsed_id
                            };
                            let mut item = serde_json::Map::new();
                            item.insert("type".into(), json!("message"));
                            item.insert("role".into(), json!("assistant"));
                            item.insert("status".into(), json!("completed"));
                            item.insert(
                                "content".into(),
                                json!([{ "type": "output_text", "text": text, "annotations": [] }]),
                            );
                            item.insert("id".into(), json!(id));
                            if let Some(phase) = phase {
                                item.insert("phase".into(), json!(phase));
                            }
                            items.push(Value::Object(item));
                        }
                        ContentBlock::ToolCall(tc) => {
                            let (call_id, mut item_id) = split_tool_call_id(&tc.id);
                            let grammar_property = grammar_input_properties.get(&tc.name);
                            if (is_different_model && item_id.starts_with("fc_"))
                                || (grammar_property.is_none() && !item_id.starts_with("fc_"))
                            {
                                item_id.clear();
                            }
                            let mut item = serde_json::Map::new();
                            if let Some(input_property) = grammar_property {
                                item.insert("type".into(), json!("custom_tool_call"));
                                item.insert("call_id".into(), json!(call_id));
                                item.insert("name".into(), json!(tc.name));
                                item.insert(
                                    "input".into(),
                                    json!(grammar_tool_input(tc, input_property)?),
                                );
                            } else {
                                let args_str = serde_json::to_string(&tc.arguments)
                                    .unwrap_or_else(|_| "{}".into());
                                item.insert("type".into(), json!("function_call"));
                                item.insert("call_id".into(), json!(call_id));
                                item.insert("name".into(), json!(tc.name));
                                item.insert("arguments".into(), json!(args_str));
                            }
                            if !item_id.is_empty() {
                                item.insert("id".into(), json!(item_id));
                            }
                            items.push(Value::Object(item));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(tr) => {
                let (call_id, _) = split_tool_call_id(&tr.tool_call_id);
                let mut texts: Vec<String> = Vec::new();
                let mut has_images = false;
                for c in &tr.content {
                    match c {
                        ContentBlock::Text { text, .. } => texts.push(text.clone()),
                        ContentBlock::Image { .. } => has_images = true,
                        _ => {}
                    }
                }
                let text_result = texts.join("\n");
                let has_text = !text_result.is_empty();
                let output: Value = if has_images && image_input {
                    let mut parts: Vec<Value> = Vec::new();
                    if has_text {
                        parts.push(json!({ "type": "input_text", "text": text_result }));
                    }
                    for c in &tr.content {
                        if let ContentBlock::Image { data, mime_type } = c {
                            parts.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }));
                        }
                    }
                    Value::Array(parts)
                } else if has_text {
                    json!(text_result)
                } else if has_images {
                    json!("(see attached image)")
                } else {
                    json!("(no tool output)")
                };
                let output_type = if grammar_input_properties.contains_key(&tr.tool_name) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                items.push(json!({ "type": output_type, "call_id": call_id, "output": output }));

                // Load tools introduced by this result via a completed client tool
                // search anchored at this transcript point (deduped across results).
                let mut deferred: Vec<&ToolDefinition> = Vec::new();
                for name in &tr.added_tool_names {
                    if loaded_tool_names.contains(name) {
                        continue;
                    }
                    let Some(tool) = deferred_by_name.get(name) else {
                        continue;
                    };
                    loaded_tool_names.insert(name.clone());
                    deferred.push(*tool);
                }
                if !deferred.is_empty() {
                    let names: Vec<&str> = deferred.iter().map(|t| t.name.as_str()).collect();
                    let search_call_id = format!(
                        "pi_tool_load_{}",
                        short_hash(&format!("{}:{}", tr.tool_call_id, names.join(",")))
                    );
                    items.push(json!({
                        "type": "tool_search_call",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "arguments": {
                            "query": names.join(" "),
                            "limit": names.len(),
                        },
                    }));
                    let deferred_tools = convert_tools(&deferred, compat, true)?;
                    items.push(json!({
                        "type": "tool_search_output",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "tools": deferred_tools,
                    }));
                }
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("provider transforms project session messages")
            }
        }
        msg_index += 1;
    }
    Ok(Value::Array(items))
}

/// `split_tool_call_id` mirrors JS `id.split("|")`: the item id is the SECOND
/// segment only (later pipes discarded), empty when there is no pipe.
fn split_tool_call_id(id: &str) -> (String, String) {
    let mut parts = id.splitn(3, '|');
    let call_id = parts.next().unwrap_or("").to_string();
    let item_id = parts.next().unwrap_or("").to_string();
    (call_id, item_id)
}

fn parse_text_signature(signature: &str) -> Option<(String, Option<String>)> {
    if signature.is_empty() {
        return None;
    }
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed["v"].as_i64() == Some(1) {
                if let Some(id) = parsed["id"].as_str() {
                    let phase = match parsed["phase"].as_str() {
                        Some("commentary") => Some("commentary".to_string()),
                        Some("final_answer") => Some("final_answer".to_string()),
                        _ => None,
                    };
                    return Some((id.to_string(), phase));
                }
            }
        }
    }
    Some((signature.to_string(), None))
}

fn encode_text_signature(id: &str, phase: Option<&str>) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let mut payload = serde_json::Map::new();
    payload.insert("v".into(), json!(1));
    payload.insert("id".into(), json!(id));
    if matches!(phase, Some("commentary" | "final_answer")) {
        payload.insert("phase".into(), json!(phase));
    }
    Some(Value::Object(payload).to_string())
}

fn mapped_reasoning_effort<'a>(model: &'a Model, effort: &'a str) -> &'a str {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(effort))
        .and_then(Option::as_deref)
        .unwrap_or(effort)
}

fn reasoning_off_effort(model: &Model) -> Option<&str> {
    if model.provider == "github-copilot" {
        return None;
    }
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get("off"))
    {
        Some(None) => None,
        Some(Some(effort)) => Some(effort.as_str()),
        None => Some("off"),
    }
}

fn reasoning_effort_for(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

// ---- SSE handling ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
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

    fn with_input(property: String, input: &str) -> Self {
        Self {
            property,
            input: input.to_string(),
            started: false,
            closed: false,
        }
    }

    fn arguments(&self) -> Value {
        json!({ self.property.clone(): self.input })
    }

    fn append(&mut self, next_input: &str, final_chunk: bool) -> Result<Option<String>> {
        if self.closed {
            if final_chunk && next_input == self.input {
                return Ok(None);
            }
            return Err(anyhow!(
                "grammar tool input for property \"{}\" changed after it was closed",
                self.property
            ));
        }
        let input_delta = next_input.strip_prefix(&self.input).ok_or_else(|| {
            anyhow!(
                "grammar tool input for property \"{}\" changed non-monotonically",
                self.property
            )
        })?;
        if !final_chunk && input_delta.is_empty() {
            return Ok(None);
        }

        // A seed from output_item.added is already reflected in public args, but
        // the first synthesized JSON delta must re-emit it with the first chunk.
        let emitted_input = if self.started {
            input_delta
        } else {
            next_input
        };
        let escaped_input = serde_json::to_string(emitted_input)?;
        let mut delta = String::new();
        if !self.started {
            delta.push('{');
            delta.push_str(&serde_json::to_string(&self.property)?);
            delta.push_str(":\"");
            self.started = true;
        }
        delta.push_str(&escaped_input[1..escaped_input.len() - 1]);
        self.input.clear();
        self.input.push_str(next_input);
        if final_chunk {
            delta.push_str("\"}");
            self.closed = true;
        }
        Ok(Some(delta))
    }
}

struct Slot {
    kind: SlotKind,
    text: String,
    partial_json: String,
    thinking_sig: String,
    text_sig: String,
    tool_id: String,
    tool_name: String,
    args: Value,
    grammar: Option<GrammarInputBuffer>,
}

impl Slot {
    fn new(kind: SlotKind) -> Self {
        Self {
            kind,
            text: String::new(),
            partial_json: String::new(),
            thinking_sig: String::new(),
            text_sig: String::new(),
            tool_id: String::new(),
            tool_name: String::new(),
            args: Value::Object(Default::default()),
            grammar: None,
        }
    }
    fn to_tool_call(&self) -> ToolCall {
        ToolCall {
            id: self.tool_id.clone(),
            name: self.tool_name.clone(),
            arguments: self.args.clone(),
            thought_signature: None,
        }
    }
    fn to_content(&self) -> ContentBlock {
        match self.kind {
            SlotKind::Thinking => ContentBlock::Thinking {
                thinking: self.text.clone(),
                thinking_signature: if self.thinking_sig.is_empty() {
                    None
                } else {
                    Some(self.thinking_sig.clone())
                },
                redacted: false,
            },
            SlotKind::Text => ContentBlock::Text {
                text: self.text.clone(),
                text_signature: if self.text_sig.is_empty() {
                    None
                } else {
                    Some(self.text_sig.clone())
                },
            },
            SlotKind::ToolCall => ContentBlock::ToolCall(self.to_tool_call()),
        }
    }
}

pub(crate) struct StreamState {
    builders: Vec<Slot>,
    /// output_index -> content_index (position in builders).
    slots: HashMap<i64, usize>,
    /// reasoning item id -> content_index, for terminal backfill of encrypted_content.
    reasoning_by_id: HashMap<String, usize>,
    grammar_input_properties: HashMap<String, String>,
    pub(crate) saw_terminal: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            builders: Vec::new(),
            slots: HashMap::new(),
            reasoning_by_id: HashMap::new(),
            grammar_input_properties: HashMap::new(),
            saw_terminal: false,
        }
    }

    pub(crate) fn with_grammar_input_properties(
        grammar_input_properties: HashMap<String, String>,
    ) -> Self {
        Self {
            grammar_input_properties,
            ..Self::new()
        }
    }

    pub(crate) fn materialize(&self, out: &mut AssistantMessage) {
        let mut content = Vec::with_capacity(self.builders.len());
        for b in &self.builders {
            content.push(b.to_content());
        }
        out.content = content;
    }

    fn get_slot(&self, oi: i64, kind: SlotKind) -> Option<usize> {
        let &ci = self.slots.get(&oi)?;
        if self.builders[ci].kind == kind {
            Some(ci)
        } else {
            None
        }
    }

    fn create_slot(
        &mut self,
        oi: i64,
        item: &Value,
        out: &mut AssistantMessage,
        tx: &UnboundedSender<AssistantMessageEvent>,
    ) -> Option<usize> {
        let ty = item["type"].as_str().unwrap_or("");
        let ci = self.builders.len();
        let slot: Slot = match ty {
            "reasoning" => Slot::new(SlotKind::Thinking),
            "message" => {
                if item["phase"].as_str() == Some("final_answer") {
                    out.stop_reason = StopReason::Stop;
                }
                Slot::new(SlotKind::Text)
            }
            "function_call" => {
                let call_id = item["call_id"].as_str().unwrap_or("");
                let id = item["id"].as_str().unwrap_or("");
                let name = item["name"].as_str().unwrap_or("");
                let arguments = item["arguments"].as_str().unwrap_or("");
                let mut sl = Slot::new(SlotKind::ToolCall);
                sl.tool_id = format!("{call_id}|{id}");
                sl.tool_name = name.to_string();
                sl.partial_json = arguments.to_string();
                sl
            }
            "custom_tool_call" => {
                let call_id = item["call_id"].as_str().unwrap_or("");
                let id = item["id"].as_str().unwrap_or("");
                let name = item["name"].as_str().unwrap_or("");
                let property = self
                    .grammar_input_properties
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "input".into());
                let seed = item.get("input").and_then(Value::as_str).unwrap_or("");
                let mut sl = Slot::new(SlotKind::ToolCall);
                sl.tool_id = format!("{call_id}|{id}");
                sl.tool_name = name.to_string();
                sl.args = json!({ property.clone(): seed });
                sl.grammar = Some(GrammarInputBuffer::with_input(property, seed));
                sl
            }
            _ => return None,
        };
        let kind = slot.kind;
        self.builders.push(slot);
        self.slots.insert(oi, ci);
        self.materialize(out);
        let start = match kind {
            SlotKind::Thinking => AssistantMessageEvent::ThinkingStart {
                content_index: ci,
                partial: out.clone(),
            },
            SlotKind::Text => AssistantMessageEvent::TextStart {
                content_index: ci,
                partial: out.clone(),
            },
            SlotKind::ToolCall => AssistantMessageEvent::ToolCallStart {
                content_index: ci,
                partial: out.clone(),
            },
        };
        let _ = tx.send(start);
        Some(ci)
    }

    fn get_or_create(
        &mut self,
        oi: i64,
        item: &Value,
        out: &mut AssistantMessage,
        tx: &UnboundedSender<AssistantMessageEvent>,
    ) -> Option<usize> {
        if let Some(&ci) = self.slots.get(&oi) {
            return Some(ci);
        }
        self.create_slot(oi, item, out, tx)
    }

    fn append_grammar_input(
        &mut self,
        oi: i64,
        next_input: &str,
        final_chunk: bool,
        out: &mut AssistantMessage,
        tx: &UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        let Some(ci) = self.get_slot(oi, SlotKind::ToolCall) else {
            return Ok(());
        };
        let delta = {
            let slot = &mut self.builders[ci];
            let Some(grammar) = slot.grammar.as_mut() else {
                return Ok(());
            };
            let delta = grammar.append(next_input, final_chunk)?;
            slot.args = grammar.arguments();
            delta
        };
        self.materialize(out);
        if let Some(delta) = delta {
            let _ = tx.send(AssistantMessageEvent::ToolCallDelta {
                content_index: ci,
                delta,
                partial: out.clone(),
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_event(
    _name: Option<&str>,
    data: &str,
    out: &mut AssistantMessage,
    state: &mut StreamState,
    model: &Model,
    requested_service_tier: Option<&str>,
    tx: &UnboundedSender<AssistantMessageEvent>,
) -> Result<()> {
    let ev = parse_json_with_repair(data).unwrap_or(Value::Null);
    let ty = ev["type"].as_str().unwrap_or("");
    match ty {
        "response.created" => {
            if let Some(id) = ev["response"]["id"].as_str() {
                out.response_id = Some(id.to_string());
            }
        }
        "response.output_item.added" => {
            if !ev["item"].is_null() {
                let oi = ev["output_index"].as_i64().unwrap_or(0);
                state.create_slot(oi, &ev["item"], out, tx);
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get_slot(oi, SlotKind::Thinking) {
                let delta = ev["delta"].as_str().unwrap_or("").to_string();
                state.builders[ci].text.push_str(&delta);
                state.materialize(out);
                let _ = tx.send(AssistantMessageEvent::ThinkingDelta {
                    content_index: ci,
                    delta,
                    partial: out.clone(),
                });
            }
        }
        "response.reasoning_summary_part.done" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get_slot(oi, SlotKind::Thinking) {
                state.builders[ci].text.push_str("\n\n");
                state.materialize(out);
                let _ = tx.send(AssistantMessageEvent::ThinkingDelta {
                    content_index: ci,
                    delta: "\n\n".to_string(),
                    partial: out.clone(),
                });
            }
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get_slot(oi, SlotKind::Text) {
                let delta = ev["delta"].as_str().unwrap_or("").to_string();
                state.builders[ci].text.push_str(&delta);
                state.materialize(out);
                let _ = tx.send(AssistantMessageEvent::TextDelta {
                    content_index: ci,
                    delta,
                    partial: out.clone(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get_slot(oi, SlotKind::ToolCall) {
                if state.builders[ci].grammar.is_none() {
                    let delta = ev["delta"].as_str().unwrap_or("").to_string();
                    let slot = &mut state.builders[ci];
                    slot.partial_json.push_str(&delta);
                    slot.args = parse_streaming_json(&slot.partial_json);
                    state.materialize(out);
                    let _ = tx.send(AssistantMessageEvent::ToolCallDelta {
                        content_index: ci,
                        delta,
                        partial: out.clone(),
                    });
                }
            }
        }
        "response.function_call_arguments.done" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get_slot(oi, SlotKind::ToolCall) {
                if state.builders[ci].grammar.is_none() {
                    let args = ev["arguments"].as_str().unwrap_or("").to_string();
                    let trailing;
                    {
                        let slot = &mut state.builders[ci];
                        let prev = slot.partial_json.clone();
                        slot.partial_json = args.clone();
                        slot.args = parse_streaming_json(&args);
                        trailing = args
                            .strip_prefix(&prev)
                            .map(str::to_string)
                            .unwrap_or_default();
                    }
                    state.materialize(out);
                    if !trailing.is_empty() {
                        let _ = tx.send(AssistantMessageEvent::ToolCallDelta {
                            content_index: ci,
                            delta: trailing,
                            partial: out.clone(),
                        });
                    }
                }
            }
        }
        "response.custom_tool_call_input.delta" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            let Some(ci) = state.get_slot(oi, SlotKind::ToolCall) else {
                return Ok(());
            };
            let Some(grammar) = state.builders[ci].grammar.as_ref() else {
                return Ok(());
            };
            let mut next_input = grammar.input.clone();
            next_input.push_str(ev["delta"].as_str().unwrap_or(""));
            state.append_grammar_input(oi, &next_input, false, out, tx)?;
        }
        "response.custom_tool_call_input.done" => {
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            let input = ev["input"].as_str().unwrap_or("");
            state.append_grammar_input(oi, input, true, out, tx)?;
        }
        "response.output_item.done" => {
            if ev["item"].is_null() {
                return Ok(());
            }
            let oi = ev["output_index"].as_i64().unwrap_or(0);
            let item = &ev["item"];
            let ity = item["type"].as_str().unwrap_or("");
            if ity == "message" && item["phase"].as_str() == Some("final_answer") {
                out.stop_reason = StopReason::Stop;
            }
            let ci = state.get_or_create(oi, item, out, tx);
            let Some(ci) = ci else {
                return Ok(());
            };
            match ity {
                "reasoning" => {
                    if state.builders[ci].kind != SlotKind::Thinking {
                        return Ok(());
                    }
                    let summary = join_parts_text(&item["summary"], "\n\n");
                    let content = join_parts_text(&item["content"], "\n\n");
                    let id = item["id"].as_str().unwrap_or("").to_string();
                    let raw = item.to_string();
                    let final_text;
                    {
                        let slot = &mut state.builders[ci];
                        let mut rebuilt = if !summary.is_empty() {
                            summary.clone()
                        } else {
                            content.clone()
                        };
                        if rebuilt.is_empty() {
                            rebuilt = slot.text.clone();
                        }
                        slot.text = rebuilt.clone();
                        slot.thinking_sig = raw;
                        final_text = rebuilt;
                    }
                    if !id.is_empty() {
                        state.reasoning_by_id.insert(id, ci);
                    }
                    state.materialize(out);
                    let _ = tx.send(AssistantMessageEvent::ThinkingEnd {
                        content_index: ci,
                        content: final_text,
                        partial: out.clone(),
                    });
                    state.slots.remove(&oi);
                }
                "message" => {
                    if state.builders[ci].kind != SlotKind::Text {
                        return Ok(());
                    }
                    let mut sb = String::new();
                    if let Some(arr) = item["content"].as_array() {
                        for p in arr {
                            if p["type"].as_str() == Some("refusal") {
                                if let Some(r) = p["refusal"].as_str() {
                                    sb.push_str(r);
                                }
                            } else if let Some(t) = p["text"].as_str() {
                                sb.push_str(t);
                            }
                        }
                    }
                    let id = item["id"].as_str().unwrap_or("");
                    state.builders[ci].text = sb.clone();
                    state.builders[ci].text_sig =
                        encode_text_signature(id, item["phase"].as_str()).unwrap_or_default();
                    state.materialize(out);
                    let _ = tx.send(AssistantMessageEvent::TextEnd {
                        content_index: ci,
                        content: sb,
                        partial: out.clone(),
                    });
                    state.slots.remove(&oi);
                }
                "function_call" => {
                    if state.builders[ci].kind != SlotKind::ToolCall
                        || state.builders[ci].grammar.is_some()
                    {
                        return Ok(());
                    }
                    let args_json = item["arguments"].as_str().unwrap_or("").to_string();
                    {
                        let slot = &mut state.builders[ci];
                        let a = if args_json.is_empty() {
                            slot.partial_json.clone()
                        } else {
                            args_json
                        };
                        let a = if a.is_empty() { "{}".to_string() } else { a };
                        slot.args = parse_streaming_json(&a);
                    }
                    state.materialize(out);
                    let tc = state.builders[ci].to_tool_call();
                    let _ = tx.send(AssistantMessageEvent::ToolCallEnd {
                        content_index: ci,
                        tool_call: tc,
                        partial: out.clone(),
                    });
                    state.slots.remove(&oi);
                }
                "custom_tool_call" => {
                    if state.builders[ci].kind != SlotKind::ToolCall
                        || state.builders[ci].grammar.is_none()
                    {
                        return Ok(());
                    }
                    let final_input = item
                        .get("input")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            state.builders[ci]
                                .grammar
                                .as_ref()
                                .map(|grammar| grammar.input.clone())
                                .unwrap_or_default()
                        });
                    state.append_grammar_input(oi, &final_input, true, out, tx)?;
                    state.builders[ci].grammar = None;
                    state.materialize(out);
                    let tc = state.builders[ci].to_tool_call();
                    let _ = tx.send(AssistantMessageEvent::ToolCallEnd {
                        content_index: ci,
                        tool_call: tc,
                        partial: out.clone(),
                    });
                    state.slots.remove(&oi);
                }
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" => {
            state.saw_terminal = true;
            let r = &ev["response"];
            if !r.is_null() {
                // Backfill reasoning encrypted_content that arrived only in the
                // terminal response (Azure omits it from output_item.done).
                let mut backfills: Vec<(usize, String)> = Vec::new();
                if let Some(output) = r["output"].as_array() {
                    for it in output {
                        if it["type"].as_str() == Some("reasoning") {
                            if let Some(ec) = it["encrypted_content"].as_str() {
                                if !ec.is_empty() {
                                    if let Some(id) = it["id"].as_str() {
                                        if let Some(&ci) = state.reasoning_by_id.get(id) {
                                            if !state.builders[ci].thinking_sig.is_empty() {
                                                backfills.push((ci, ec.to_string()));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for (ci, ec) in backfills {
                    let sig = state.builders[ci].thinking_sig.clone();
                    if let Ok(mut stored) = serde_json::from_str::<Value>(&sig) {
                        let needs = stored
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .map(|s| s.is_empty())
                            .unwrap_or(true);
                        if needs {
                            if let Value::Object(ref mut map) = stored {
                                map.insert("encrypted_content".into(), json!(ec));
                            }
                            if let Ok(rebuilt) = serde_json::to_string(&stored) {
                                state.builders[ci].thinking_sig = rebuilt;
                            }
                        }
                    }
                }
                if let Some(id) = r["id"].as_str() {
                    out.response_id = Some(id.to_string());
                }
                if let Some(usage) = r.get("usage") {
                    let input_tokens = usage["input_tokens"].as_i64().unwrap_or(0);
                    let output_tokens = usage["output_tokens"].as_i64().unwrap_or(0);
                    let total = usage["total_tokens"].as_i64().unwrap_or(0);
                    let cached = usage["input_tokens_details"]["cached_tokens"]
                        .as_i64()
                        .unwrap_or(0);
                    let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
                        .as_i64()
                        .unwrap_or(0);
                    let reasoning = usage["output_tokens_details"]["reasoning_tokens"]
                        .as_i64()
                        .unwrap_or(0);
                    let input = (input_tokens - cached - cache_write).max(0);
                    out.usage = Usage {
                        input,
                        output: output_tokens,
                        cache_read: cached,
                        cache_write,
                        cache_write_1h: 0,
                        reasoning,
                        total_tokens: total,
                        cost: CostBreakdown::default(),
                    };
                }
                out.raw_stop_reason = r["status"].as_str().map(String::from);
            } else {
                out.raw_stop_reason = None;
            }
            calculate_cost(model, &mut out.usage);
            // Service-tier pricing: the response-reported tier wins over the
            // requested one (`response?.service_tier ?? options.serviceTier`).
            let service_tier = if !r.is_null() {
                r.get("service_tier")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
            .or(requested_service_tier)
            .unwrap_or("");
            apply_responses_service_tier_pricing(&mut out.usage, service_tier, model);
            let status = out.raw_stop_reason.clone().unwrap_or_default();
            match map_status(&status) {
                Ok(reason) => out.stop_reason = reason,
                Err(e) => return Err(e),
            }
            if out.stop_reason == StopReason::Stop
                && state.builders.iter().any(|b| b.kind == SlotKind::ToolCall)
            {
                out.stop_reason = StopReason::ToolUse;
            }
        }
        "error" => {
            let code = ev["code"].as_str().unwrap_or("unknown");
            let msg = ev["message"].as_str().unwrap_or("no message");
            return Err(anyhow!("Error Code {code}: {msg}"));
        }
        "response.failed" => {
            state.saw_terminal = true;
            out.raw_stop_reason = ev["response"]["status"].as_str().map(String::from);
            return Err(anyhow!("{}", failed_message(&ev)));
        }
        _ => {}
    }
    Ok(())
}

fn map_status(status: &str) -> Result<StopReason> {
    Ok(match status {
        "" | "completed" | "in_progress" | "queued" => StopReason::Stop,
        "incomplete" => StopReason::Length,
        "failed" | "cancelled" => StopReason::Error,
        other => return Err(anyhow!("Unhandled stop reason: {other}")),
    })
}

fn join_parts_text(parts: &Value, sep: &str) -> String {
    let arr = match parts.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    arr.iter()
        .filter_map(|p| {
            if p["type"].as_str() == Some("refusal") {
                p["refusal"].as_str().map(String::from)
            } else {
                p["text"].as_str().map(String::from)
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}

fn failed_message(ev: &Value) -> String {
    let r = &ev["response"];
    if !r.is_null() {
        let err = &r["error"];
        if !err.is_null() {
            let code = err["code"].as_str().unwrap_or("unknown");
            let msg = err["message"].as_str().unwrap_or("no message");
            return format!("{code}: {msg}");
        }
        if let Some(reason) = r["incomplete_details"]["reason"].as_str() {
            if !reason.is_empty() {
                return format!("incomplete: {reason}");
            }
        }
    }
    "Unknown error (no error details in response)".to_string()
}

/// `parse_streaming_json` parses a partial JSON object, closing open structures so
/// truncated streaming tool-call arguments still decode. Always returns an object.
fn parse_streaming_json(partial: &str) -> Value {
    let trimmed = partial.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    if let Some(v) = parse_json_with_repair(trimmed) {
        if v.is_object() {
            return v;
        }
    }
    if let Some(completed) = complete_partial_json(trimmed) {
        if let Some(v) = parse_json_with_repair(&completed) {
            if v.is_object() {
                return v;
            }
        }
    }
    Value::Object(Default::default())
}

fn complete_partial_json(s: &str) -> Option<String> {
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for &ch in s.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == b'\\' {
                escaped = true;
                continue;
            }
            if ch == b'"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut completed = s.trim_end().to_string();
    if in_string {
        completed.push('"');
    }
    while let Some(c) = stack.pop() {
        completed.push(c as char);
    }
    Some(completed)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const SSE: &str = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}

data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}

data: {\"type\":\"response.reasoning_summary_part.added\",\"part\":{\"type\":\"summary_text\",\"text\":\"\"}}

data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"pondering\"}

data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"pondering\"}]}}

data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}

data: {\"type\":\"response.content_part.added\",\"part\":{\"type\":\"output_text\",\"text\":\"\"}}

data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer: \"}

data: {\"type\":\"response.output_text.delta\",\"delta\":\"42\"}

data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Answer: 42\"}]}}

data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"\"}}

data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"x\\\":1}\"}

data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"{\\\"x\\\":1}\"}}

data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":20,\"output_tokens\":8,\"total_tokens\":28,\"input_tokens_details\":{\"cached_tokens\":5}}}}

";

    #[derive(Clone)]
    struct Captured {
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    fn find_subsequence(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn extract_content_length(headers: &[u8]) -> usize {
        let s = String::from_utf8_lossy(headers);
        for line in s.lines() {
            if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                return rest.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    fn parse_request(buf: &[u8]) -> Captured {
        let idx = find_subsequence(buf, b"\r\n\r\n").unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..idx]).to_string();
        let body = String::from_utf8_lossy(&buf[idx + 4..]).to_string();
        let mut lines = head.lines();
        let path = lines
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Captured {
            path,
            headers,
            body,
        }
    }

    fn spawn_mock(captured: Arc<Mutex<Option<Captured>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
                if let Some(idx) = find_subsequence(&buf, b"\r\n\r\n") {
                    let cl = extract_content_length(&buf[..idx]);
                    if buf.len() >= idx + 4 + cl {
                        break;
                    }
                }
            }
            let cap = parse_request(&buf);
            *captured.lock().unwrap() = Some(cap);
            let resp =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).unwrap();
            sock.write_all(SSE.as_bytes()).unwrap();
            sock.flush().unwrap();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn openai_responses_parses_stream() {
        let captured = Arc::new(Mutex::new(None::<Captured>));
        let url = spawn_mock(captured.clone());

        let mut model = Model::default();
        model.id = "gpt-5".into();
        model.api = API_OPENAI_RESPONSES.into();
        model.provider = "openai".into();
        model.base_url = url;
        model.reasoning = true;
        model.max_tokens = 4096;
        model.cost.input = 1.25;
        model.cost.output = 10.0;

        let mut props = HashMap::new();
        props.insert(
            "x".to_string(),
            Schema {
                schema_type: Some(Value::String("integer".into())),
                ..Schema::default()
            },
        );
        let req = Context {
            system_prompt: "be terse".into(),
            messages: vec![Message::user_text("what is 6*7?", 1)],
            tools: vec![ToolDefinition {
                name: "calc".into(),
                description: "calc".into(),
                parameters: Schema::object(props, vec!["x".to_string()]),
                constrained_sampling: None,
            }],
        };

        let opts = OpenAIResponsesOptions {
            stream: StreamOptions {
                api_key: Some("sk".into()),
                ..StreamOptions::default()
            },
            reasoning_effort: Some("medium".into()),
            ..OpenAIResponsesOptions::default()
        };
        let stream = stream_openai_responses(model, req, opts);

        let final_msg = stream.result().await.expect("final message");

        // Drain queued events (Start/deltas/ends/Done remain after result()).
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }

        assert_eq!(
            final_msg.stop_reason,
            StopReason::ToolUse,
            "stop_reason: {:?} err: {:?}",
            final_msg.stop_reason,
            final_msg.error_message
        );

        let mut thinking = String::new();
        let mut text = String::new();
        let mut tool: Option<ToolCall> = None;
        for c in &final_msg.content {
            match c {
                ContentBlock::Thinking {
                    thinking: t,
                    thinking_signature,
                    ..
                } => {
                    thinking.clone_from(t);
                    assert!(
                        thinking_signature.is_some(),
                        "reasoning signature not captured"
                    );
                }
                ContentBlock::Text { text: t, .. } => text.clone_from(t),
                ContentBlock::ToolCall(tc) => tool = Some(tc.clone()),
                _ => {}
            }
        }
        assert_eq!(thinking, "pondering");
        assert_eq!(text, "Answer: 42");
        let tool = tool.expect("tool call present");
        assert_eq!(tool.name, "calc");
        assert_eq!(tool.id, "call_1|fc_1");
        assert_eq!(tool.arguments["x"].as_f64(), Some(1.0));
        assert_eq!(final_msg.usage.input, 15);
        assert_eq!(final_msg.usage.cache_read, 5);
        assert_eq!(final_msg.usage.output, 8);
        assert_eq!(final_msg.usage.total_tokens, 28);
        assert_eq!(final_msg.response_id, Some("resp_1".to_string()));

        // Events prove text/tool/final streaming.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::Start { .. })),
            "Start event"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == "42")
            ),
            "TextDelta 42"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::ToolCallEnd { .. })),
            "ToolCallEnd"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::Done { .. })),
            "Done"
        );

        let cap = captured.lock().unwrap().clone().expect("request captured");
        assert!(cap.path.ends_with("/responses"), "path: {}", cap.path);
        assert_eq!(
            cap.headers.get("authorization"),
            Some(&"Bearer sk".to_string())
        );
        assert_eq!(
            cap.headers.get("accept"),
            Some(&"text/event-stream".to_string())
        );
        let body: Value = serde_json::from_str(&cap.body).expect("body is json");
        assert!(body["input"].is_array(), "input array sent: {body}");
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        // developer role for reasoning model + system prompt.
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"], "be terse");
        // user text round-trips as input_text.
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
        // tool registered as a function tool.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "calc");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["x"]["type"],
            "integer"
        );
    }

    #[test]
    fn responses_projects_visible_bash_and_excludes_hidden_bash() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let req = Context {
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
        let transformed =
            transform_messages(&req.messages, &model, normalize_responses_tool_call_id);
        let input = convert_input(
            &model,
            &req,
            &transformed,
            &HashMap::new(),
            &get_responses_compat(&model),
        )
        .expect("input");
        assert_eq!(input.as_array().map(Vec::len), Some(1));
        assert_eq!(input[0]["role"], "user");
        assert_eq!(
            input[0]["content"][0]["text"],
            "Ran `echo ok`\n```\nok\n```"
        );
    }

    #[test]
    fn responses_headers_are_case_insensitive_single_valued_and_caller_wins() {
        let model = Model {
            provider: "openai".into(),
            headers: Some(HashMap::from([(
                "authorization".into(),
                "Bearer model".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("AUTHORIZATION".into(), "Bearer request".into())]),
            ..StreamOptions::default()
        };
        let headers =
            responses_request_headers(&model, &Context::default(), &options, "provider-key")
                .expect("headers");
        let request = common::client(&options)
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
    }

    #[test]
    fn responses_header_only_auth_skips_default_credential() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([(
                "aUtHoRiZaTiOn".into(),
                "Bearer model-secret".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("AUTHORIZATION".into(), "Bearer caller-secret".into())]),
            ..StreamOptions::default()
        };
        let api_key = responses_api_key(&model, &options).expect("header-owned auth");
        assert!(api_key.is_empty());
        let headers = responses_request_headers(&model, &Context::default(), &options, api_key)
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
    fn responses_no_key_or_auth_header_returns_sanitized_error() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([
                ("Authorization".into(), " ".into()),
                ("X-Secret".into(), "must-not-leak".into()),
            ])),
            ..Model::default()
        };
        let error = responses_api_key(&model, &StreamOptions::default()).expect_err("missing auth");
        assert_eq!(error.to_string(), "No API key for provider: custom");
        assert!(!error.to_string().contains("must-not-leak"));
    }

    #[test]
    fn responses_cloudflare_uses_gateway_authorization_only() {
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
        let headers =
            responses_request_headers(&model, &Context::default(), &options, "gateway-key")
                .expect("headers");
        let request = common::client(&options)
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
    fn responses_cloudflare_header_only_auth_preserves_caller_value() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([("CF-AIG-AUTHORIZATION".into(), "Bearer caller".into())]),
            ..StreamOptions::default()
        };
        let api_key = responses_api_key(&model, &options).expect("header-owned auth");
        assert!(api_key.is_empty());
        let headers = responses_request_headers(&model, &Context::default(), &options, api_key)
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
    fn responses_copilot_headers_and_bearer_auth_match_request_context() {
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
        let headers = responses_request_headers(&model, &context, &options, "copilot-token")
            .expect("headers");
        let request = common::client(&options)
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
    fn output_item_added_start_events_carry_materialized_slot() {
        // Regression: each *Start event must carry a partial snapshot in which the
        // just-created slot is already materialized (partial.content[content_index]
        // exists with the block kind of the added output item).
        let model = Model::default();
        let mut out = AssistantMessage::pending(&model);
        let mut state = StreamState::new();
        let (tx, mut rx) = unbounded_channel();

        let cases: [(i64, &str, SlotKind); 3] = [
            (0, r#"{"type":"reasoning","id":"rs_1"}"#, SlotKind::Thinking),
            (1, r#"{"type":"message","id":"msg_1"}"#, SlotKind::Text),
            (
                2,
                r#"{"type":"function_call","id":"fc_1","call_id":"call_1","name":"calc","arguments":""}"#,
                SlotKind::ToolCall,
            ),
        ];
        for (oi, item, kind) in cases {
            let data = format!(
                r#"{{"type":"response.output_item.added","output_index":{oi},"item":{item}}}"#
            );
            handle_event(
                Some("response.output_item.added"),
                &data,
                &mut out,
                &mut state,
                &model,
                None,
                &tx,
            )
            .expect("output_item.added handled");
            let start = rx
                .try_recv()
                .expect("start event emitted for added output item");
            let ci = oi as usize;
            match kind {
                SlotKind::Thinking => assert!(
                    matches!(&start, AssistantMessageEvent::ThinkingStart { content_index, .. } if *content_index == ci)
                ),
                SlotKind::Text => assert!(
                    matches!(&start, AssistantMessageEvent::TextStart { content_index, .. } if *content_index == ci)
                ),
                SlotKind::ToolCall => assert!(
                    matches!(&start, AssistantMessageEvent::ToolCallStart { content_index, .. } if *content_index == ci)
                ),
            }
            let partial = start.partial().expect("start event carries partial");
            // Snapshot covers every slot created so far, not just the new one.
            assert_eq!(
                partial.content.len(),
                ci + 1,
                "partial must be a current snapshot"
            );
            let expected = match kind {
                SlotKind::Thinking => ContentBlock::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                },
                SlotKind::Text => ContentBlock::Text {
                    text: String::new(),
                    text_signature: None,
                },
                SlotKind::ToolCall => ContentBlock::ToolCall(ToolCall {
                    id: "call_1|fc_1".into(),
                    name: "calc".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }),
            };
            assert_eq!(
                partial.content[ci], expected,
                "partial.content[{ci}] must be the created block kind"
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "no extra events beyond the three start events"
        );
    }

    #[test]
    fn thinking_signature_replays_as_raw_store_false_item() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            reasoning: true,
            ..Model::default()
        };
        let raw_reasoning = json!({
            "type": "reasoning",
            "id": "rs_previous",
            "summary": [{ "type": "summary_text", "text": "prior thought" }],
            "encrypted_content": "encrypted-store-false-replay",
        });
        let mut assistant = AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "prior thought".into(),
                thinking_signature: Some(raw_reasoning.to_string()),
                redacted: false,
            },
            ContentBlock::text("prior answer"),
        ];
        let req = Context {
            messages: vec![
                Message::user_text("first turn", 1),
                Message::Assistant(assistant),
                Message::user_text("follow-up", 3),
            ],
            ..Context::default()
        };

        let payload = build_responses_params(&model, &req, &OpenAIResponsesOptions::default())
            .expect("multi-turn payload");
        assert_eq!(payload["store"], false);
        let input = payload["input"].as_array().expect("input array");
        assert_eq!(input.len(), 4);
        assert_eq!(
            input[1], raw_reasoning,
            "reasoning item must replay verbatim before its answer"
        );
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[3]["content"][0]["text"], "follow-up");
    }

    #[test]
    fn reasoning_models_emit_off_without_selected_effort_except_excluded_provider() {
        let mut model = Model {
            id: "reasoning-model".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            reasoning: true,
            ..Model::default()
        };
        let payload = build_responses_params(
            &model,
            &Context::default(),
            &OpenAIResponsesOptions::default(),
        )
        .expect("reasoning-off payload");
        assert_eq!(payload["reasoning"], json!({ "effort": "off" }));
        assert!(
            payload.get("include").is_none(),
            "off mode does not request encrypted reasoning"
        );

        model.thinking_level_map = Some(HashMap::from([("off".into(), Some("none".into()))]));
        let mapped = build_responses_params(
            &model,
            &Context::default(),
            &OpenAIResponsesOptions::default(),
        )
        .expect("mapped reasoning-off payload");
        assert_eq!(mapped["reasoning"], json!({ "effort": "none" }));

        model.provider = "github-copilot".into();
        let excluded = build_responses_params(
            &model,
            &Context::default(),
            &OpenAIResponsesOptions::default(),
        )
        .expect("excluded-provider payload");
        assert!(
            excluded.get("reasoning").is_none(),
            "GitHub Copilot rejects the default off payload"
        );
    }

    #[test]
    fn streamed_text_signature_preserves_and_replays_message_id() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let mut out = AssistantMessage::pending(&model);
        let mut state = StreamState::new();
        let (tx, _rx) = unbounded_channel();
        let added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_signed","phase":"final_answer"}}"#;
        handle_event(
            Some("response.output_item.added"),
            added,
            &mut out,
            &mut state,
            &model,
            None,
            &tx,
        )
        .expect("message start");
        let done = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_signed","phase":"final_answer","content":[{"type":"output_text","text":"signed answer"}]}}"#;
        handle_event(
            Some("response.output_item.done"),
            done,
            &mut out,
            &mut state,
            &model,
            None,
            &tx,
        )
        .expect("message done");

        let signature = match &out.content[0] {
            ContentBlock::Text {
                text_signature: Some(signature),
                ..
            } => signature,
            other => panic!("expected signed text block, got {other:?}"),
        };
        let (id, phase) = parse_text_signature(signature).expect("valid text signature");
        assert_eq!(id, "msg_signed");
        assert_eq!(phase.as_deref(), Some("final_answer"));
        let req = Context {
            messages: vec![Message::Assistant(out)],
            ..Context::default()
        };

        let transformed =
            transform_messages(&req.messages, &model, normalize_responses_tool_call_id);
        let empty_deferred = HashMap::new();
        let compat = get_responses_compat(&model);
        let input = convert_input(&model, &req, &transformed, &empty_deferred, &compat)
            .expect("replayed input");
        let replayed = &input.as_array().expect("input array")[0];
        assert_eq!(replayed["id"], "msg_signed");
        assert_eq!(replayed["phase"], "final_answer");
        assert_eq!(replayed["content"][0]["text"], "signed answer");
    }

    fn assistant_with_text_blocks(model: &Model, blocks: Vec<ContentBlock>) -> AssistantMessage {
        let mut msg = AssistantMessage::pending(model);
        msg.stop_reason = StopReason::Stop;
        msg.content = blocks;
        msg
    }

    fn replay_assistant_message_ids(model: &Model, messages: Vec<Message>) -> Vec<Value> {
        let req = Context {
            messages: messages.clone(),
            ..Context::default()
        };
        let transformed = transform_messages(&messages, model, normalize_responses_tool_call_id);
        let empty_deferred = HashMap::new();
        let compat = get_responses_compat(model);
        let input = convert_input(model, &req, &transformed, &empty_deferred, &compat)
            .expect("convert_input");
        input
            .as_array()
            .expect("input array")
            .iter()
            .filter(|item| item["type"] == "message" && item["role"] == "assistant")
            .cloned()
            .collect()
    }

    #[test]
    fn unsigned_assistant_turns_use_unique_msg_index_fallbacks() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        // Transformed transcript indices: user=0, asst=1, user=2, asst=3.
        let messages = vec![
            Message::user_text("hi", 1),
            Message::Assistant(assistant_with_text_blocks(
                &model,
                vec![ContentBlock::text("first turn")],
            )),
            Message::user_text("again", 2),
            Message::Assistant(assistant_with_text_blocks(
                &model,
                vec![ContentBlock::text("second turn")],
            )),
        ];
        let ids: Vec<_> = replay_assistant_message_ids(&model, messages)
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["msg_pi_1".to_string(), "msg_pi_3".to_string()]);
        assert_ne!(
            ids[0], ids[1],
            "two unsigned assistant turns must not collide"
        );
    }

    #[test]
    fn multiple_unsigned_text_blocks_suffix_text_block_index() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        // Sole message → msg_index 0; blocks 0/1/2 → msg_pi_0, msg_pi_0_1, msg_pi_0_2.
        let messages = vec![Message::Assistant(assistant_with_text_blocks(
            &model,
            vec![
                ContentBlock::text("a"),
                ContentBlock::text("b"),
                ContentBlock::text("c"),
            ],
        ))];
        let ids: Vec<_> = replay_assistant_message_ids(&model, messages)
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ids,
            vec![
                "msg_pi_0".to_string(),
                "msg_pi_0_1".to_string(),
                "msg_pi_0_2".to_string()
            ]
        );
    }

    #[test]
    fn message_ids_normalize_overlong_utf16_and_preserve_phase() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let exact64 = "a".repeat(64);
        assert_eq!(exact64.encode_utf16().count(), 64);
        // 31 astral chars (62 units) + 3 ASCII = 65 UTF-16 units.
        let overlong_surrogate = format!("{}abc", "\u{1F648}".repeat(31));
        assert_eq!(overlong_surrogate.encode_utf16().count(), 65);
        let overlong_ascii = "x".repeat(65);
        assert_eq!(overlong_ascii.encode_utf16().count(), 65);

        let messages = vec![Message::Assistant(assistant_with_text_blocks(
            &model,
            vec![
                ContentBlock::Text {
                    text: "exact".into(),
                    text_signature: encode_text_signature(&exact64, Some("commentary")),
                },
                ContentBlock::Text {
                    text: "surrogate".into(),
                    text_signature: encode_text_signature(
                        &overlong_surrogate,
                        Some("final_answer"),
                    ),
                },
                ContentBlock::Text {
                    text: "ascii".into(),
                    text_signature: encode_text_signature(&overlong_ascii, None),
                },
            ],
        ))];
        let items = replay_assistant_message_ids(&model, messages);
        assert_eq!(items.len(), 3);

        // Exactly 64 UTF-16 units stays intact, phase preserved.
        assert_eq!(items[0]["id"], exact64);
        assert_eq!(items[0]["phase"], "commentary");

        // Overlong ids hash deterministically to msg_${short_hash(id)}.
        let want_surrogate = format!("msg_{}", short_hash(&overlong_surrogate));
        assert_eq!(items[1]["id"], want_surrogate);
        assert_eq!(items[1]["phase"], "final_answer");
        assert!(items[1]["id"].as_str().unwrap().encode_utf16().count() <= 64);

        let want_ascii = format!("msg_{}", short_hash(&overlong_ascii));
        assert_eq!(items[2]["id"], want_ascii);
        assert!(items[2].get("phase").is_none());
        // Same input → same hash (deterministic).
        assert_eq!(want_ascii, format!("msg_{}", short_hash(&overlong_ascii)));
        assert_eq!(want_ascii, "msg_yl02lyv9wrwf");
    }

    #[test]
    fn shared_transform_synthesizes_orphan_result_and_normalizes_responses_ids() {
        let model = Model {
            id: "gpt-target".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let mut source = AssistantMessage::pending(&Model {
            id: "claude-source".into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: "anthropic".into(),
            ..Model::default()
        });
        source.stop_reason = StopReason::ToolUse;
        source.content = vec![ContentBlock::ToolCall(ToolCall {
            id: "call bad|foreign item??".into(),
            name: "read".into(),
            arguments: json!({"path":"x"}),
            thought_signature: Some("foreign".into()),
        })];
        let req = Context {
            messages: vec![
                Message::Assistant(source),
                Message::user_text("continue", 2),
            ],
            ..Context::default()
        };

        let payload = build_responses_params(&model, &req, &OpenAIResponsesOptions::default())
            .expect("transformed payload");
        let input = payload["input"].as_array().expect("input array");
        let normalized = format!("call_bad|fc_{}", short_hash("foreign item??"));
        let (call_id, item_id) = split_tool_call_id(&normalized);
        assert_eq!(input[0]["call_id"], call_id);
        assert_eq!(input[0]["id"], item_id);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], input[0]["call_id"]);
        assert_eq!(input[1]["output"], "No result provided");
        assert_eq!(input[2]["content"][0]["text"], "continue");
    }

    #[test]
    fn shared_runtime_url_attribution_and_json_repair_are_integrated() {
        let model = Model {
            id: "gpt-target".into(), api: API_OPENAI_RESPONSES.into(), provider: "opencode".into(), reasoning: true,
            base_url: "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai/".into(),
            thinking_level_map: Some(HashMap::from([("xhigh".into(), Some("xhigh".into()))])),
            headers: Some(HashMap::from([("x-model-header".into(), "model".into())])),
            ..Model::default()
        };
        let ctx = Context {
            system_prompt: "context".repeat(32),
            ..Context::default()
        };
        let stream = StreamOptions {
            max_tokens: Some(50_000),
            session_id: Some("session-1".into()),
            headers: HashMap::from([("x-request-header".into(), "request".into())]),
            env: HashMap::from([
                (CLOUDFLARE_ACCOUNT_ID_ENV.into(), "account".into()),
                (CLOUDFLARE_GATEWAY_ID_ENV.into(), "gateway".into()),
            ]),
            ..StreamOptions::default()
        };
        let expected_max = clamp_max_tokens_to_context(&model, &ctx, 50_000);
        let built = build_simple_responses_options(
            &model,
            &ctx,
            SimpleStreamOptions {
                stream: stream.clone(),
                reasoning: Some(ThinkingLevel::Max),
                thinking_budgets: None,
            },
        );
        assert_eq!(built.stream.max_tokens, Some(expected_max));
        assert_eq!(built.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(
            responses_base_url(&model, &stream).expect("resolved URL"),
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai"
        );
        let headers =
            responses_request_headers(&model, &ctx, &stream, "provider-key").expect("headers");
        assert_eq!(
            headers
                .get("x-opencode-session")
                .and_then(|value| value.to_str().ok()),
            Some("session-1")
        );
        assert_eq!(
            headers
                .get("x-opencode-client")
                .and_then(|value| value.to_str().ok()),
            Some("pi")
        );
        assert_eq!(
            headers
                .get("x-model-header")
                .and_then(|value| value.to_str().ok()),
            Some("model")
        );
        assert_eq!(
            headers
                .get("x-request-header")
                .and_then(|value| value.to_str().ok()),
            Some("request")
        );
        assert!(
            !headers.contains_key("User-Agent"),
            "telemetry-gated Cloudflare attribution remains deferred"
        );

        let mut out = AssistantMessage::pending(&model);
        let mut state = StreamState::new();
        let (tx, _rx) = unbounded_channel();
        let repaired = "\u{feff}```json\n{\"type\":\"response.created\",\"response\":{\"id\":\"repaired\"},}\n```";
        handle_event(None, repaired, &mut out, &mut state, &model, None, &tx)
            .expect("repaired SSE JSON");
        assert_eq!(out.response_id.as_deref(), Some("repaired"));
    }

    #[test]
    fn short_hash_matches_upstream_vectors() {
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("foreign item??"), "ynoe9u3sbf6f");
        assert_eq!(short_hash("call_123"), "1xcma7q1veaxnf");
        assert_eq!(short_hash("tool-🚀-id"), "uqbpgwnqnha0");
    }

    fn deferred_tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("The {name} tool"),
            parameters: Schema::object(HashMap::new(), Vec::new()),
            constrained_sampling: None,
        }
    }

    fn deferred_tool_context(model: &Model, tools: Vec<ToolDefinition>, added: &[&str]) -> Context {
        let mut assistant = AssistantMessage::pending(model);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![ContentBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "base_tool".into(),
            arguments: json!({}),
            thought_signature: None,
        })];
        Context {
            messages: vec![
                Message::user_text("hi", 1),
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    tool_name: "base_tool".into(),
                    content: vec![ContentBlock::text("done")],
                    details: None,
                    usage: None,
                    added_tool_names: added.iter().map(|s| (*s).to_string()).collect(),
                    is_error: false,
                    timestamp: 3,
                }),
                Message::user_text("next", 4),
            ],
            tools,
            ..Context::default()
        }
    }

    #[test]
    fn deferred_tool_search_emits_call_and_output_when_enabled() {
        let model = Model {
            id: "gpt-5.4".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            compat: Some(json!({ "supportsToolSearch": true })),
            ..Model::default()
        };
        let req = deferred_tool_context(
            &model,
            vec![
                deferred_tool_def("base_tool"),
                deferred_tool_def("late_tool"),
            ],
            &["late_tool"],
        );
        let body = build_responses_params(&model, &req, &OpenAIResponsesOptions::default())
            .expect("deferred tool-search payload");

        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "base_tool");
        assert!(tools[0].get("defer_loading").is_none());

        let input = body["input"].as_array().expect("input array");
        let call = input
            .iter()
            .find(|item| item["type"] == "tool_search_call")
            .expect("tool_search_call present");
        let out = input
            .iter()
            .find(|item| item["type"] == "tool_search_output")
            .expect("tool_search_output present");
        assert_eq!(call["execution"], "client");
        assert_eq!(call["status"], "completed");
        assert_eq!(call["call_id"], out["call_id"]);
        let want_id = format!("pi_tool_load_{}", short_hash("call_1:late_tool"));
        assert_eq!(call["call_id"], want_id);
        assert_eq!(call["arguments"]["query"], "late_tool");
        assert_eq!(call["arguments"]["limit"], 1);

        let out_tools = out["tools"].as_array().expect("tool_search_output.tools");
        assert_eq!(out_tools.len(), 1);
        assert_eq!(out_tools[0]["name"], "late_tool");
        assert_eq!(out_tools[0]["defer_loading"], true);

        // function_call_output still precedes the tool_search items.
        let fco_idx = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .unwrap();
        let call_idx = input
            .iter()
            .position(|item| item["type"] == "tool_search_call")
            .unwrap();
        let out_idx = input
            .iter()
            .position(|item| item["type"] == "tool_search_output")
            .unwrap();
        assert!(fco_idx < call_idx && call_idx + 1 == out_idx);
    }

    #[test]
    fn deferred_tool_search_disabled_sends_all_tools_normally() {
        let model = Model {
            id: "gpt-5.4".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        let req = deferred_tool_context(
            &model,
            vec![
                deferred_tool_def("base_tool"),
                deferred_tool_def("late_tool"),
            ],
            &["late_tool"],
        );
        let body = build_responses_params(&model, &req, &OpenAIResponsesOptions::default())
            .expect("disabled deferred payload");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "base_tool");
        assert_eq!(tools[1]["name"], "late_tool");
        assert!(tools.iter().all(|t| t.get("defer_loading").is_none()));

        let input = body["input"].as_array().expect("input array");
        assert!(input.iter().all(|item| {
            item["type"] != "tool_search_call" && item["type"] != "tool_search_output"
        }));
    }

    #[test]
    fn prompt_cache_and_service_tier_params_use_existing_fields() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            compat: Some(json!({
                "supportsLongCacheRetention": true,
                "supportsExplicitPromptCacheMode": true,
            })),
            ..Model::default()
        };
        let req = Context {
            messages: vec![Message::user_text("hi", 1)],
            ..Context::default()
        };

        let long = build_responses_params(
            &model,
            &req,
            &OpenAIResponsesOptions {
                stream: StreamOptions {
                    session_id: Some("session-123".into()),
                    cache_retention: CacheRetention::Long,
                    ..StreamOptions::default()
                },
                service_tier: Some("flex".into()),
                ..OpenAIResponsesOptions::default()
            },
        )
        .expect("long cache payload");
        assert_eq!(long["prompt_cache_key"], "session-123");
        assert_eq!(long["prompt_cache_retention"], "24h");
        assert_eq!(long["service_tier"], "flex");
        assert!(long.get("prompt_cache_options").is_none());

        // Retention is independent of sessionId; key still requires one.
        let long_no_session = build_responses_params(
            &model,
            &req,
            &OpenAIResponsesOptions {
                stream: StreamOptions {
                    cache_retention: CacheRetention::Long,
                    ..StreamOptions::default()
                },
                ..OpenAIResponsesOptions::default()
            },
        )
        .expect("long without session");
        assert_eq!(long_no_session["prompt_cache_retention"], "24h");
        assert!(long_no_session.get("prompt_cache_key").is_none());

        let none_explicit = build_responses_params(
            &model,
            &req,
            &OpenAIResponsesOptions {
                stream: StreamOptions {
                    session_id: Some("sess-1".into()),
                    cache_retention: CacheRetention::None,
                    ..StreamOptions::default()
                },
                ..OpenAIResponsesOptions::default()
            },
        )
        .expect("explicit none payload");
        assert!(none_explicit.get("prompt_cache_key").is_none());
        assert!(none_explicit.get("prompt_cache_retention").is_none());
        assert_eq!(
            none_explicit["prompt_cache_options"],
            json!({ "mode": "explicit" })
        );

        let long_key = "あ".repeat(70);
        assert_eq!(clamp_prompt_cache_key(&long_key).chars().count(), 64);
        assert_eq!(clamp_prompt_cache_key("short"), "short");
    }

    #[test]
    fn service_tier_pricing_multiplies_usage_cost() {
        let mut model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            ..Model::default()
        };
        let mut usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            reasoning: 0,
            total_tokens: 2_000_000,
            cost: CostBreakdown::default(),
        };
        calculate_cost(&model, &mut usage);
        let base_total = usage.cost.total;

        let mut flex = usage.clone();
        apply_responses_service_tier_pricing(&mut flex, "flex", &model);
        assert!((flex.cost.total - base_total * 0.5).abs() < 1e-9);

        let mut priority = usage.clone();
        apply_responses_service_tier_pricing(&mut priority, "priority", &model);
        assert!((priority.cost.total - base_total * 2.0).abs() < 1e-9);

        model.id = "gpt-5.5".into();
        let mut gpt55 = usage.clone();
        apply_responses_service_tier_pricing(&mut gpt55, "priority", &model);
        assert!((gpt55.cost.total - base_total * 2.5).abs() < 1e-9);

        // Terminal response service_tier wins over the requested option.
        let mut out = AssistantMessage::pending(&model);
        let mut state = StreamState::new();
        let (tx, _rx) = unbounded_channel();
        let data = r#"{"type":"response.completed","response":{"id":"resp_tier","status":"completed","service_tier":"flex","usage":{"input_tokens":1000000,"output_tokens":1000000,"total_tokens":2000000}}}"#;
        handle_event(
            Some("response.completed"),
            data,
            &mut out,
            &mut state,
            &model,
            Some("priority"),
            &tx,
        )
        .expect("completed with service_tier");
        assert!(
            (out.usage.cost.total - base_total * 0.5).abs() < 1e-9,
            "response tier must win"
        );
    }

    #[test]
    fn service_tier_cost_multiplier_noop_tiers_return_exact_one() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            ..Model::default()
        };
        // "" is the fallback used when neither response nor requested tier is
        // present (handle_event does `.or(requested).unwrap_or("")`); auto/default
        // are valid OpenAI tiers that must be billed at face value, and any other
        // unknown string falls through to the wildcard arm.
        for tier in ["", "auto", "default", "auto-tier-42", "nonexistent"] {
            assert_eq!(
                service_tier_cost_multiplier(&model, tier),
                1.0,
                "tier {tier:?} must map to the exact 1.0 fallback multiplier",
            );
        }
        // Sanity: the scaling arms still deviate so the table above is meaningful.
        assert_eq!(service_tier_cost_multiplier(&model, "flex"), 0.5);
        assert_eq!(service_tier_cost_multiplier(&model, "priority"), 2.0);
    }

    #[test]
    fn apply_responses_service_tier_pricing_noop_tiers_leave_cost_unchanged() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.3125,
                cache_write: 1.875,
                tiers: Vec::new(),
            },
            ..Model::default()
        };
        // Non-trivial cache components so a stray multiplier would perturb every
        // field, not just input/output.
        let mut base = Usage {
            input: 1_000_000,
            output: 500_000,
            cache_read: 200_000,
            cache_write: 100_000,
            cache_write_1h: 0,
            reasoning: 0,
            total_tokens: 1_800_000,
            cost: CostBreakdown::default(),
        };
        calculate_cost(&model, &mut base);
        let expected = base.cost.clone();

        for tier in ["", "auto", "default", "nonexistent-tier"] {
            let mut usage = base.clone();
            apply_responses_service_tier_pricing(&mut usage, tier, &model);
            assert_eq!(
                usage.cost, expected,
                "tier {tier:?} must not perturb any cost field (no-op early return)",
            );
            // No recompute drift: total still equals the summed components.
            assert!(
                (usage.cost.total
                    - (usage.cost.input
                        + usage.cost.output
                        + usage.cost.cache_read
                        + usage.cost.cache_write))
                    .abs()
                    < 1e-12,
                "tier {tier:?} total must equal summed components",
            );
        }
    }

    #[test]
    fn response_completed_without_service_tier_leaves_cost_unchanged() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            ..Model::default()
        };
        let mut baseline = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            reasoning: 0,
            total_tokens: 2_000_000,
            cost: CostBreakdown::default(),
        };
        calculate_cost(&model, &mut baseline);
        let expected_cost = baseline.cost.clone();

        let mut out = AssistantMessage::pending(&model);
        let mut state = StreamState::new();
        let (tx, _rx) = unbounded_channel();
        // Neither a `service_tier` field on the response nor a requested tier:
        // the event path falls back to "" and must leave the cost untouched.
        let data = r#"{"type":"response.completed","response":{"id":"resp_noop","status":"completed","usage":{"input_tokens":1000000,"output_tokens":1000000,"total_tokens":2000000}}}"#;
        handle_event(
            Some("response.completed"),
            data,
            &mut out,
            &mut state,
            &model,
            None,
            &tx,
        )
        .expect("completed without service_tier");
        assert_eq!(out.usage.input, 1_000_000);
        assert_eq!(out.usage.output, 1_000_000);
        assert_eq!(
            out.usage.cost, expected_cost,
            "absent service_tier must leave cost unchanged"
        );
    }

    #[test]
    fn response_completed_noop_tier_overrides_requested_priority_and_still_noop() {
        let model = Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            ..Model::default()
        };
        let mut baseline = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            reasoning: 0,
            total_tokens: 2_000_000,
            cost: CostBreakdown::default(),
        };
        calculate_cost(&model, &mut baseline);
        let expected_cost = baseline.cost.clone();
        let priority_total = expected_cost.total * 2.0;

        for tier in ["auto", "default", "nonexistent-tier-7"] {
            let mut out = AssistantMessage::pending(&model);
            let mut state = StreamState::new();
            let (tx, _rx) = unbounded_channel();
            // Requested `priority` (x2) is overridden by the response-reported
            // no-op tier, which must win AND leave the cost at the base.
            let data = format!(
                r#"{{"type":"response.completed","response":{{"id":"resp_{tier}","status":"completed","service_tier":"{tier}","usage":{{"input_tokens":1000000,"output_tokens":1000000,"total_tokens":2000000}}}}}}"#,
            );
            handle_event(
                Some("response.completed"),
                &data,
                &mut out,
                &mut state,
                &model,
                Some("priority"),
                &tx,
            )
            .expect("completed with no-op service_tier");
            assert_eq!(
                out.usage.cost, expected_cost,
                "tier {tier:?} must override the requested priority multiplier yet be a no-op",
            );
            assert!(
                out.usage.cost.total < priority_total,
                "tier {tier:?} must not inherit the requested priority (x2) cost",
            );
        }
    }

    fn constrained_response_tool(sampling: ConstrainedSampling) -> ToolDefinition {
        ToolDefinition {
            name: "query".into(),
            description: "query data".into(),
            parameters: Schema::object(
                HashMap::from([(
                    "query".into(),
                    Schema {
                        schema_type: Some(json!("string")),
                        ..Schema::default()
                    },
                )]),
                vec!["query".into()],
            ),
            constrained_sampling: Some(sampling),
        }
    }

    fn constrained_response_model(compat: Value) -> Model {
        Model {
            id: "gpt-5".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            compat: Some(compat),
            ..Model::default()
        }
    }

    #[test]
    fn responses_strict_prefer_and_require_follow_compat() {
        let context = |strict| Context {
            tools: vec![constrained_response_tool(ConstrainedSampling::json_schema(
                strict,
            ))],
            ..Context::default()
        };
        let supported = constrained_response_model(json!({"supportsStrictMode":true}));
        for strictness in [
            ConstrainedSamplingStrictness::Prefer,
            ConstrainedSamplingStrictness::Require,
        ] {
            let body = build_responses_params(
                &supported,
                &context(strictness),
                &OpenAIResponsesOptions::default(),
            )
            .expect("supported strict tool");
            assert_eq!(body["tools"][0]["strict"], true);
        }
        let unsupported = constrained_response_model(json!({"supportsStrictMode":false}));
        let fallback = build_responses_params(
            &unsupported,
            &context(ConstrainedSamplingStrictness::Prefer),
            &OpenAIResponsesOptions::default(),
        )
        .expect("prefer fallback");
        assert!(fallback["tools"][0].get("strict").is_none());
        let error = build_responses_params(
            &unsupported,
            &context(ConstrainedSamplingStrictness::Require),
            &OpenAIResponsesOptions::default(),
        )
        .expect_err("require error");
        assert_eq!(
            error.to_string(),
            "Tool \"query\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    #[test]
    fn responses_grammar_definition_fallback_and_deferred_flag() {
        let grammar = constrained_response_tool(ConstrainedSampling::grammar(GrammarVariants {
            openai_lark: Some("start: /.+/".into()),
            openai_regex: None,
        }));
        let model = constrained_response_model(json!({"supportsOpenAIGrammarTools":true}));
        let tools = convert_tools(&[&grammar], &get_responses_compat(&model), true)
            .expect("grammar definition");
        assert_eq!(
            tools,
            vec![
                json!({"type":"custom", "name":"query", "description":"query data", "format":{"type":"grammar", "syntax":"lark", "definition":"start: /.+/"}, "defer_loading":true})
            ]
        );
        let unsupported_model =
            constrained_response_model(json!({"supportsOpenAIGrammarTools":false}));
        let fallback = convert_tools(
            &[&grammar],
            &get_responses_compat(&unsupported_model),
            false,
        )
        .expect("grammar fallback");
        assert_eq!(fallback[0]["type"], "function");
        assert!(fallback[0].get("strict").is_none());
    }

    #[test]
    fn responses_custom_call_and_output_replay_round_trip() {
        let model = constrained_response_model(json!({"supportsOpenAIGrammarTools":true}));
        let grammar = constrained_response_tool(ConstrainedSampling::grammar(GrammarVariants {
            openai_lark: Some("start: /.+/".into()),
            openai_regex: None,
        }));
        let mut assistant = AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![ContentBlock::ToolCall(ToolCall {
            id: "ctc_call|ctc_item".into(),
            name: "query".into(),
            arguments: json!({"query":"SELECT 1"}),
            thought_signature: None,
        })];
        let req = Context {
            messages: vec![
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "ctc_call|ctc_item".into(),
                    tool_name: "query".into(),
                    content: vec![ContentBlock::text("ok")],
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    timestamp: 1,
                }),
            ],
            tools: vec![grammar],
            ..Context::default()
        };
        let body = build_responses_params(&model, &req, &OpenAIResponsesOptions::default())
            .expect("custom replay");
        let input = body["input"].as_array().expect("input array");
        assert_eq!(
            input[0],
            json!({"type":"custom_tool_call", "call_id":"ctc_call", "id":"ctc_item", "name":"query", "input":"SELECT 1"})
        );
        assert_eq!(
            input[1],
            json!({"type":"custom_tool_call_output", "call_id":"ctc_call", "output":"ok"})
        );
    }

    #[test]
    fn responses_streamed_custom_call_preserves_seed_and_emits_json_deltas() {
        let model = constrained_response_model(json!({"supportsOpenAIGrammarTools":true}));
        let mut state = StreamState::with_grammar_input_properties(HashMap::from([(
            "query".into(),
            "query".into(),
        )]));
        let mut out = AssistantMessage::pending(&model);
        let (tx, mut rx) = unbounded_channel();
        for event in [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_item","call_id":"ctc_call","name":"query","input":"a"}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"b"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_item","call_id":"ctc_call","name":"query","input":"ab"}}"#,
        ] {
            handle_event(None, event, &mut out, &mut state, &model, None, &tx)
                .expect("custom event");
        }
        let mut deltas = Vec::new();
        let mut ended = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AssistantMessageEvent::ToolCallDelta { delta, .. } => deltas.push(delta),
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => ended = Some(tool_call),
                _ => {}
            }
        }
        assert_eq!(deltas, vec![r#"{"query":"ab"#, r#""}"#]);
        let call = ended.expect("custom tool end");
        assert_eq!(call.id, "ctc_call|ctc_item");
        assert_eq!(call.arguments, json!({"query":"ab"}));
    }

    #[test]
    fn responses_ordinary_function_tool_remains_function_shaped() {
        let model = constrained_response_model(
            json!({"supportsStrictMode":true, "supportsOpenAIGrammarTools":true}),
        );
        let tool = ToolDefinition {
            name: "ordinary".into(),
            description: "ordinary function".into(),
            parameters: Schema::object(HashMap::new(), Vec::new()),
            constrained_sampling: None,
        };
        let body = build_responses_params(
            &model,
            &Context {
                tools: vec![tool],
                ..Context::default()
            },
            &OpenAIResponsesOptions::default(),
        )
        .expect("ordinary tool");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("format").is_none());
    }
}
