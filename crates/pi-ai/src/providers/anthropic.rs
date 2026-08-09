//! Anthropic Messages API provider (`anthropic-messages`).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, bail};
use futures_util::FutureExt;
use regex::Regex;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::*;

use super::common::{
    apply_provider_headers, apply_provider_request, client, consume_sse, error_body, fail,
    insert_header, insert_header_map, is_aborted, notify_response, send_with_retry,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const FINE_GRAINED_TOOL_STREAM_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const PROVIDER_STOPPED_PREFIX: &str = "Provider stopped with: ";

const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Provider-native options for [`stream_anthropic`].
#[derive(Clone, Debug, Default)]
pub struct AnthropicOptions {
    pub stream: StreamOptions,
    /// When false, the `thinking` key is omitted (pi `undefined`). When true,
    /// [`Self::thinking_enabled`] selects enabled/adaptive vs `{type:"disabled"}`.
    pub thinking_provided: bool,
    pub thinking_enabled: bool,
    pub thinking_budget_tokens: i64,
    pub effort: Option<String>,
    pub thinking_display: Option<String>,
    pub interleaved_thinking: Option<bool>,
    pub tool_choice: Option<Value>,
}

#[derive(Clone, Copy, Debug)]
struct AnthropicCompat {
    supports_eager_tool_input_streaming: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    allow_empty_signature: bool,
    force_adaptive_thinking: bool,
    supports_strict_tools: bool,
    #[allow(dead_code)]
    supports_tool_references: bool,
}

fn get_anthropic_compat(model: &Model) -> AnthropicCompat {
    let mut c = AnthropicCompat {
        supports_eager_tool_input_streaming: true,
        supports_long_cache_retention: true,
        send_session_affinity_headers: false,
        supports_cache_control_on_tools: true,
        supports_temperature: true,
        allow_empty_signature: false,
        force_adaptive_thinking: false,
        supports_strict_tools: false,
        supports_tool_references: default_supports_tool_references(model),
    };
    if let Some(compat) = &model.compat {
        if let Some(v) = compat
            .get("supportsEagerToolInputStreaming")
            .and_then(Value::as_bool)
        {
            c.supports_eager_tool_input_streaming = v;
        }
        if let Some(v) = compat
            .get("supportsLongCacheRetention")
            .and_then(Value::as_bool)
        {
            c.supports_long_cache_retention = v;
        }
        if let Some(v) = compat
            .get("sendSessionAffinityHeaders")
            .and_then(Value::as_bool)
        {
            c.send_session_affinity_headers = v;
        }
        if let Some(v) = compat
            .get("supportsCacheControlOnTools")
            .and_then(Value::as_bool)
        {
            c.supports_cache_control_on_tools = v;
        }
        if let Some(v) = compat.get("supportsTemperature").and_then(Value::as_bool) {
            c.supports_temperature = v;
        }
        if let Some(v) = compat.get("allowEmptySignature").and_then(Value::as_bool) {
            c.allow_empty_signature = v;
        }
        if let Some(v) = compat.get("forceAdaptiveThinking").and_then(Value::as_bool) {
            c.force_adaptive_thinking = v;
        }
        if let Some(v) = compat.get("supportsStrictTools").and_then(Value::as_bool) {
            c.supports_strict_tools = v;
        }
        if let Some(v) = compat
            .get("supportsToolReferences")
            .and_then(Value::as_bool)
        {
            c.supports_tool_references = v;
        }
    }
    c
}

fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let re = Regex::new(r"^claude-(?:opus|sonnet|fable)-(\d+)(?:-(\d+))?(?:-|$)").expect("regex");
    let Some(caps) = re.captures(&model.id) else {
        return false;
    };
    let Ok(major) = caps[1].parse::<i32>() else {
        return false;
    };
    let mut minor = 0;
    if let Some(m) = caps.get(2) {
        if m.as_str().len() < 8 {
            if let Ok(v) = m.as_str().parse::<i32>() {
                minor = v;
            }
        }
    }
    major > 4 || (major == 4 && minor >= 5)
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

fn get_cache_control(
    model: &Model,
    retention: CacheRetention,
    env: &HashMap<String, String>,
) -> Option<Value> {
    let r = resolve_cache_retention(retention, env);
    if r == CacheRetention::None {
        return None;
    }
    let mut cc = Map::new();
    cc.insert("type".into(), json!("ephemeral"));
    if r == CacheRetention::Long && get_anthropic_compat(model).supports_long_cache_retention {
        cc.insert("ttl".into(), json!("1h"));
    }
    Some(Value::Object(cc))
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn to_claude_code_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.to_ascii_lowercase() == lower)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    let lower = name.to_ascii_lowercase();
    tools
        .iter()
        .find(|t| t.name.to_ascii_lowercase() == lower)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

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

fn sanitize_surrogates(s: &str) -> String {
    s.to_string()
}

fn simple_max_tokens_default(model: &Model, opts: &SimpleStreamOptions) -> i64 {
    opts.stream.max_tokens.unwrap_or(model.max_tokens).max(1)
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

fn clamp_anthropic_thinking_level(model: &Model, level: ThinkingLevel) -> Option<ThinkingLevel> {
    match crate::clamp_thinking_level(model, thinking_level_name(level)) {
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    if let Some(Some(mapped)) = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(thinking_level_name(level)))
    {
        return mapped.clone();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low".into(),
        ThinkingLevel::Medium => "medium".into(),
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => "high".into(),
    }
}

fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<i64>,
    model_max_tokens: i64,
    level: ThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> (i64, i64) {
    let mut budgets = HashMap::from([
        (ThinkingLevel::Minimal, 1024i64),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 16384),
    ]);
    if let Some(c) = custom {
        if let Some(v) = c.minimal {
            budgets.insert(ThinkingLevel::Minimal, v);
        }
        if let Some(v) = c.low {
            budgets.insert(ThinkingLevel::Low, v);
        }
        if let Some(v) = c.medium {
            budgets.insert(ThinkingLevel::Medium, v);
        }
        if let Some(v) = c.high {
            budgets.insert(ThinkingLevel::High, v);
        }
    }
    let clamped = match level {
        ThinkingLevel::XHigh | ThinkingLevel::Max => ThinkingLevel::High,
        other => other,
    };
    let mut thinking_budget = *budgets.get(&clamped).unwrap_or(&1024);
    const MIN_OUTPUT: i64 = 1024;
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + thinking_budget).min(model_max_tokens),
    };
    if max_tokens <= thinking_budget {
        thinking_budget = (max_tokens - MIN_OUTPUT).max(0);
    }
    (max_tokens, thinking_budget)
}

fn convert_user_blocks(content: &[ContentBlock]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for b in content {
        match b {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({"type":"text","text": sanitize_surrogates(text)}));
            }
            ContentBlock::Image { data, mime_type } => {
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime_type,
                        "data": data,
                    }
                }));
            }
            _ => {}
        }
    }
    blocks
}

fn convert_assistant_blocks(
    am: &AssistantMessage,
    oauth: bool,
    allow_empty_sig: bool,
) -> Vec<Value> {
    let mut blocks = Vec::new();
    for b in &am.content {
        match b {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({"type":"text","text": sanitize_surrogates(text)}));
            }
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                redacted,
            } => {
                if *redacted {
                    blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": thinking_signature.clone().unwrap_or_default(),
                    }));
                    continue;
                }
                let sig = thinking_signature.clone().unwrap_or_default();
                let has_sig = !sig.trim().is_empty();
                if thinking.trim().is_empty() && !has_sig {
                    continue;
                }
                if !has_sig {
                    if allow_empty_sig {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": sanitize_surrogates(thinking),
                            "signature": "",
                        }));
                    } else {
                        blocks.push(json!({
                            "type": "text",
                            "text": sanitize_surrogates(thinking),
                        }));
                    }
                } else {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": sanitize_surrogates(thinking),
                        "signature": sig,
                    }));
                }
            }
            ContentBlock::ToolCall(tc) => {
                let name = if oauth {
                    to_claude_code_name(&tc.name)
                } else {
                    tc.name.clone()
                };
                let args = if tc.arguments.is_null() {
                    json!({})
                } else {
                    tc.arguments.clone()
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": name,
                    "input": args,
                }));
            }
            _ => {}
        }
    }
    blocks
}

fn convert_content_blocks(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|c| matches!(c, ContentBlock::Image { .. }));
    if !has_images {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        return Value::String(sanitize_surrogates(&texts.join("\n")));
    }
    let mut blocks = Vec::new();
    let mut has_text = false;
    for c in content {
        match c {
            ContentBlock::Text { text, .. } => {
                has_text = true;
                blocks.push(json!({"type":"text","text": sanitize_surrogates(text)}));
            }
            ContentBlock::Image { data, mime_type } => {
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime_type,
                        "data": data,
                    }
                }));
            }
            _ => {}
        }
    }
    if !has_text {
        blocks.insert(0, json!({"type":"text","text":"(see attached image)"}));
    }
    Value::Array(blocks)
}

struct ConvertedToolResult {
    tool_result: Value,
    sibling_content: Vec<Value>,
}

fn convert_tool_result(
    tr: &ToolResultMessage,
    oauth: bool,
    deferred_tool_names: &HashSet<String>,
    loaded_tool_names: &mut HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> ConvertedToolResult {
    let mut references = Vec::new();
    for name in &tr.added_tool_names {
        let normalized = normalize_tool_name(name);
        if !deferred_tool_names.contains(&normalized) || loaded_tool_names.contains(&normalized) {
            continue;
        }
        loaded_tool_names.insert(normalized);
        let ref_name = if oauth {
            to_claude_code_name(name)
        } else {
            name.clone()
        };
        references.push(json!({"type":"tool_reference","tool_name": ref_name}));
    }

    let converted_content = convert_content_blocks(&tr.content);
    let content = if references.is_empty() {
        converted_content.clone()
    } else {
        Value::Array(references.clone())
    };
    let result = json!({
        "type": "tool_result",
        "tool_use_id": tr.tool_call_id,
        "content": content,
        "is_error": tr.is_error,
    });

    let mut sibling = Vec::new();
    if !references.is_empty() {
        match converted_content {
            Value::String(s) => sibling.push(json!({"type":"text","text": s})),
            Value::Array(a) => sibling.extend(a),
            _ => {}
        }
    }
    ConvertedToolResult {
        tool_result: result,
        sibling_content: sibling,
    }
}

fn convert_anthropic_messages(
    transformed: &[Message],
    oauth: bool,
    cc: Option<&Value>,
    allow_empty_sig: bool,
    deferred_tool_names: &HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> Vec<Value> {
    let mut params = Vec::new();
    let mut loaded_tool_names: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(um) => {
                let blocks = convert_user_blocks(&um.content);
                if !blocks.is_empty() {
                    params.push(json!({"role":"user","content": blocks}));
                }
                i += 1;
            }
            Message::Assistant(am) => {
                let blocks = convert_assistant_blocks(am, oauth, allow_empty_sig);
                if !blocks.is_empty() {
                    params.push(json!({"role":"assistant","content": blocks}));
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                let mut tool_results = Vec::new();
                let mut sibling_content = Vec::new();
                while i < transformed.len() {
                    let Message::ToolResult(tr) = &transformed[i] else {
                        break;
                    };
                    let res = convert_tool_result(
                        tr,
                        oauth,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_tool_name,
                    );
                    tool_results.push(res.tool_result);
                    sibling_content.extend(res.sibling_content);
                    i += 1;
                }
                let mut content = tool_results;
                content.extend(sibling_content);
                params.push(json!({"role":"user","content": content}));
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("provider transforms project session messages")
            }
        }
    }

    if let Some(cc) = cc {
        if let Some(last) = params.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some("user") {
                if let Some(Value::Array(content)) = last.get_mut("content") {
                    if let Some(blk) = content.last_mut() {
                        if let Some(obj) = blk.as_object_mut() {
                            let t = obj.get("type").and_then(Value::as_str).unwrap_or("");
                            if matches!(t, "text" | "image" | "tool_result") {
                                obj.insert("cache_control".into(), cc.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    params
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

fn convert_anthropic_tools(
    tools: &[&Tool],
    oauth: bool,
    eager: bool,
    supports_strict_tools: bool,
    cc: Option<&Value>,
    defer_loading: bool,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(tools.len());
    for (i, t) in tools.iter().enumerate() {
        let name = if oauth {
            to_claude_code_name(&t.name)
        } else {
            t.name.clone()
        };
        let strict = resolve_json_schema_strict_sampling(t, supports_strict_tools)?;
        let schema_val = serde_json::to_value(&t.parameters).unwrap_or_else(|_| json!({}));
        let props = schema_val
            .get("properties")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let required = schema_val
            .get("required")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let mut input_schema = json!({
            "type": "object",
            "properties": props,
            "required": required,
        });
        if strict {
            if let Value::Object(mut full) = schema_val {
                if let Value::Object(legacy) = &input_schema {
                    for (k, v) in legacy {
                        full.insert(k.clone(), v.clone());
                    }
                }
                input_schema = Value::Object(full);
            }
        }
        let mut tool = json!({
            "name": name,
            "description": t.description,
            "input_schema": input_schema,
        });
        let obj = tool.as_object_mut().expect("tool object");
        if eager {
            obj.insert("eager_input_streaming".into(), json!(true));
        }
        if strict {
            obj.insert("strict".into(), json!(true));
        }
        if defer_loading {
            obj.insert("defer_loading".into(), json!(true));
        }
        if let Some(cc) = cc {
            if i + 1 == tools.len() {
                obj.insert("cache_control".into(), cc.clone());
            }
        }
        out.push(tool);
    }
    Ok(out)
}
fn split_deferred_tools<'a>(
    tools: &'a [Tool],
    messages: &[Message],
    enabled: bool,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (Vec<&'a Tool>, Vec<&'a Tool>) {
    let mut unique_tools: Vec<(String, &Tool)> = Vec::with_capacity(tools.len());
    for tool in tools {
        let normalized = normalize_tool_name(&tool.name);
        if let Some(index) = unique_tools
            .iter()
            .position(|(name, _)| name == &normalized)
        {
            unique_tools.remove(index);
        }
        unique_tools.push((normalized, tool));
    }
    if !enabled {
        return (
            unique_tools.into_iter().map(|(_, tool)| tool).collect(),
            Vec::new(),
        );
    }

    let mut deferred_names = HashSet::new();
    let mut used_names = HashSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        used_names.insert(normalize_tool_name(&call.name));
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in &result.added_tool_names {
                    let normalized = normalize_tool_name(name);
                    if !used_names.contains(&normalized) {
                        deferred_names.insert(normalized);
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

    let mut immediate = Vec::with_capacity(unique_tools.len());
    let mut deferred = Vec::new();
    for (name, tool) in unique_tools {
        if deferred_names.contains(&name) {
            deferred.push(tool);
        } else {
            immediate.push(tool);
        }
    }
    if immediate.is_empty() && !deferred.is_empty() {
        immediate = deferred;
        deferred = Vec::new();
    }
    (immediate, deferred)
}

fn build_anthropic_params(
    model: &Model,
    req: &Context,
    oauth: bool,
    opts: &AnthropicOptions,
) -> Result<Value> {
    let cc = get_cache_control(model, opts.stream.cache_retention, &opts.stream.env);
    let compat = get_anthropic_compat(model);
    let max_tokens = clamp_max_tokens_to_context(
        model,
        req,
        opts.stream.max_tokens.unwrap_or(model.max_tokens),
    );
    let transformed =
        crate::transform_messages(&req.messages, model, |id, _, _| normalize_tool_call_id(id));
    let normalize_tool_name: Box<dyn Fn(&str) -> String> = if oauth {
        Box::new(|n: &str| to_claude_code_name(n))
    } else {
        Box::new(|n: &str| n.to_string())
    };

    let (immediate_tools, deferred_tools) = split_deferred_tools(
        &req.tools,
        &transformed,
        compat.supports_tool_references,
        &*normalize_tool_name,
    );
    let deferred_tool_names: HashSet<String> = deferred_tools
        .iter()
        .map(|tool| normalize_tool_name(&tool.name))
        .collect();

    let mut params = json!({
        "model": model.id,
        "messages": convert_anthropic_messages(
            &transformed,
            oauth,
            cc.as_ref(),
            compat.allow_empty_signature,
            &deferred_tool_names,
            &*normalize_tool_name,
        ),
        "max_tokens": max_tokens,
        "stream": true,
    });

    let text_block = |text: &str| -> Value {
        let mut blk = json!({"type":"text","text": sanitize_surrogates(text)});
        if let Some(cc) = &cc {
            blk.as_object_mut()
                .expect("text block")
                .insert("cache_control".into(), cc.clone());
        }
        blk
    };

    if oauth {
        let mut system = vec![text_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
        )];
        if !req.system_prompt.is_empty() {
            system.push(text_block(&req.system_prompt));
        }
        params
            .as_object_mut()
            .expect("params")
            .insert("system".into(), Value::Array(system));
    } else if !req.system_prompt.is_empty() {
        params.as_object_mut().expect("params").insert(
            "system".into(),
            Value::Array(vec![text_block(&req.system_prompt)]),
        );
    }

    let thinking_on = opts.thinking_provided && opts.thinking_enabled;
    if let Some(temp) = opts.stream.temperature {
        if !thinking_on && compat.supports_temperature {
            params
                .as_object_mut()
                .expect("params")
                .insert("temperature".into(), json!(temp));
        }
    }

    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let tool_cc = if compat.supports_cache_control_on_tools {
            cc.as_ref()
        } else {
            None
        };
        let mut tools = convert_anthropic_tools(
            &immediate_tools,
            oauth,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            tool_cc,
            false,
        )?;
        tools.extend(convert_anthropic_tools(
            &deferred_tools,
            oauth,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            None,
            true,
        )?);
        params
            .as_object_mut()
            .expect("params")
            .insert("tools".into(), Value::Array(tools));
    }

    if model.reasoning && opts.thinking_provided {
        if opts.thinking_enabled {
            let display = opts
                .thinking_display
                .clone()
                .unwrap_or_else(|| "summarized".into());
            if compat.force_adaptive_thinking {
                params.as_object_mut().expect("params").insert(
                    "thinking".into(),
                    json!({"type":"adaptive","display": display}),
                );
                if let Some(effort) = &opts.effort {
                    if !effort.is_empty() {
                        params
                            .as_object_mut()
                            .expect("params")
                            .insert("output_config".into(), json!({"effort": effort}));
                    }
                }
            } else {
                let budget = if opts.thinking_budget_tokens == 0 {
                    1024
                } else {
                    opts.thinking_budget_tokens
                };
                params.as_object_mut().expect("params").insert(
                    "thinking".into(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": budget,
                        "display": display,
                    }),
                );
            }
        } else {
            let omit_disabled = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get("off"))
                .is_some_and(Option::is_none);
            if !omit_disabled {
                params
                    .as_object_mut()
                    .expect("params")
                    .insert("thinking".into(), json!({"type":"disabled"}));
            }
        }
    }

    if let Some(meta) = &opts.stream.metadata {
        if let Some(uid) = meta.get("user_id").and_then(Value::as_str) {
            params
                .as_object_mut()
                .expect("params")
                .insert("metadata".into(), json!({"user_id": uid}));
        }
    }

    if let Some(tc) = &opts.tool_choice {
        let tool_choice = match tc {
            Value::String(s) => json!({"type": s}),
            other => other.clone(),
        };
        params
            .as_object_mut()
            .expect("params")
            .insert("tool_choice".into(), tool_choice);
    }

    Ok(params)
}

fn has_effective_anthropic_auth_header(model: &Model, opts: &AnthropicOptions) -> bool {
    let recognized = if model.provider == "cloudflare-ai-gateway" {
        &["cf-aig-authorization"][..]
    } else {
        &["authorization", "x-api-key", "cf-aig-authorization"][..]
    };
    recognized.iter().any(|name| {
        let option_value = opts.stream.headers.iter().find_map(|(header, value)| {
            header.eq_ignore_ascii_case(name).then_some(value.as_str())
        });
        let model_value = model.headers.as_ref().and_then(|headers| {
            headers.iter().find_map(|(header, value)| {
                header.eq_ignore_ascii_case(name).then_some(value.as_str())
            })
        });
        option_value
            .or(model_value)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn validate_anthropic_auth(
    model: &Model,
    opts: &AnthropicOptions,
    api_key: &str,
    auth_token: &str,
) -> Result<()> {
    if !api_key.is_empty()
        || !auth_token.is_empty()
        || has_effective_anthropic_auth_header(model, opts)
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "No API key for provider: {}",
            model.provider
        ))
    }
}

fn anthropic_request_headers(
    model: &Model,
    context: &Context,
    opts: &AnthropicOptions,
    oauth: bool,
    api_key: &str,
    auth_token: &str,
    has_tools: bool,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    // Telemetry-gated attribution remains disabled until the caller exposes a verified setting.
    if let Some(attribution) = merge_provider_attribution_headers(
        model,
        opts.stream.session_id.as_deref(),
        false,
        &HashMap::new(),
    ) {
        insert_header_map(&mut headers, &attribution)?;
    }
    insert_header(&mut headers, "content-type", "application/json")?;
    insert_header(&mut headers, "accept", "application/json")?;
    insert_header(&mut headers, "anthropic-version", ANTHROPIC_VERSION)?;
    insert_header(
        &mut headers,
        "anthropic-dangerous-direct-browser-access",
        "true",
    )?;

    let compat = get_anthropic_compat(model);
    let mut betas = Vec::new();
    if has_tools && !compat.supports_eager_tool_input_streaming {
        betas.push(FINE_GRAINED_TOOL_STREAM_BETA.to_string());
    }
    if opts.interleaved_thinking.unwrap_or(true) && !compat.force_adaptive_thinking {
        betas.push(INTERLEAVED_THINKING_BETA.to_string());
    }

    if !auth_token.is_empty() {
        insert_header(
            &mut headers,
            "authorization",
            &format!("Bearer {auth_token}"),
        )?;
        if !betas.is_empty() {
            insert_header(&mut headers, "anthropic-beta", &betas.join(","))?;
        }
    } else if model.provider == "cloudflare-ai-gateway" {
        if !api_key.is_empty() {
            insert_header(
                &mut headers,
                "cf-aig-authorization",
                &format!("Bearer {api_key}"),
            )?;
        }
        if !betas.is_empty() {
            insert_header(&mut headers, "anthropic-beta", &betas.join(","))?;
        }
    } else if model.provider == "github-copilot" {
        if !api_key.is_empty() {
            insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
        }
        if !betas.is_empty() {
            insert_header(&mut headers, "anthropic-beta", &betas.join(","))?;
        }
    } else if oauth {
        insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
        insert_header(
            &mut headers,
            "user-agent",
            &format!("claude-cli/{CLAUDE_CODE_VERSION}"),
        )?;
        insert_header(&mut headers, "x-app", "cli")?;
        let mut oauth_betas = vec![
            "claude-code-20250219".to_string(),
            "oauth-2025-04-20".to_string(),
        ];
        oauth_betas.extend(betas);
        insert_header(&mut headers, "anthropic-beta", &oauth_betas.join(","))?;
    } else {
        if !api_key.is_empty() {
            insert_header(&mut headers, "x-api-key", api_key)?;
        }
        if !betas.is_empty() {
            insert_header(&mut headers, "anthropic-beta", &betas.join(","))?;
        }
        if let Some(sid) = &opts.stream.session_id {
            if !sid.is_empty()
                && compat.send_session_affinity_headers
                && resolve_cache_retention(opts.stream.cache_retention, &opts.stream.env)
                    != CacheRetention::None
            {
                insert_header(&mut headers, "x-session-affinity", sid)?;
            }
        }
    }

    if let Some(model_headers) = &model.headers {
        insert_header_map(&mut headers, model_headers)?;
    }
    if let Some(copilot_headers) = crate::github_copilot::dynamic_headers(&model.provider, context)
    {
        insert_header_map(&mut headers, &copilot_headers)?;
    }
    insert_header_map(&mut headers, &opts.stream.headers)?;
    Ok(headers)
}

fn map_anthropic_stop_reason(
    reason: &str,
    explanation: Option<&str>,
) -> Result<(StopReason, Option<String>)> {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => {
            let msg = explanation
                .filter(|s| !s.is_empty())
                .unwrap_or("The model refused to complete the request")
                .to_string();
            Ok((StopReason::Error, Some(msg)))
        }
        "sensitive" => Ok((
            StopReason::Error,
            Some(format!("{PROVIDER_STOPPED_PREFIX}sensitive")),
        )),
        other => bail!("Unhandled stop reason: {other}"),
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicUsageRaw {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    cache_creation: Option<CacheCreationRaw>,
    output_tokens_details: Option<OutputTokensDetailsRaw>,
}

#[derive(Debug, Deserialize)]
struct CacheCreationRaw {
    ephemeral_1h_input_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OutputTokensDetailsRaw {
    thinking_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    index: Option<usize>,
    message: Option<MessageStartRaw>,
    content_block: Option<ContentBlockRaw>,
    delta: Option<DeltaRaw>,
    usage: Option<AnthropicUsageRaw>,
}

#[derive(Debug, Deserialize)]
struct MessageStartRaw {
    id: Option<String>,
    usage: Option<AnthropicUsageRaw>,
}

#[derive(Debug, Deserialize)]
struct ContentBlockRaw {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    id: Option<String>,
    name: Option<String>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaRaw {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    partial_json: Option<String>,
    signature: Option<String>,
    stop_reason: Option<String>,
    stop_details: Option<StopDetailsRaw>,
}

#[derive(Debug, Deserialize)]
struct StopDetailsRaw {
    explanation: Option<String>,
}

fn apply_usage(usage: &mut Usage, u: &AnthropicUsageRaw, is_start: bool) {
    if is_start {
        usage.input = u.input_tokens.unwrap_or(0);
        usage.output = u.output_tokens.unwrap_or(0);
        usage.cache_read = u.cache_read_input_tokens.unwrap_or(0);
        usage.cache_write = u.cache_creation_input_tokens.unwrap_or(0);
        if let Some(cc) = &u.cache_creation {
            usage.cache_write_1h = cc.ephemeral_1h_input_tokens.unwrap_or(0);
        }
    } else {
        if let Some(v) = u.input_tokens {
            usage.input = v;
        }
        if let Some(v) = u.output_tokens {
            usage.output = v;
        }
        if let Some(v) = u.cache_read_input_tokens {
            usage.cache_read = v;
        }
        if let Some(v) = u.cache_creation_input_tokens {
            usage.cache_write = v;
        }
    }
    if let Some(details) = &u.output_tokens_details {
        if let Some(t) = details.thinking_tokens {
            usage.reasoning = t;
        }
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

fn complete_partial_json(s: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start: isize = -1;
    let bytes = s.as_bytes();
    for (i, &ch) in bytes.iter().enumerate() {
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
            b'"' => {
                in_string = true;
                string_start = i as isize;
            }
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                let _ = stack.pop();
            }
            _ => {}
        }
    }

    let mut completed = s.trim_end_matches([' ', '\t', '\r', '\n']).to_string();
    if in_string && is_dangling_object_key(s, &stack, string_start) {
        let start = string_start as usize;
        completed = s[..start]
            .trim_end_matches([' ', '\t', '\r', '\n'])
            .to_string();
        completed = completed.trim_end_matches(',').to_string();
        completed = trim_dangling_colon(&completed);
    } else if in_string {
        completed.push('"');
    } else {
        completed = completed.trim_end_matches(',').to_string();
        completed = trim_dangling_colon(&completed);
    }
    for &c in stack.iter().rev() {
        completed.push(c as char);
    }
    if completed.is_empty() {
        None
    } else {
        Some(completed)
    }
}

fn is_dangling_object_key(s: &str, stack: &[u8], string_start: isize) -> bool {
    if string_start < 0 || stack.last() != Some(&b'}') {
        return false;
    }
    let bytes = s.as_bytes();
    let mut j = string_start - 1;
    while j >= 0 {
        match bytes[j as usize] {
            b' ' | b'\t' | b'\r' | b'\n' => j -= 1,
            b'{' | b',' => return true,
            _ => return false,
        }
    }
    false
}

fn trim_dangling_colon(s: &str) -> String {
    let t = s.trim_end_matches([' ', '\t', '\r', '\n']);
    if t.ends_with(':') {
        if let Some(idx) = t.rfind(['{', '[', ',']) {
            return t[..=idx].to_string();
        }
    }
    s.to_string()
}

fn parse_streaming_json(partial: &str) -> Value {
    if partial.trim().is_empty() {
        return json!({});
    }
    if let Some(value) = parse_json_with_repair(partial).filter(Value::is_object) {
        return value;
    }
    if let Some(value) = complete_partial_json(partial)
        .as_deref()
        .and_then(parse_json_with_repair)
        .filter(Value::is_object)
    {
        return value;
    }
    json!({})
}

#[derive(Clone)]
struct BlockBuilder {
    kind: &'static str,
    text: String,
    thinking: String,
    thinking_sig: String,
    redacted: bool,
    tool_id: String,
    tool_name: String,
    partial_json: String,
    args: Value,
}

impl BlockBuilder {
    fn to_content(&self) -> ContentBlock {
        match self.kind {
            "text" => ContentBlock::text(self.text.clone()),
            "thinking" => ContentBlock::Thinking {
                thinking: self.thinking.clone(),
                thinking_signature: if self.thinking_sig.is_empty() {
                    None
                } else {
                    Some(self.thinking_sig.clone())
                },
                redacted: self.redacted,
            },
            "toolCall" => ContentBlock::ToolCall(ToolCall {
                id: self.tool_id.clone(),
                name: self.tool_name.clone(),
                arguments: if self.args.is_null() {
                    json!({})
                } else {
                    self.args.clone()
                },
                thought_signature: None,
            }),
            _ => ContentBlock::text(String::new()),
        }
    }
}

/// Maps unified reasoning onto Anthropic options, then streams.
pub async fn stream_simple_anthropic(
    model: Model,
    req: Context,
    opts: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let base_max =
        clamp_max_tokens_to_context(&model, &req, simple_max_tokens_default(&model, &opts));
    let mut aopts = AnthropicOptions {
        stream: opts.stream,
        thinking_provided: true,
        ..AnthropicOptions::default()
    };
    aopts.stream.max_tokens = Some(base_max);

    let Some(reasoning) = opts
        .reasoning
        .and_then(|level| clamp_anthropic_thinking_level(&model, level))
    else {
        aopts.thinking_enabled = false;
        return stream_anthropic(model, req, aopts).await;
    };

    let compat = get_anthropic_compat(&model);
    if compat.force_adaptive_thinking {
        aopts.thinking_enabled = true;
        aopts.effort = Some(map_thinking_level_to_effort(&model, reasoning));
        return stream_anthropic(model, req, aopts).await;
    }

    let (adjusted_max, thinking_budget) = adjust_max_tokens_for_thinking(
        aopts.stream.max_tokens,
        model.max_tokens,
        reasoning,
        opts.thinking_budgets.as_ref(),
    );
    let mt = clamp_max_tokens_to_context(&model, &req, adjusted_max);
    aopts.stream.max_tokens = Some(mt);
    aopts.thinking_enabled = true;
    aopts.thinking_budget_tokens = thinking_budget.min((mt - 1024).max(0));
    stream_anthropic(model, req, aopts).await
}

/// Streams an assistant response from the Anthropic Messages API.
pub async fn stream_anthropic(
    model: Model,
    req: Context,
    opts: AnthropicOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let stream2 = stream.clone();
    tokio::spawn(async move {
        run_anthropic_stream(stream2, model, req, opts).await;
    });
    stream
}

async fn run_anthropic_stream(
    stream: AssistantMessageEventStream,
    model: Model,
    req: Context,
    opts: AnthropicOptions,
) {
    let mut out = AssistantMessage::pending(&model);
    let aborted = || is_aborted(&opts.stream);

    let api_key = opts.stream.api_key.clone().unwrap_or_default();
    let mut auth_token = String::new();
    if model.provider == "anthropic" && api_key.is_empty() {
        auth_token = opts
            .stream
            .env
            .get("ANTHROPIC_AUTH_TOKEN")
            .cloned()
            .or_else(|| std::env::var("ANTHROPIC_AUTH_TOKEN").ok())
            .unwrap_or_default();
    }
    if let Err(error) = validate_anthropic_auth(&model, &opts, &api_key, &auth_token) {
        fail(&stream, out, error.to_string(), aborted()).await;
        return;
    }

    let oauth = auth_token.is_empty()
        && model.provider != "cloudflare-ai-gateway"
        && model.provider != "github-copilot"
        && is_oauth_token(&api_key);

    let body = match build_anthropic_params(&model, &req, oauth, &opts) {
        Ok(b) => b,
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };
    let body = match apply_provider_request(body, &model, &opts.stream).await {
        Ok(b) => b,
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };

    let configured_base_url = if model.base_url.is_empty() {
        ANTHROPIC_DEFAULT_BASE_URL
    } else {
        &model.base_url
    };
    let base_url = match resolve_base_url(configured_base_url, &opts.stream.env) {
        Ok(url) => url.trim_end_matches('/').to_string(),
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };
    let url = format!("{base_url}/v1/messages");

    let http = match client(&opts.stream) {
        Ok(c) => c,
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };

    let has_tools = !req.tools.is_empty();

    let request_headers = match anthropic_request_headers(
        &model,
        &req,
        &opts,
        oauth,
        &api_key,
        &auth_token,
        has_tools,
    ) {
        Ok(headers) => match apply_provider_headers(headers, &model, &opts.stream).await {
            Ok(headers) => headers,
            Err(e) => {
                fail(&stream, out, e.to_string(), aborted()).await;
                return;
            }
        },
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };
    let payload = body;
    let build = || {
        http.post(&url)
            .json(&payload)
            .headers(request_headers.clone())
    };

    let resp = match send_with_retry(&opts.stream, build).await {
        Ok(r) => r,
        Err(e) => {
            fail(&stream, out, e.to_string(), aborted()).await;
            return;
        }
    };
    if let Err(e) = notify_response(&opts.stream, &resp, &model).await {
        fail(&stream, out, e.to_string(), aborted()).await;
        return;
    }
    if !resp.status().is_success() {
        let (msg, aborted) = match error_body("Anthropic", resp, &opts.stream).await {
            Ok(m) => (m, aborted()),
            Err(e) => (e.to_string(), true),
        };
        fail(&stream, out, msg, aborted).await;
        return;
    }

    stream
        .push(AssistantMessageEvent::Start {
            partial: out.clone(),
        })
        .await;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AssistantMessageEvent>();
    let event_tx_cb = event_tx.clone();
    let event_stream = stream.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            event_stream.push(event).await;
        }
    });

    let mut builders: Vec<BlockBuilder> = Vec::new();
    let mut index_map: HashMap<usize, usize> = HashMap::new();
    let mut saw_start = false;
    let mut saw_stop = false;
    let mut fatal: Option<String> = None;

    let materialize = |builders: &[BlockBuilder], out: &mut AssistantMessage| {
        out.content = builders.iter().map(BlockBuilder::to_content).collect();
    };

    let sse = consume_sse(resp, &opts.stream, |name, data| {
        if fatal.is_some() {
            return Ok(());
        }
        if name == Some("error") {
            fatal = Some(data.to_string());
            return Ok(());
        }
        let known = matches!(
            name,
            Some(
                "message_start"
                    | "message_delta"
                    | "message_stop"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
            )
        );
        let Some(value) = parse_json_with_repair(data) else {
            if known || name.is_none() {
                fatal = Some(format!("Could not parse Anthropic SSE event: data={data}"));
            }
            return Ok(());
        };
        let ev: AnthropicStreamEvent = match serde_json::from_value(value) {
            Ok(event) => event,
            Err(e) => {
                if known || name.is_none() {
                    fatal = Some(format!(
                        "Could not decode Anthropic SSE event: {e}; data={data}"
                    ));
                }
                return Ok(());
            }
        };
        match ev.event_type.as_str() {
            "message_start" => {
                saw_start = true;
                if let Some(msg) = &ev.message {
                    if let Some(id) = &msg.id {
                        out.response_id = Some(id.clone());
                    }
                    if let Some(u) = &msg.usage {
                        apply_usage(&mut out.usage, u, true);
                        calculate_cost(&model, &mut out.usage);
                    }
                }
            }
            "content_block_start" => {
                let Some(cb) = &ev.content_block else {
                    return Ok(());
                };
                let idx_wire = ev.index.unwrap_or(0);
                let (builder, kind) = match cb.block_type.as_str() {
                    "text" => (
                        BlockBuilder {
                            kind: "text",
                            text: cb.text.clone().unwrap_or_default(),
                            thinking: String::new(),
                            thinking_sig: String::new(),
                            redacted: false,
                            tool_id: String::new(),
                            tool_name: String::new(),
                            partial_json: String::new(),
                            args: json!({}),
                        },
                        "text",
                    ),
                    "thinking" => (
                        BlockBuilder {
                            kind: "thinking",
                            text: String::new(),
                            thinking: cb.thinking.clone().unwrap_or_default(),
                            thinking_sig: cb.signature.clone().unwrap_or_default(),
                            redacted: false,
                            tool_id: String::new(),
                            tool_name: String::new(),
                            partial_json: String::new(),
                            args: json!({}),
                        },
                        "thinking",
                    ),
                    "redacted_thinking" => (
                        BlockBuilder {
                            kind: "thinking",
                            text: String::new(),
                            thinking: "[Reasoning redacted]".into(),
                            thinking_sig: cb.data.clone().unwrap_or_default(),
                            redacted: true,
                            tool_id: String::new(),
                            tool_name: String::new(),
                            partial_json: String::new(),
                            args: json!({}),
                        },
                        "thinking",
                    ),
                    "tool_use" => {
                        let mut name = cb.name.clone().unwrap_or_default();
                        if oauth {
                            name = from_claude_code_name(&name, &req.tools);
                        }
                        (
                            BlockBuilder {
                                kind: "toolCall",
                                text: String::new(),
                                thinking: String::new(),
                                thinking_sig: String::new(),
                                redacted: false,
                                tool_id: cb.id.clone().unwrap_or_default(),
                                tool_name: name,
                                partial_json: String::new(),
                                args: json!({}),
                            },
                            "tool",
                        )
                    }
                    _ => return Ok(()),
                };
                builders.push(builder);
                let content_index = builders.len() - 1;
                index_map.insert(idx_wire, content_index);
                materialize(&builders, &mut out);
                let event = match kind {
                    "text" => AssistantMessageEvent::TextStart {
                        content_index,
                        partial: out.clone(),
                    },
                    "thinking" => AssistantMessageEvent::ThinkingStart {
                        content_index,
                        partial: out.clone(),
                    },
                    "tool" => AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: out.clone(),
                    },
                    _ => return Ok(()),
                };
                if event_tx_cb.send(event).is_err() {
                    fatal = Some("Anthropic event stream closed".into());
                }
            }
            "content_block_delta" => {
                let Some(&idx) = ev.index.and_then(|i| index_map.get(&i)) else {
                    return Ok(());
                };
                let Some(delta) = &ev.delta else {
                    return Ok(());
                };
                let Some(dtype) = &delta.delta_type else {
                    return Ok(());
                };
                match dtype.as_str() {
                    "text_delta" => {
                        if builders[idx].kind != "text" {
                            return Ok(());
                        }
                        if let Some(t) = &delta.text {
                            builders[idx].text.push_str(t);
                            materialize(&builders, &mut out);
                            if event_tx_cb
                                .send(AssistantMessageEvent::TextDelta {
                                    content_index: idx,
                                    delta: t.clone(),
                                    partial: out.clone(),
                                })
                                .is_err()
                            {
                                fatal = Some("Anthropic event stream closed".into());
                            }
                        }
                    }
                    "thinking_delta" => {
                        if builders[idx].kind != "thinking" {
                            return Ok(());
                        }
                        if let Some(t) = &delta.thinking {
                            builders[idx].thinking.push_str(t);
                            materialize(&builders, &mut out);
                            if event_tx_cb
                                .send(AssistantMessageEvent::ThinkingDelta {
                                    content_index: idx,
                                    delta: t.clone(),
                                    partial: out.clone(),
                                })
                                .is_err()
                            {
                                fatal = Some("Anthropic event stream closed".into());
                            }
                        }
                    }
                    "input_json_delta" => {
                        if builders[idx].kind != "toolCall" {
                            return Ok(());
                        }
                        if let Some(pj) = &delta.partial_json {
                            builders[idx].partial_json.push_str(pj);
                            builders[idx].args = parse_streaming_json(&builders[idx].partial_json);
                            materialize(&builders, &mut out);
                            if event_tx_cb
                                .send(AssistantMessageEvent::ToolCallDelta {
                                    content_index: idx,
                                    delta: pj.clone(),
                                    partial: out.clone(),
                                })
                                .is_err()
                            {
                                fatal = Some("Anthropic event stream closed".into());
                            }
                        }
                    }
                    "signature_delta" => {
                        if builders[idx].kind != "thinking" {
                            return Ok(());
                        }
                        if let Some(sig) = &delta.signature {
                            builders[idx].thinking_sig.push_str(sig);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(&idx) = ev.index.and_then(|i| index_map.get(&i)) else {
                    return Ok(());
                };
                match builders[idx].kind {
                    "text" => {
                        materialize(&builders, &mut out);
                        if event_tx_cb
                            .send(AssistantMessageEvent::TextEnd {
                                content_index: idx,
                                content: builders[idx].text.clone(),
                                partial: out.clone(),
                            })
                            .is_err()
                        {
                            fatal = Some("Anthropic event stream closed".into());
                        }
                    }
                    "thinking" => {
                        materialize(&builders, &mut out);
                        if event_tx_cb
                            .send(AssistantMessageEvent::ThinkingEnd {
                                content_index: idx,
                                content: builders[idx].thinking.clone(),
                                partial: out.clone(),
                            })
                            .is_err()
                        {
                            fatal = Some("Anthropic event stream closed".into());
                        }
                    }
                    "toolCall" => {
                        builders[idx].args = parse_streaming_json(&builders[idx].partial_json);
                        materialize(&builders, &mut out);
                        let ContentBlock::ToolCall(tc) = builders[idx].to_content() else {
                            return Ok(());
                        };
                        if event_tx_cb
                            .send(AssistantMessageEvent::ToolCallEnd {
                                content_index: idx,
                                tool_call: tc,
                                partial: out.clone(),
                            })
                            .is_err()
                        {
                            fatal = Some("Anthropic event stream closed".into());
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(delta) = &ev.delta {
                    if let Some(reason) = &delta.stop_reason {
                        out.raw_stop_reason = Some(reason.clone());
                        let explanation = delta
                            .stop_details
                            .as_ref()
                            .and_then(|d| d.explanation.as_deref());
                        match map_anthropic_stop_reason(reason, explanation) {
                            Ok((sr, err_msg)) => {
                                out.stop_reason = sr;
                                if let Some(m) = err_msg {
                                    out.error_message = Some(m);
                                }
                            }
                            Err(e) => {
                                fatal = Some(e.to_string());
                                return Ok(());
                            }
                        }
                    }
                }
                if let Some(u) = &ev.usage {
                    apply_usage(&mut out.usage, u, false);
                    calculate_cost(&model, &mut out.usage);
                }
            }
            "message_stop" => {
                saw_stop = true;
            }
            _ => {}
        }
        Ok(())
    })
    .await;

    drop(event_tx_cb);
    drop(event_tx);
    let _ = forwarder.await;

    if let Err(e) = sse {
        fail(&stream, out, e.to_string(), aborted()).await;
        return;
    }
    if let Some(e) = fatal {
        fail(&stream, out, e, aborted()).await;
        return;
    }

    materialize(&builders, &mut out);

    if saw_start && !saw_stop {
        fail(
            &stream,
            out,
            "Anthropic stream ended before message_stop",
            aborted(),
        )
        .await;
        return;
    }
    if aborted() {
        fail(&stream, out, "Request was aborted", true).await;
        return;
    }
    if out.stop_reason == StopReason::Pending {
        fail(
            &stream,
            out,
            "Anthropic stream ended without a stop reason",
            false,
        )
        .await;
        return;
    }
    if out.stop_reason == StopReason::Aborted || out.stop_reason == StopReason::Error {
        let msg = out
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".into());
        let is_ab = out.stop_reason == StopReason::Aborted;
        fail(&stream, out, msg, is_ab).await;
        return;
    }

    stream
        .push(AssistantMessageEvent::Done {
            reason: out.stop_reason,
            message: out.clone(),
        })
        .await;
    stream.end(Some(out)).await;
}

/// Registers the `anthropic-messages` API provider.
pub fn register_anthropic() {
    let stream_fn: StreamFn = Arc::new(|model, ctx, opts| {
        async move {
            stream_anthropic(
                model,
                ctx,
                AnthropicOptions {
                    stream: opts,
                    ..AnthropicOptions::default()
                },
            )
            .await
        }
        .boxed()
    });
    let simple_fn: SimpleStreamFn = Arc::new(|model, ctx, opts| {
        async move { stream_simple_anthropic(model, ctx, opts).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_ANTHROPIC_MESSAGES.into(),
            stream: stream_fn,
            stream_simple: simple_fn,
            generate_image: None,
        },
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    const ANTHROPIC_SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

    struct Captured {
        path: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    fn spawn_mock(sse: &'static str) -> (String, Arc<Mutex<Option<Captured>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 65536];
            let mut req = Vec::new();
            loop {
                let n = stream.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let text = String::from_utf8_lossy(&req);
                    if let Some(cl) = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    {
                        let len: usize = cl.split(':').nth(1).unwrap().trim().parse().unwrap_or(0);
                        if let Some(pos) = text.find("\r\n\r\n") {
                            let body_start = pos + 4;
                            if req.len().saturating_sub(body_start) >= len {
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&req);
            let mut lines = text.split("\r\n");
            let request_line = lines.next().unwrap_or("");
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let mut headers = HashMap::new();
            for line in lines.by_ref() {
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let body_str = text.split("\r\n\r\n").nth(1).unwrap_or("");
            let body: Value = serde_json::from_str(body_str).unwrap_or(json!({}));
            *cap.lock().expect("lock") = Some(Captured {
                path,
                headers,
                body,
            });

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            stream.write_all(resp.as_bytes()).expect("write");
            let _ = stream.flush();
        });
        (format!("http://{addr}"), captured)
    }
    fn test_model(id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: "anthropic".into(),
            base_url: ANTHROPIC_DEFAULT_BASE_URL.into(),
            max_tokens: 4096,
            ..Model::default()
        }
    }

    fn test_tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: format!("The {name} tool"),
            parameters: Schema::object(
                HashMap::from([("value".into(), Schema::string())]),
                vec!["value".into()],
            ),
            constrained_sampling: None,
        }
    }

    fn deferred_tool_context() -> Context {
        Context {
            messages: vec![
                Message::user_text("start", 1),
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "base_tool".into(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                    api: API_ANTHROPIC_MESSAGES.into(),
                    provider: "anthropic".into(),
                    model: "claude-opus-4-6".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: vec![],
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    tool_name: "base_tool".into(),
                    content: vec![ContentBlock::text("done")],
                    usage: None,
                    details: None,
                    added_tool_names: vec!["late_tool".into()],
                    is_error: false,
                    timestamp: 3,
                }),
                Message::user_text("continue", 4),
            ],
            tools: vec![test_tool("base_tool"), test_tool("late_tool")],
            ..Context::default()
        }
    }

    fn find_tool_result(params: &Value) -> &Value {
        params["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .find(|block| block["type"] == "tool_result")
            .expect("tool result")
    }

    #[test]
    fn anthropic_projects_visible_bash_and_excludes_hidden_bash() {
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
        let params = build_anthropic_params(
            &test_model("claude-opus-4-6"),
            &context,
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");
        let messages = params["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"][0]["text"],
            "Ran `echo ok`\n```\nok\n```"
        );
    }

    #[test]
    fn anthropic_payload_repairs_orphan_tool_calls() {
        let context = Context {
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "lookup".into(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                    api: API_ANTHROPIC_MESSAGES.into(),
                    provider: "anthropic".into(),
                    model: "claude-opus-4-6".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: vec![],
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 1,
                }),
                Message::user_text("continue", 2),
            ],
            ..Context::default()
        };

        let params = build_anthropic_params(
            &test_model("claude-opus-4-6"),
            &context,
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");

        let messages = params["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(messages[1]["content"][0]["is_error"], true);
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn anthropic_payload_normalizes_cross_provider_tool_ids() {
        let original_id = "call|with spaces/and-punctuation";
        let context = Context {
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: original_id.into(),
                        name: "lookup".into(),
                        arguments: json!({}),
                        thought_signature: Some("foreign-signature".into()),
                    })],
                    api: API_OPENAI_RESPONSES.into(),
                    provider: "openai".into(),
                    model: "gpt-5".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: vec![],
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    raw_stop_reason: None,
                    timestamp: 1,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: original_id.into(),
                    tool_name: "lookup".into(),
                    content: vec![ContentBlock::text("done")],
                    usage: None,
                    details: None,
                    added_tool_names: vec![],
                    is_error: false,
                    timestamp: 2,
                }),
            ],
            ..Context::default()
        };

        let params = build_anthropic_params(
            &test_model("claude-opus-4-6"),
            &context,
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");

        let normalized = normalize_tool_call_id(original_id);
        assert_eq!(params["messages"][0]["content"][0]["id"], normalized);
        assert_eq!(
            params["messages"][1]["content"][0]["tool_use_id"],
            normalized
        );
    }

    #[test]
    fn anthropic_payload_splits_and_references_deferred_tools() {
        let params = build_anthropic_params(
            &test_model("claude-opus-4-6"),
            &deferred_tool_context(),
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");

        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "base_tool");
        assert!(tools[0].get("defer_loading").is_none());
        assert_eq!(tools[1]["name"], "late_tool");
        assert_eq!(tools[1]["defer_loading"], true);

        let result = find_tool_result(&params);
        assert_eq!(
            result["content"],
            json!([{"type":"tool_reference","tool_name":"late_tool"}])
        );
        let result_message = params["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| {
                message["content"].as_array().is_some_and(|content| {
                    content.iter().any(|block| block["type"] == "tool_result")
                })
            })
            .expect("tool result message");
        assert!(
            result_message["content"]
                .as_array()
                .expect("content")
                .iter()
                .any(|block| block == &json!({"type":"text","text":"done"}))
        );
    }

    #[test]
    fn anthropic_payload_without_tool_reference_support_is_unchanged() {
        let params = build_anthropic_params(
            &test_model("claude-opus-4-1"),
            &deferred_tool_context(),
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");
        let tools = params["tools"].as_array().expect("tools");

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().expect("tool name"))
                .collect::<Vec<_>>(),
            vec!["base_tool", "late_tool"]
        );
        assert!(tools.iter().all(|tool| tool.get("defer_loading").is_none()));
        assert_eq!(find_tool_result(&params)["content"], "done");
    }

    #[test]
    fn anthropic_strict_sampling_respects_capability_and_requirement() {
        let mut schema = Schema::object(
            HashMap::from([("value".into(), Schema::string())]),
            vec!["value".into()],
        );
        schema.additional_properties = Some(json!(false));
        let tool = |strict| Tool {
            name: "rich".into(),
            description: "rich tool".into(),
            parameters: schema.clone(),
            constrained_sampling: Some(ConstrainedSampling::json_schema(strict)),
        };

        let supported_tool = tool(ConstrainedSamplingStrictness::Prefer);
        let supported =
            convert_anthropic_tools(&[&supported_tool], false, false, true, None, false)
                .expect("strict tool");
        assert_eq!(supported[0]["strict"], true);
        assert_eq!(supported[0]["input_schema"]["additionalProperties"], false);

        let preferred_tool = tool(ConstrainedSamplingStrictness::Prefer);
        let preferred =
            convert_anthropic_tools(&[&preferred_tool], false, false, false, None, false)
                .expect("prefer fallback");
        assert!(preferred[0].get("strict").is_none());
        assert!(
            preferred[0]["input_schema"]
                .get("additionalProperties")
                .is_none()
        );

        let required = tool(ConstrainedSamplingStrictness::Require);
        let error = convert_anthropic_tools(&[&required], false, false, false, None, false)
            .expect_err("require must fail without strict support");
        assert_eq!(
            error.to_string(),
            "Tool \"rich\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    #[test]
    fn anthropic_sensitive_stop_uses_provider_stopped_prefix() {
        assert_eq!(
            map_anthropic_stop_reason("sensitive", None).expect("stop mapping"),
            (
                StopReason::Error,
                Some("Provider stopped with: sensitive".into())
            )
        );
    }

    #[test]
    fn anthropic_headers_are_case_insensitive_single_valued_and_caller_wins() {
        let model = Model {
            provider: "anthropic".into(),
            headers: Some(HashMap::from([
                ("X-API-KEY".into(), "model-key".into()),
                ("Authorization".into(), "Bearer model".into()),
            ])),
            ..Model::default()
        };
        let opts = AnthropicOptions {
            stream: StreamOptions {
                headers: HashMap::from([
                    ("x-api-key".into(), "request-key".into()),
                    ("AUTHORIZATION".into(), "Bearer request".into()),
                ]),
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };
        let headers = anthropic_request_headers(
            &model,
            &Context::default(),
            &opts,
            false,
            "provider-key",
            "",
            false,
        )
        .expect("headers");
        let request = client(&opts.stream)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(request.headers().get_all("x-api-key").iter().count(), 1);
        assert_eq!(
            request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("request-key")
        );
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
    fn anthropic_header_only_auth_skips_default_credential_and_caller_wins() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([("X-API-KEY".into(), "model-secret".into())])),
            ..Model::default()
        };
        let opts = AnthropicOptions {
            stream: StreamOptions {
                headers: HashMap::from([("x-api-key".into(), "caller-secret".into())]),
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };
        validate_anthropic_auth(&model, &opts, "", "").expect("header-owned auth");
        let headers =
            anthropic_request_headers(&model, &Context::default(), &opts, false, "", "", false)
                .expect("headers");
        let request = client(&opts.stream)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(request.headers().get_all("x-api-key").iter().count(), 1);
        assert_eq!(
            request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("caller-secret")
        );
        assert!(!request.headers().contains_key("authorization"));
    }

    #[test]
    fn anthropic_no_key_or_auth_header_returns_sanitized_error() {
        let model = Model {
            provider: "custom".into(),
            headers: Some(HashMap::from([
                ("Authorization".into(), " ".into()),
                ("X-Secret".into(), "must-not-leak".into()),
            ])),
            ..Model::default()
        };
        let error = validate_anthropic_auth(&model, &AnthropicOptions::default(), "", "")
            .expect_err("missing auth");
        assert_eq!(error.to_string(), "No API key for provider: custom");
        assert!(!error.to_string().contains("must-not-leak"));
    }

    #[test]
    fn anthropic_cloudflare_uses_gateway_authorization_only() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let opts = AnthropicOptions::default();
        let headers = anthropic_request_headers(
            &model,
            &Context::default(),
            &opts,
            false,
            "gateway-key",
            "",
            false,
        )
        .expect("headers");
        let request = client(&opts.stream)
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
    fn anthropic_cloudflare_caller_can_override_gateway_authorization() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let opts = AnthropicOptions {
            stream: StreamOptions {
                headers: HashMap::from([("CF-AIG-AUTHORIZATION".into(), "Bearer caller".into())]),
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };
        let headers = anthropic_request_headers(
            &model,
            &Context::default(),
            &opts,
            false,
            "gateway-key",
            "",
            false,
        )
        .expect("headers");
        let request = client(&opts.stream)
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
            Some("Bearer caller")
        );
    }

    #[test]
    fn anthropic_cloudflare_header_only_auth_preserves_caller_value() {
        let model = Model {
            provider: "cloudflare-ai-gateway".into(),
            ..Model::default()
        };
        let opts = AnthropicOptions {
            stream: StreamOptions {
                headers: HashMap::from([("CF-AIG-AUTHORIZATION".into(), "Bearer caller".into())]),
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };
        validate_anthropic_auth(&model, &opts, "", "").expect("header-owned auth");
        let headers =
            anthropic_request_headers(&model, &Context::default(), &opts, false, "", "", false)
                .expect("headers");
        let request = client(&opts.stream)
            .expect("client")
            .get("http://example.test")
            .headers(headers)
            .build()
            .expect("request");
        assert_eq!(
            request
                .headers()
                .get("cf-aig-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer caller")
        );
        assert!(!request.headers().contains_key("authorization"));
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn anthropic_copilot_headers_are_dynamic_and_options_can_override() {
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
        let opts = AnthropicOptions {
            stream: StreamOptions {
                headers: HashMap::from([("x-initiator".into(), "caller".into())]),
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };
        let headers =
            anthropic_request_headers(&model, &context, &opts, false, "copilot-token", "", false)
                .expect("headers");
        let request = client(&opts.stream)
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
            Some("caller")
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

    #[tokio::test]
    async fn anthropic_stream_sends_headers_payload_and_parses_text() {
        let (base, captured) = spawn_mock(ANTHROPIC_SSE);
        let model = Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: "anthropic".into(),
            base_url: base,
            input: vec!["text".into(), "image".into()],
            max_tokens: 4096,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                ..ModelCost::default()
            },
            ..Model::default()
        };
        let req = Context {
            system_prompt: "be helpful".into(),
            messages: vec![Message::user_text("hi", 1)],
            tools: vec![],
        };
        let opts = AnthropicOptions {
            stream: StreamOptions {
                api_key: Some("test-key".into()),
                max_retries: 0,
                ..StreamOptions::default()
            },
            ..AnthropicOptions::default()
        };

        let stream = stream_anthropic(model, req, opts).await;
        let mut saw_text_delta = false;
        let mut final_msg = None;
        while let Some(ev) = stream.next().await {
            match ev {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    if delta.contains("Hello") || delta.contains("world") {
                        saw_text_delta = true;
                    }
                }
                AssistantMessageEvent::Done { message, .. } => {
                    final_msg = Some(message);
                }
                AssistantMessageEvent::Error { error, .. } => {
                    panic!("stream error: {:?}", error.error_message);
                }
                _ => {}
            }
        }
        let final_msg = final_msg.expect("final message");
        assert_eq!(final_msg.stop_reason, StopReason::Stop);
        assert_eq!(final_msg.response_id.as_deref(), Some("msg_1"));
        assert_eq!(final_msg.text(), "Hello world");
        assert!(saw_text_delta, "expected text delta events");
        assert_eq!(final_msg.usage.input, 10);
        assert_eq!(final_msg.usage.output, 5);
        assert!(final_msg.usage.cost.total > 0.0);

        let cap = captured
            .lock()
            .expect("lock")
            .take()
            .expect("captured request");
        assert_eq!(cap.path, "/v1/messages");
        assert_eq!(
            cap.headers.get("x-api-key").map(String::as_str),
            Some("test-key")
        );
        assert_eq!(
            cap.headers.get("anthropic-version").map(String::as_str),
            Some(ANTHROPIC_VERSION)
        );
        assert_eq!(cap.body["model"], json!("claude-test"));
        assert_eq!(cap.body["stream"], json!(true));
        assert_eq!(cap.body["max_tokens"], json!(4096));
        assert!(cap.body.get("system").is_some(), "system prompt missing");
        let msgs = cap.body["messages"].as_array().expect("messages");
        assert!(!msgs.is_empty());
    }

    #[tokio::test]
    async fn anthropic_http_error_fails_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"error":{"message":"bad key"}}"#;
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        let model = Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: "anthropic".into(),
            base_url: format!("http://{addr}"),
            max_tokens: 1024,
            ..Model::default()
        };
        let stream = stream_anthropic(
            model,
            Context {
                messages: vec![Message::user_text("hi", 1)],
                ..Context::default()
            },
            AnthropicOptions {
                stream: StreamOptions {
                    api_key: Some("bad".into()),
                    max_retries: 0,
                    ..StreamOptions::default()
                },
                ..AnthropicOptions::default()
            },
        )
        .await;
        let final_msg = stream.result().await.expect("result");
        assert_eq!(final_msg.stop_reason, StopReason::Error);
        assert!(
            final_msg
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("401"),
            "err={:?}",
            final_msg.error_message
        );
    }

    #[tokio::test]
    async fn register_anthropic_exposes_provider() {
        register_anthropic();
        assert!(get_api_provider(API_ANTHROPIC_MESSAGES).is_some());
    }
}
