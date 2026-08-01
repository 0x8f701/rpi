use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use chrono::Utc;
use futures_util::{FutureExt, StreamExt};
use reqwest::{
    Response,
    header::{HeaderMap, HeaderValue},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    API_BEDROCK_CONVERSE_STREAM, ApiProvider, AssistantMessage, AssistantMessageEvent,
    AssistantMessageEventStream, CacheRetention, ConstrainedSampling,
    ConstrainedSamplingStrictness, ContentBlock, Context, Message, Model, SimpleStreamFn,
    SimpleStreamOptions, StopReason, StreamFn, StreamOptions, ThinkingBudgets, ThinkingLevel,
    ToolCall, ToolDefinition, calculate_cost, clamp_max_tokens_to_context, clamp_thinking_level,
    new_assistant_message_event_stream, parse_json_with_repair, register_api_provider,
    transform_messages,
};

use super::common::{
    apply_provider_headers, apply_provider_request, client, error_body, fail, insert_header,
    insert_header_map, is_aborted, notify_response, send_with_retry,
};

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const DEFAULT_REGION: &str = "us-east-1";
const BEDROCK_CONTENT_TYPE: &str = "application/json";
const BEDROCK_EVENT_STREAM: &str = "application/vnd.amazon.eventstream";
const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const MAX_EVENT_STREAM_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct BedrockOptions {
    pub stream: StreamOptions,
    pub region: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    pub thinking_display: Option<String>,
    pub request_metadata: Option<HashMap<String, String>>,
    pub bearer_token: Option<String>,
}

impl Default for BedrockOptions {
    fn default() -> Self {
        Self {
            stream: StreamOptions::default(),
            region: None,
            tool_choice: None,
            reasoning: None,
            thinking_budgets: None,
            interleaved_thinking: None,
            thinking_display: None,
            request_metadata: None,
            bearer_token: None,
        }
    }
}

impl From<StreamOptions> for BedrockOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

#[derive(Clone)]
enum BedrockAuth {
    Bearer(String),
    SigV4(AwsCredentials),
    Skip,
}

#[derive(Clone, Debug)]
struct ResolvedEndpoint {
    base_url: String,
    region: String,
}

#[derive(Clone)]
struct PreparedRequest {
    url: String,
    body: String,
    headers: HeaderMap,
}

#[derive(Clone, Debug, Default)]
struct EventFrame {
    headers: HashMap<String, HeaderValueWire>,
    payload: Vec<u8>,
}

#[derive(Clone, Debug)]
enum HeaderValueWire {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    Binary(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

#[derive(Clone, Debug, Default)]
struct EventStreamDecoder {
    pending: Vec<u8>,
}

impl EventStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventFrame>> {
        self.pending.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.pending.len() < 4 {
                break;
            }
            let total_len =
                u32::from_be_bytes(self.pending[..4].try_into().expect("four bytes")) as usize;
            if total_len < 16 {
                bail!("Bedrock event-stream frame is shorter than the 16-byte envelope");
            }
            if total_len > MAX_EVENT_STREAM_MESSAGE_BYTES {
                bail!("Bedrock event-stream frame exceeds the 64 MiB safety limit");
            }
            if self.pending.len() < total_len {
                break;
            }
            let frame = self.pending.drain(..total_len).collect::<Vec<_>>();
            frames.push(decode_event_frame(&frame)?);
        }
        Ok(frames)
    }

    fn finish(self) -> Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            bail!("Truncated Bedrock event-stream frame")
        }
    }
}

#[derive(Clone, Debug)]
struct BlockBuilder {
    wire_index: i64,
    kind: BlockKind,
}

#[derive(Clone, Debug)]
enum BlockKind {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
        redacted: bool,
    },
    Tool {
        id: String,
        name: String,
        partial_json: String,
        arguments: Value,
    },
}

impl BlockBuilder {
    fn content(&self) -> ContentBlock {
        match &self.kind {
            BlockKind::Text { text } => ContentBlock::text(text.clone()),
            BlockKind::Thinking {
                thinking,
                signature,
                redacted,
            } => ContentBlock::Thinking {
                thinking: thinking.clone(),
                thinking_signature: (!signature.is_empty()).then(|| signature.clone()),
                redacted: *redacted,
            },
            BlockKind::Tool {
                id,
                name,
                arguments,
                ..
            } => ContentBlock::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                thought_signature: None,
            }),
        }
    }
}

fn env_value<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn header_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn option_or_model_header<'a>(
    model: &'a Model,
    options: &'a StreamOptions,
    name: &str,
) -> Option<&'a str> {
    header_value(&options.headers, name).or_else(|| {
        model
            .headers
            .as_ref()
            .and_then(|headers| header_value(headers, name))
    })
}

fn get_configured_region(model: &Model, options: &BedrockOptions) -> String {
    arn_region(&model.id)
        .or_else(|| options.region.clone())
        .or_else(|| env_value(&options.stream.env, "AWS_REGION").map(str::to_owned))
        .or_else(|| env_value(&options.stream.env, "AWS_DEFAULT_REGION").map(str::to_owned))
        .or_else(|| standard_endpoint_region(&model.base_url))
        .unwrap_or_else(|| DEFAULT_REGION.to_owned())
}

fn arn_region(model_id: &str) -> Option<String> {
    let mut parts = model_id.split(':');
    let arn = parts.next()?;
    let partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    if arn == "arn" && partition.starts_with("aws") && service == "bedrock" && !region.is_empty() {
        Some(region.to_owned())
    } else {
        None
    }
}

fn standard_endpoint_region(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host
        .strip_prefix("bedrock-runtime-fips.")
        .or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = host
        .strip_suffix(".amazonaws.com.cn")
        .or_else(|| host.strip_suffix(".amazonaws.com"))?;
    (!region.is_empty()
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    .then(|| region.to_owned())
}

fn resolve_endpoint(model: &Model, options: &BedrockOptions) -> Result<ResolvedEndpoint> {
    let region = get_configured_region(model, options);
    let configured_region = options.region.is_some()
        || env_value(&options.stream.env, "AWS_REGION").is_some()
        || env_value(&options.stream.env, "AWS_DEFAULT_REGION").is_some()
        || arn_region(&model.id).is_some();
    let standard_region = standard_endpoint_region(&model.base_url);
    let base_url = if model.base_url.trim().is_empty() {
        standard_base_url(&region)
    } else if standard_region.is_some_and(|endpoint_region| endpoint_region != region)
        && configured_region
    {
        standard_base_url(&region)
    } else {
        model.base_url.trim_end_matches('/').to_owned()
    };
    let url = reqwest::Url::parse(&base_url)
        .map_err(|error| anyhow!("Invalid Bedrock base URL: {error}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("Bedrock base URL must use http or https");
    }
    Ok(ResolvedEndpoint { base_url, region })
}

fn standard_base_url(region: &str) -> String {
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://bedrock-runtime.{region}.{suffix}")
}

fn resolve_auth(model: &Model, options: &BedrockOptions) -> Result<BedrockAuth> {
    let skip_auth = env_value(&options.stream.env, "AWS_BEDROCK_SKIP_AUTH") == Some("1");
    if skip_auth {
        return Ok(BedrockAuth::Skip);
    }

    let explicit_authorization = header_value(&options.stream.headers, "authorization");
    let bearer = options
        .bearer_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            options
                .stream
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "<authenticated>")
        })
        .or_else(|| env_value(&options.stream.env, "AWS_BEARER_TOKEN_BEDROCK"))
        .or_else(|| {
            explicit_authorization.and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
                    .then(|| token.trim())
            })
        });
    if let Some(token) = bearer {
        return Ok(BedrockAuth::Bearer(token.to_owned()));
    }

    let access_key_id = env_value(&options.stream.env, "AWS_ACCESS_KEY_ID");
    let secret_access_key = env_value(&options.stream.env, "AWS_SECRET_ACCESS_KEY");
    match (access_key_id, secret_access_key) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(BedrockAuth::SigV4(AwsCredentials {
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: env_value(&options.stream.env, "AWS_SESSION_TOKEN").map(str::to_owned),
        })),
        (Some(_), None) | (None, Some(_)) => {
            bail!("Amazon Bedrock requires both AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY")
        }
        (None, None) => {
            bail!("Amazon Bedrock requires an explicit bearer token or AWS access-key credentials")
        }
    }
}

fn encode_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn double_encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn request_url(endpoint: &ResolvedEndpoint, model_id: &str) -> String {
    format!(
        "{}/model/{}/converse-stream",
        endpoint.base_url,
        encode_path_segment(model_id)
    )
}

fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization" || lower == "host" || lower.starts_with("x-amz-")
}

fn merged_custom_headers(model: &Model, options: &StreamOptions) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            if !is_reserved_header(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }
    for (name, value) in &options.headers {
        if !is_reserved_header(name) {
            if let Some(existing) = headers
                .keys()
                .find(|existing| existing.eq_ignore_ascii_case(name))
                .cloned()
            {
                headers.remove(&existing);
            }
            headers.insert(name.clone(), value.clone());
        }
    }
    headers
}

fn normalized_header_value(value: &HeaderValue) -> Result<String> {
    Ok(value
        .to_str()
        .map_err(|error| anyhow!("Invalid Bedrock signing header value: {error}"))?
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

fn canonical_headers(headers: &HeaderMap) -> Result<(String, String)> {
    let mut values = Vec::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "cache-control"
                | "connection"
                | "expect"
                | "from"
                | "keep-alive"
                | "max-forwards"
                | "pragma"
                | "referer"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "user-agent"
                | "x-amzn-trace-id"
        ) || lower.starts_with("proxy-")
            || lower.starts_with("sec-")
        {
            continue;
        }
        values.push((lower, normalized_header_value(value)?));
    }
    values.sort_by(|left, right| left.0.cmp(&right.0));
    let signed_headers = values
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = values
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    Ok((canonical, signed_headers))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sign_sigv4(
    method: &str,
    url: &str,
    body: &str,
    headers: &mut HeaderMap,
    credentials: &AwsCredentials,
    region: &str,
    amz_date: &str,
) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Bedrock request URL has no host"))?;
    let host = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    insert_header(headers, "host", &host)?;
    insert_header(headers, "x-amz-date", amz_date)?;
    if let Some(token) = &credentials.session_token {
        insert_header(headers, "x-amz-security-token", token)?;
    }
    let payload_hash = hex(Sha256::digest(body.as_bytes()));
    insert_header(headers, "x-amz-content-sha256", &payload_hash)?;

    let (canonical_headers, signed_headers) = canonical_headers(headers)?;
    let canonical_path = double_encode_path(parsed.path());
    let canonical_request = format!(
        "{method}\n{canonical_path}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        parsed.query().unwrap_or("")
    );
    let short_date = amz_date
        .get(..8)
        .ok_or_else(|| anyhow!("Invalid AWS signing timestamp"))?;
    let scope = format!("{short_date}/{region}/bedrock/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(Sha256::digest(canonical_request.as_bytes()))
    );
    let date_key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        short_date.as_bytes(),
    );
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, b"bedrock");
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    let signature = hex(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    insert_header(headers, "authorization", &authorization)?;
    Ok(())
}

fn prepare_request(
    model: &Model,
    endpoint: &ResolvedEndpoint,
    body: &Value,
    auth: &BedrockAuth,
    options: &BedrockOptions,
    signing_time: chrono::DateTime<Utc>,
) -> Result<PreparedRequest> {
    let url = request_url(endpoint, &model.id);
    let body = serde_json::to_string(body)?;
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "content-type", BEDROCK_CONTENT_TYPE)?;
    insert_header(&mut headers, "accept", BEDROCK_EVENT_STREAM)?;
    insert_header_map(&mut headers, &merged_custom_headers(model, &options.stream))?;
    match auth {
        BedrockAuth::Bearer(token) => {
            insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
        }
        BedrockAuth::SigV4(credentials) => {
            sign_sigv4(
                "POST",
                &url,
                &body,
                &mut headers,
                credentials,
                &endpoint.region,
                &signing_time.format("%Y%m%dT%H%M%SZ").to_string(),
            )?;
        }
        BedrockAuth::Skip => {}
    }
    Ok(PreparedRequest { url, body, headers })
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn non_blank_text(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"text": text}))
}

fn required_text(text: &str) -> Value {
    non_blank_text(text).unwrap_or_else(|| json!({"text": EMPTY_TEXT_PLACEHOLDER}))
}

fn image_block(mime_type: &str, data: &str) -> Result<Value> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => bail!("Unknown image type: {mime_type}"),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| anyhow!("Invalid base64 image data: {error}"))?;
    Ok(json!({
        "image": {
            "format": format,
            "source": {"bytes": base64::engine::general_purpose::STANDARD.encode(bytes)},
        }
    }))
}

fn is_anthropic_claude(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    let name = model.name.to_ascii_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn model_match_candidates(model: &Model) -> Vec<String> {
    [&model.id, &model.name]
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_ascii_lowercase();
            let normalized = lower
                .chars()
                .map(|character| {
                    if matches!(character, ' ' | '_' | '.' | ':') {
                        '-'
                    } else {
                        character
                    }
                })
                .collect();
            [lower, normalized]
        })
        .collect()
}

fn supports_adaptive_thinking(model: &Model) -> bool {
    model_match_candidates(model).iter().any(|candidate| {
        candidate.contains("opus-4-6")
            || candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-4-6")
            || candidate.contains("sonnet-5")
            || candidate.contains("fable-5")
    })
}

fn supports_native_xhigh(model: &Model) -> bool {
    model_match_candidates(model).iter().any(|candidate| {
        candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("fable-5")
    })
}

fn supports_prompt_caching(model: &Model, env: &HashMap<String, String>) -> bool {
    let candidates = model_match_candidates(model);
    if !candidates
        .iter()
        .any(|candidate| candidate.contains("claude"))
    {
        return env_value(env, "AWS_BEDROCK_FORCE_CACHE") == Some("1");
    }
    candidates.iter().any(|candidate| {
        candidate.contains("fable-5")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("-4-")
            || candidate.contains("claude-3-7-sonnet")
            || candidate.contains("claude-3-5-haiku")
    })
}

fn cache_point(retention: CacheRetention) -> Value {
    let mut point = Map::from_iter([("type".into(), json!("default"))]);
    if retention == CacheRetention::Long {
        point.insert("ttl".into(), json!("1h"));
    }
    json!({"cachePoint": point})
}

fn convert_tool_result_content(content: &[ContentBlock]) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                if let Some(text) = non_blank_text(text) {
                    result.push(text);
                }
            }
            ContentBlock::Image { data, mime_type } => result.push(image_block(mime_type, data)?),
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) => {}
        }
    }
    if result.is_empty() {
        result.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    Ok(result)
}

fn convert_messages(
    context: &Context,
    model: &Model,
    retention: CacheRetention,
    env: &HashMap<String, String>,
) -> Result<Vec<Value>> {
    let transformed = transform_messages(&context.messages, model, |id, _, _| {
        normalize_tool_call_id(id)
    });
    let mut result = Vec::with_capacity(transformed.len());
    let mut index = 0;
    while index < transformed.len() {
        match &transformed[index] {
            Message::User(message) => {
                let mut content = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if let Some(text) = non_blank_text(text) {
                                content.push(text);
                            }
                        }
                        ContentBlock::Image { data, mime_type } => {
                            content.push(image_block(mime_type, data)?);
                        }
                        ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) => {}
                    }
                }
                if content.is_empty() {
                    content.push(required_text(""));
                }
                result.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(message) => {
                let mut content = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if let Some(text) = non_blank_text(text) {
                                content.push(text);
                            }
                        }
                        ContentBlock::ToolCall(call) => content.push(json!({
                            "toolUse": {
                                "toolUseId": call.id,
                                "name": call.name,
                                "input": call.arguments,
                            }
                        })),
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted {
                                if is_anthropic_claude(model) {
                                    if let Some(signature) = thinking_signature
                                        .as_deref()
                                        .filter(|value| !value.trim().is_empty())
                                    {
                                        content.push(json!({
                                            "reasoningContent": {
                                                "redactedContent": signature
                                            }
                                        }));
                                    }
                                }
                            } else if !thinking.trim().is_empty() {
                                if is_anthropic_claude(model) {
                                    if let Some(signature) = thinking_signature
                                        .as_deref()
                                        .filter(|value| !value.trim().is_empty())
                                    {
                                        content.push(json!({
                                            "reasoningContent": {
                                                "reasoningText": {"text": thinking, "signature": signature}
                                            }
                                        }));
                                    } else {
                                        content.push(json!({"text": thinking}));
                                    }
                                } else {
                                    content.push(json!({
                                        "reasoningContent": {"reasoningText": {"text": thinking}}
                                    }));
                                }
                            }
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !content.is_empty() {
                    result.push(json!({"role":"assistant","content":content}));
                }
            }
            Message::ToolResult(message) => {
                let mut content = Vec::new();
                let mut next = index;
                while let Some(Message::ToolResult(tool_result)) = transformed.get(next) {
                    content.push(json!({
                        "toolResult": {
                            "toolUseId": tool_result.tool_call_id,
                            "content": convert_tool_result_content(&tool_result.content)?,
                            "status": if tool_result.is_error { "error" } else { "success" },
                        }
                    }));
                    next += 1;
                }
                result.push(json!({"role":"user","content":content}));
                index = next - 1;
            }
            Message::BashExecution(message) => {
                result.push(json!({
                    "role":"user",
                    "content":[{"text": format!("{}\n{}", message.command, message.output)}]
                }));
            }
            Message::Custom(_) | Message::BranchSummary(_) | Message::CompactionSummary(_) => {
                unreachable!("session-only messages must be removed before provider conversion")
            }
        }
        index += 1;
    }
    if retention != CacheRetention::None
        && supports_prompt_caching(model, env)
        && let Some(last) = result.last_mut()
        && last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(cache_point(retention));
    }
    Ok(result)
}

fn build_system_prompt(
    system_prompt: &str,
    model: &Model,
    retention: CacheRetention,
    env: &HashMap<String, String>,
) -> Option<Value> {
    if system_prompt.is_empty() {
        return None;
    }
    let mut blocks = vec![json!({"text": system_prompt})];
    if retention != CacheRetention::None && supports_prompt_caching(model, env) {
        blocks.push(cache_point(retention));
    }
    Some(Value::Array(blocks))
}

fn supports_strict_tools(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("supportsStrictMode"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn strict_tool(tool: &ToolDefinition, supported: bool) -> Result<bool> {
    let strictness = match &tool.constrained_sampling {
        Some(ConstrainedSampling::JsonSchema { strict }) => *strict,
        _ => return Ok(false),
    };
    if supported {
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

fn convert_tool_config(
    tools: &[ToolDefinition],
    tool_choice: Option<&Value>,
    supports_strict: bool,
) -> Result<Option<Value>> {
    if tools.is_empty() || tool_choice.is_some_and(|choice| choice.as_str() == Some("none")) {
        return Ok(None);
    }
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = strict_tool(tool, supports_strict)?;
        let mut specification = Map::from_iter([
            ("name".into(), json!(tool.name)),
            ("description".into(), json!(tool.description)),
            ("inputSchema".into(), json!({"json": tool.parameters})),
        ]);
        if strict {
            specification.insert("strict".into(), Value::Bool(true));
        }
        converted.push(json!({"toolSpec": specification}));
    }
    let choice = match tool_choice {
        Some(Value::String(value)) if value == "auto" => Some(json!({"auto":{}})),
        Some(Value::String(value)) if value == "any" => Some(json!({"any":{}})),
        Some(Value::Object(value)) if value.get("type").and_then(Value::as_str) == Some("tool") => {
            value
                .get("name")
                .and_then(Value::as_str)
                .map(|name| json!({"tool":{"name":name}}))
        }
        _ => None,
    };
    let mut config = Map::from_iter([("tools".into(), Value::Array(converted))]);
    if let Some(choice) = choice {
        config.insert("toolChoice".into(), choice);
    }
    Ok(Some(Value::Object(config)))
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

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::XHigh && supports_native_xhigh(model) {
        return "xhigh".into();
    }
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

fn reasoning_budget(level: ThinkingLevel, budgets: Option<&ThinkingBudgets>) -> i64 {
    match level {
        ThinkingLevel::Minimal => budgets.and_then(|budgets| budgets.minimal).unwrap_or(1024),
        ThinkingLevel::Low => budgets.and_then(|budgets| budgets.low).unwrap_or(2048),
        ThinkingLevel::Medium => budgets.and_then(|budgets| budgets.medium).unwrap_or(8192),
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => {
            budgets.and_then(|budgets| budgets.high).unwrap_or(16384)
        }
    }
}

fn is_govcloud(model: &Model, endpoint: &ResolvedEndpoint) -> bool {
    endpoint.region.to_ascii_lowercase().starts_with("us-gov-")
        || model.id.to_ascii_lowercase().starts_with("us-gov.")
        || model.id.to_ascii_lowercase().starts_with("arn:aws-us-gov:")
}

fn additional_model_request_fields(
    model: &Model,
    endpoint: &ResolvedEndpoint,
    options: &BedrockOptions,
) -> Option<Value> {
    let level = options.reasoning.filter(|_| model.reasoning)?;
    if !is_anthropic_claude(model) {
        return None;
    }
    let display = (!is_govcloud(model, endpoint)).then(|| {
        options
            .thinking_display
            .clone()
            .unwrap_or_else(|| "summarized".into())
    });
    if supports_adaptive_thinking(model) {
        let mut thinking = Map::from_iter([("type".into(), json!("adaptive"))]);
        if let Some(display) = display {
            thinking.insert("display".into(), json!(display));
        }
        return Some(json!({
            "thinking": thinking,
            "output_config": {"effort": map_thinking_level_to_effort(model, level)},
        }));
    }
    let mut thinking = Map::from_iter([
        ("type".into(), json!("enabled")),
        (
            "budget_tokens".into(),
            json!(reasoning_budget(level, options.thinking_budgets.as_ref())),
        ),
    ]);
    if let Some(display) = display {
        thinking.insert("display".into(), json!(display));
    }
    let mut fields = Map::from_iter([("thinking".into(), Value::Object(thinking))]);
    if options.interleaved_thinking.unwrap_or(true) {
        fields.insert(
            "anthropic_beta".into(),
            json!(["interleaved-thinking-2025-05-14"]),
        );
    }
    Some(Value::Object(fields))
}

fn build_payload(
    model: &Model,
    context: &Context,
    endpoint: &ResolvedEndpoint,
    options: &BedrockOptions,
) -> Result<Value> {
    let retention = options.stream.cache_retention;
    let mut payload = Map::from_iter([
        (
            "messages".into(),
            Value::Array(convert_messages(
                context,
                model,
                retention,
                &options.stream.env,
            )?),
        ),
        ("inferenceConfig".into(), Value::Object(Map::new())),
    ]);
    let inference = payload
        .get_mut("inferenceConfig")
        .and_then(Value::as_object_mut)
        .expect("inference config object");
    let max_tokens = options
        .stream
        .max_tokens
        .or_else(|| is_anthropic_claude(model).then_some(model.max_tokens));
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.stream.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    if let Some(system) = build_system_prompt(
        &context.system_prompt,
        model,
        retention,
        &options.stream.env,
    ) {
        payload.insert("system".into(), system);
    }
    if let Some(tool_config) = convert_tool_config(
        &context.tools,
        options.tool_choice.as_ref(),
        supports_strict_tools(model),
    )? {
        payload.insert("toolConfig".into(), tool_config);
    }
    if let Some(fields) = additional_model_request_fields(model, endpoint, options) {
        payload.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        payload.insert("requestMetadata".into(), serde_json::to_value(metadata)?);
    }
    Ok(Value::Object(payload))
}

fn parse_streaming_json(partial: &str) -> Value {
    if partial.trim().is_empty() {
        return json!({});
    }
    if let Some(value) = parse_json_with_repair(partial).filter(Value::is_object) {
        return value;
    }
    let mut completed = partial.to_owned();
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for byte in partial.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => stack.push(b'}'),
                b'[' => stack.push(b']'),
                b'}' | b']' => {
                    if stack.last() == Some(&byte) {
                        stack.pop();
                    }
                }
                _ => {}
            }
        }
    }
    if escaped {
        completed.push('\\');
    }
    if in_string {
        completed.push('"');
    }
    while let Some(closer) = stack.pop() {
        completed.push(closer as char);
    }
    parse_json_with_repair(&completed)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn materialize(builders: &[BlockBuilder], output: &mut AssistantMessage) {
    output.content = builders.iter().map(BlockBuilder::content).collect();
}

fn find_builder(builders: &[BlockBuilder], wire_index: i64) -> Option<usize> {
    builders
        .iter()
        .position(|builder| builder.wire_index == wire_index)
}

async fn handle_bedrock_event(
    event_type: &str,
    value: &Value,
    builders: &mut Vec<BlockBuilder>,
    output: &mut AssistantMessage,
    model: &Model,
    stream: &AssistantMessageEventStream,
) -> Result<()> {
    match event_type {
        "messageStart" => {
            if value.get("role").and_then(Value::as_str) != Some("assistant") {
                bail!("Unexpected Bedrock message start role");
            }
            stream
                .push(AssistantMessageEvent::Start {
                    partial: output.clone(),
                })
                .await;
        }
        "contentBlockStart" => {
            let wire_index = value
                .get("contentBlockIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if let Some(tool) = value.pointer("/start/toolUse") {
                builders.push(BlockBuilder {
                    wire_index,
                    kind: BlockKind::Tool {
                        id: tool
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: tool
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        partial_json: String::new(),
                        arguments: json!({}),
                    },
                });
                materialize(builders, output);
                stream
                    .push(AssistantMessageEvent::ToolCallStart {
                        content_index: builders.len() - 1,
                        partial: output.clone(),
                    })
                    .await;
            }
        }
        "contentBlockDelta" => {
            let wire_index = value
                .get("contentBlockIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let delta = value.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                let index = find_builder(builders, wire_index).unwrap_or_else(|| {
                    builders.push(BlockBuilder {
                        wire_index,
                        kind: BlockKind::Text {
                            text: String::new(),
                        },
                    });
                    builders.len() - 1
                });
                let was_empty =
                    matches!(&builders[index].kind, BlockKind::Text { text } if text.is_empty());
                if was_empty {
                    materialize(builders, output);
                    stream
                        .push(AssistantMessageEvent::TextStart {
                            content_index: index,
                            partial: output.clone(),
                        })
                        .await;
                }
                if let BlockKind::Text { text: current } = &mut builders[index].kind {
                    current.push_str(text);
                }
                materialize(builders, output);
                stream
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: text.to_owned(),
                        partial: output.clone(),
                    })
                    .await;
            } else if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
                if let Some(index) = find_builder(builders, wire_index) {
                    if let BlockKind::Tool {
                        partial_json,
                        arguments,
                        ..
                    } = &mut builders[index].kind
                    {
                        partial_json.push_str(input);
                        *arguments = parse_streaming_json(partial_json);
                    }
                    materialize(builders, output);
                    stream
                        .push(AssistantMessageEvent::ToolCallDelta {
                            content_index: index,
                            delta: input.to_owned(),
                            partial: output.clone(),
                        })
                        .await;
                }
            } else if let Some(reasoning) = delta.get("reasoningContent") {
                let index = find_builder(builders, wire_index).unwrap_or_else(|| {
                    builders.push(BlockBuilder {
                        wire_index,
                        kind: BlockKind::Thinking {
                            thinking: String::new(),
                            signature: String::new(),
                            redacted: false,
                        },
                    });
                    builders.len() - 1
                });
                let was_empty = matches!(&builders[index].kind, BlockKind::Thinking { thinking, signature, .. } if thinking.is_empty() && signature.is_empty());
                if was_empty {
                    materialize(builders, output);
                    stream
                        .push(AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                            partial: output.clone(),
                        })
                        .await;
                }
                let mut emitted = None;
                if let BlockKind::Thinking {
                    thinking,
                    signature,
                    redacted,
                } = &mut builders[index].kind
                {
                    if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                        thinking.push_str(text);
                        emitted = Some(text.to_owned());
                    }
                    if let Some(value) = reasoning.get("signature").and_then(Value::as_str) {
                        signature.push_str(value);
                    }
                    if let Some(value) = reasoning.get("redactedContent").and_then(Value::as_str) {
                        *redacted = true;
                        signature.push_str(value);
                    }
                }
                materialize(builders, output);
                if let Some(delta) = emitted {
                    stream
                        .push(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta,
                            partial: output.clone(),
                        })
                        .await;
                }
            }
        }
        "contentBlockStop" => {
            let wire_index = value
                .get("contentBlockIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let Some(index) = find_builder(builders, wire_index) else {
                return Ok(());
            };
            if let BlockKind::Tool {
                partial_json,
                arguments,
                ..
            } = &mut builders[index].kind
            {
                *arguments = parse_streaming_json(partial_json);
            }
            materialize(builders, output);
            match &builders[index].kind {
                BlockKind::Text { text } => {
                    stream
                        .push(AssistantMessageEvent::TextEnd {
                            content_index: index,
                            content: text.clone(),
                            partial: output.clone(),
                        })
                        .await;
                }
                BlockKind::Thinking { thinking, .. } => {
                    stream
                        .push(AssistantMessageEvent::ThinkingEnd {
                            content_index: index,
                            content: thinking.clone(),
                            partial: output.clone(),
                        })
                        .await;
                }
                BlockKind::Tool { .. } => {
                    let ContentBlock::ToolCall(tool_call) = builders[index].content() else {
                        unreachable!();
                    };
                    stream
                        .push(AssistantMessageEvent::ToolCallEnd {
                            content_index: index,
                            tool_call,
                            partial: output.clone(),
                        })
                        .await;
                }
            }
        }
        "messageStop" => {
            let reason = value.get("stopReason").and_then(Value::as_str);
            let (stop_reason, error) = map_stop_reason(reason);
            output.stop_reason = stop_reason;
            output.raw_stop_reason = reason.map(str::to_owned);
            if let Some(error) = error {
                output.error_message = Some(error);
            }
        }
        "metadata" => {
            if let Some(usage) = value.get("usage") {
                output.usage.input = usage
                    .get("inputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                output.usage.output = usage
                    .get("outputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                output.usage.cache_read = usage
                    .get("cacheReadInputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                output.usage.cache_write = usage
                    .get("cacheWriteInputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                output.usage.total_tokens = usage
                    .get("totalTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(output.usage.input + output.usage.output);
                calculate_cost(model, &mut output.usage);
            }
        }
        "internalServerException"
        | "modelStreamErrorException"
        | "validationException"
        | "throttlingException"
        | "serviceUnavailableException" => {
            bail!(format_bedrock_exception(event_type, value));
        }
        _ => {}
    }
    Ok(())
}

fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(reason) => (StopReason::Error, Some(reason.to_owned())),
        None => (StopReason::Error, None),
    }
}

fn format_bedrock_exception(event_type: &str, value: &Value) -> String {
    let prefix = match event_type {
        "internalServerException" => "Internal server error",
        "modelStreamErrorException" => "Model stream error",
        "validationException" => "Validation error",
        "throttlingException" => "Throttling error",
        "serviceUnavailableException" => "Service unavailable",
        _ => event_type,
    };
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(event_type);
    let hint = if message.to_ascii_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    format!("{prefix}: {message}{hint}")
}

fn frame_header_string<'a>(frame: &'a EventFrame, name: &str) -> Option<&'a str> {
    match frame.headers.get(name) {
        Some(HeaderValueWire::String(value)) => Some(value),
        _ => None,
    }
}

fn decode_event_frame(frame: &[u8]) -> Result<EventFrame> {
    if frame.len() < 16 {
        bail!("Bedrock event-stream frame is too short");
    }
    let total_len = u32::from_be_bytes(frame[..4].try_into().expect("four bytes")) as usize;
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().expect("four bytes")) as usize;
    if total_len != frame.len() || 12 + headers_len + 4 > frame.len() {
        bail!("Bedrock event-stream frame length is invalid");
    }
    let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().expect("four bytes"));
    if crc32(&frame[..8]) != prelude_crc {
        bail!("Bedrock event-stream prelude checksum mismatch");
    }
    let message_crc = u32::from_be_bytes(frame[frame.len() - 4..].try_into().expect("four bytes"));
    if crc32(&frame[..frame.len() - 4]) != message_crc {
        bail!("Bedrock event-stream message checksum mismatch");
    }
    let headers = decode_frame_headers(&frame[12..12 + headers_len])?;
    let payload = frame[12 + headers_len..frame.len() - 4].to_vec();
    Ok(EventFrame { headers, payload })
}

fn read_exact<'a>(bytes: &'a [u8], position: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = position
        .checked_add(length)
        .ok_or_else(|| anyhow!("Bedrock event-stream header length overflow"))?;
    let value = bytes
        .get(*position..end)
        .ok_or_else(|| anyhow!("Truncated Bedrock event-stream header"))?;
    *position = end;
    Ok(value)
}

fn decode_frame_headers(bytes: &[u8]) -> Result<HashMap<String, HeaderValueWire>> {
    let mut headers = HashMap::new();
    let mut position = 0;
    while position < bytes.len() {
        let name_length = *read_exact(bytes, &mut position, 1)?
            .first()
            .expect("one byte") as usize;
        let name = String::from_utf8(read_exact(bytes, &mut position, name_length)?.to_vec())?;
        let tag = *read_exact(bytes, &mut position, 1)?
            .first()
            .expect("one byte");
        let value = match tag {
            0 => HeaderValueWire::Bool(true),
            1 => HeaderValueWire::Bool(false),
            2 => HeaderValueWire::Byte(read_exact(bytes, &mut position, 1)?[0] as i8),
            3 => HeaderValueWire::Short(i16::from_be_bytes(
                read_exact(bytes, &mut position, 2)?
                    .try_into()
                    .expect("two bytes"),
            )),
            4 => HeaderValueWire::Integer(i32::from_be_bytes(
                read_exact(bytes, &mut position, 4)?
                    .try_into()
                    .expect("four bytes"),
            )),
            5 => HeaderValueWire::Long(i64::from_be_bytes(
                read_exact(bytes, &mut position, 8)?
                    .try_into()
                    .expect("eight bytes"),
            )),
            6 | 7 => {
                let length = u16::from_be_bytes(
                    read_exact(bytes, &mut position, 2)?
                        .try_into()
                        .expect("two bytes"),
                ) as usize;
                let value = read_exact(bytes, &mut position, length)?.to_vec();
                if tag == 6 {
                    HeaderValueWire::Binary(value)
                } else {
                    HeaderValueWire::String(String::from_utf8(value)?)
                }
            }
            8 => HeaderValueWire::Timestamp(i64::from_be_bytes(
                read_exact(bytes, &mut position, 8)?
                    .try_into()
                    .expect("eight bytes"),
            )),
            9 => HeaderValueWire::Uuid(
                read_exact(bytes, &mut position, 16)?
                    .try_into()
                    .expect("sixteen bytes"),
            ),
            _ => bail!("Unrecognized Bedrock event-stream header type {tag}"),
        };
        headers.insert(name, value);
    }
    Ok(headers)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

async fn consume_event_stream(
    response: Response,
    options: &StreamOptions,
    model: &Model,
    stream: &AssistantMessageEventStream,
    output: &mut AssistantMessage,
) -> Result<()> {
    let mut bytes = response.bytes_stream();
    let mut decoder = EventStreamDecoder::default();
    let mut builders = Vec::new();
    loop {
        let chunk = match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
                chunk = bytes.next() => chunk,
            },
            None => bytes.next().await,
        };
        let Some(chunk) = chunk else {
            break;
        };
        for frame in decoder.push(&chunk?)? {
            let message_type = frame_header_string(&frame, ":message-type").unwrap_or("event");
            match message_type {
                "error" => {
                    let code = frame_header_string(&frame, ":error-code").unwrap_or("UnknownError");
                    let message =
                        frame_header_string(&frame, ":error-message").unwrap_or("Unknown error");
                    bail!("{code}: {message}");
                }
                "exception" => {
                    let event_type = frame_header_string(&frame, ":exception-type")
                        .or_else(|| frame_header_string(&frame, ":event-type"))
                        .unwrap_or("unknownException");
                    let value: Value = serde_json::from_slice(&frame.payload).unwrap_or_else(
                        |_| json!({"message": String::from_utf8_lossy(&frame.payload)}),
                    );
                    bail!(format_bedrock_exception(event_type, &value));
                }
                "event" => {
                    let Some(event_type) = frame_header_string(&frame, ":event-type") else {
                        continue;
                    };
                    let value: Value = if frame.payload.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_slice(&frame.payload).map_err(|error| {
                            anyhow!("Invalid Bedrock {event_type} event JSON: {error}")
                        })?
                    };
                    handle_bedrock_event(event_type, &value, &mut builders, output, model, stream)
                        .await?;
                }
                other => bail!("Unrecognized Bedrock event-stream message type {other}"),
            }
        }
    }
    decoder.finish()?;
    materialize(&builders, output);
    Ok(())
}

fn redact_bedrock_secrets(message: &str, model: &Model, options: &BedrockOptions) -> String {
    let mut secrets = Vec::new();
    let mut add = |value: Option<&str>| {
        if let Some(value) = value
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "<authenticated>")
        {
            secrets.push(value.to_owned());
        }
    };
    add(options.bearer_token.as_deref());
    add(options.stream.api_key.as_deref());
    for name in [
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        add(env_value(&options.stream.env, name));
    }
    add(header_value(&options.stream.headers, "authorization"));
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets
        .into_iter()
        .fold(message.to_owned(), |redacted, secret| {
            redacted.replace(&secret, "[REDACTED]")
        })
}

fn format_stream_error(error: anyhow::Error) -> String {
    let message = error.to_string();
    if message.to_ascii_lowercase().contains("data retention mode")
        && !message.contains(BEDROCK_DATA_RETENTION_DOCS_URL)
    {
        format!(
            "{message} See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes."
        )
    } else {
        message
    }
}

pub async fn stream_bedrock(
    model: Model,
    context: Context,
    options: BedrockOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let stream_task = stream.clone();
    tokio::spawn(async move {
        run_bedrock_stream(stream_task, model, context, options).await;
    });
    stream
}

async fn run_bedrock_stream(
    stream: AssistantMessageEventStream,
    model: Model,
    context: Context,
    options: BedrockOptions,
) {
    let mut output = AssistantMessage::pending(&model);
    let endpoint = match resolve_endpoint(&model, &options) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&error.to_string(), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    let auth = match resolve_auth(&model, &options) {
        Ok(auth) => auth,
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&error.to_string(), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    let payload = match build_payload(&model, &context, &endpoint, &options) {
        Ok(payload) => match apply_provider_request(payload, &model, &options.stream).await {
            Ok(payload) => payload,
            Err(error) => {
                fail(
                    &stream,
                    output,
                    redact_bedrock_secrets(&error.to_string(), &model, &options),
                    is_aborted(&options.stream),
                )
                .await;
                return;
            }
        },
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&error.to_string(), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    let request = match prepare_request(&model, &endpoint, &payload, &auth, &options, Utc::now()) {
        Ok(mut request) => match apply_provider_headers(request.headers, &model, &options.stream).await {
            Ok(headers) => {
                request.headers = headers;
                request
            }
            Err(error) => {
                fail(
                    &stream,
                    output,
                    redact_bedrock_secrets(&error.to_string(), &model, &options),
                    is_aborted(&options.stream),
                )
                .await;
                return;
            }
        },
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&error.to_string(), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    let http = match client(&options.stream) {
        Ok(http) => http,
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&error.to_string(), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    let response = match send_with_retry(&options.stream, || {
        http.post(&request.url)
            .headers(request.headers.clone())
            .body(request.body.clone())
    })
    .await
    {
        Ok(response) => response,
        Err(error) => {
            fail(
                &stream,
                output,
                redact_bedrock_secrets(&format_stream_error(error), &model, &options),
                is_aborted(&options.stream),
            )
            .await;
            return;
        }
    };
    if let Err(error) = notify_response(&options.stream, &response, &model).await {
        fail(
            &stream,
            output,
            redact_bedrock_secrets(&error.to_string(), &model, &options),
            is_aborted(&options.stream),
        )
        .await;
        return;
    }
    if !response.status().is_success() {
        let message = error_body("Bedrock", response, &options.stream)
            .await
            .unwrap_or_else(|error| error.to_string());
        fail(
            &stream,
            output,
            redact_bedrock_secrets(&message, &model, &options),
            is_aborted(&options.stream),
        )
        .await;
        return;
    }
    if let Err(error) =
        consume_event_stream(response, &options.stream, &model, &stream, &mut output).await
    {
        fail(
            &stream,
            output,
            redact_bedrock_secrets(&format_stream_error(error), &model, &options),
            is_aborted(&options.stream),
        )
        .await;
        return;
    }
    if is_aborted(&options.stream) {
        fail(&stream, output, "Request was aborted", true).await;
        return;
    }
    if output.stop_reason == StopReason::Pending {
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
        let message = output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".into());
        fail(&stream, output, message, false).await;
        return;
    }
    calculate_cost(&model, &mut output.usage);
    stream
        .push(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output.clone(),
        })
        .await;
    stream.end(Some(output)).await;
}

fn clamp_bedrock_reasoning(model: &Model, level: ThinkingLevel) -> Option<ThinkingLevel> {
    match clamp_thinking_level(model, thinking_level_name(level)) {
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn adjusted_thinking_tokens(
    requested_max: i64,
    model_max: i64,
    level: ThinkingLevel,
    budgets: Option<&ThinkingBudgets>,
) -> (i64, i64) {
    let budget = reasoning_budget(level, budgets).max(0);
    let output = requested_max.max(budget + 1024).min(model_max.max(1));
    (output, budget)
}

pub async fn stream_simple_bedrock(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let requested_max = options.stream.max_tokens.unwrap_or(model.max_tokens);
    let mut native = BedrockOptions {
        stream: options.stream,
        thinking_budgets: options.thinking_budgets.clone(),
        ..BedrockOptions::default()
    };
    native.stream.max_tokens = Some(clamp_max_tokens_to_context(&model, &context, requested_max));
    let Some(reasoning) = options
        .reasoning
        .and_then(|level| clamp_bedrock_reasoning(&model, level))
    else {
        return stream_bedrock(model, context, native).await;
    };
    native.reasoning = Some(reasoning);
    if is_anthropic_claude(&model) && !supports_adaptive_thinking(&model) {
        let (adjusted_max, budget) = adjusted_thinking_tokens(
            requested_max,
            model.max_tokens,
            reasoning,
            options.thinking_budgets.as_ref(),
        );
        let max_tokens = clamp_max_tokens_to_context(&model, &context, adjusted_max);
        native.stream.max_tokens = Some(max_tokens);
        let mut budgets = native.thinking_budgets.take().unwrap_or_default();
        let budget = budget.min((max_tokens - 1024).max(0));
        match reasoning {
            ThinkingLevel::Minimal => budgets.minimal = Some(budget),
            ThinkingLevel::Low => budgets.low = Some(budget),
            ThinkingLevel::Medium => budgets.medium = Some(budget),
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => {
                budgets.high = Some(budget);
            }
        }
        native.thinking_budgets = Some(budgets);
    }
    stream_bedrock(model, context, native).await
}

pub fn register_bedrock() {
    let stream: StreamFn = Arc::new(|model, context, options| {
        async move { stream_bedrock(model, context, options.into()).await }.boxed()
    });
    let stream_simple: SimpleStreamFn = Arc::new(|model, context, options| {
        async move { stream_simple_bedrock(model, context, options).await }.boxed()
    });
    register_api_provider(
        ApiProvider {
            api: API_BEDROCK_CONVERSE_STREAM.into(),
            stream,
            stream_simple,
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
        thread,
    };
    use tokio_util::sync::CancellationToken;

    fn model(base_url: String) -> Model {
        Model {
            id: "anthropic.claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            api: API_BEDROCK_CONVERSE_STREAM.into(),
            provider: "amazon-bedrock".into(),
            base_url,
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            max_tokens: 64_000,
            ..Model::default()
        }
    }

    fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(7);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn event_frame(event_type: &str, payload: Value) -> Vec<u8> {
        let mut headers = string_header(":message-type", "event");
        headers.extend(string_header(":event-type", event_type));
        headers.extend(string_header(":content-type", "application/json"));
        let payload = serde_json::to_vec(&payload).expect("serialize payload");
        let total = 16 + headers.len() + payload.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&crc32(&out).to_be_bytes());
        out.extend(headers);
        out.extend(payload);
        let checksum = crc32(&out);
        out.extend_from_slice(&checksum.to_be_bytes());
        out
    }

    fn fixture_stream() -> Vec<u8> {
        let frames = [
            event_frame("messageStart", json!({"role":"assistant"})),
            event_frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think","signature":"sig"}}}),
            ),
            event_frame("contentBlockStop", json!({"contentBlockIndex":0})),
            event_frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":1,"delta":{"text":"hello"}}),
            ),
            event_frame("contentBlockStop", json!({"contentBlockIndex":1})),
            event_frame(
                "contentBlockStart",
                json!({"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"call-1","name":"read"}}}),
            ),
            event_frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"path\":\"x\"}"}}}),
            ),
            event_frame("contentBlockStop", json!({"contentBlockIndex":2})),
            event_frame("messageStop", json!({"stopReason":"tool_use"})),
            event_frame(
                "metadata",
                json!({"usage":{"inputTokens":12,"outputTokens":8,"cacheReadInputTokens":3,"cacheWriteInputTokens":2,"totalTokens":20}}),
            ),
        ];
        frames.concat()
    }

    fn spawn_server(response_body: Vec<u8>) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("local address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let mut content_length = 0usize;
            let mut headers_end = None;
            loop {
                let count = socket.read(&mut buffer).expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if headers_end.is_none() {
                    headers_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4);
                    if let Some(end) = headers_end {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':').and_then(|(name, value)| {
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                            })
                            .unwrap_or(0);
                    }
                }
                if headers_end.is_some_and(|end| request.len() >= end + content_length) {
                    break;
                }
            }
            let _ = request_tx.send(request);
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {BEDROCK_EVENT_STREAM}\r\ncontent-length: {}\r\nx-amzn-requestid: fixture\r\n\r\n",
                response_body.len()
            );
            socket.write_all(headers.as_bytes()).expect("write headers");
            socket.write_all(&response_body).expect("write body");
        });
        (format!("http://{address}"), request_rx)
    }

    fn explicit_credentials(options: &mut StreamOptions) {
        options
            .env
            .insert("AWS_ACCESS_KEY_ID".into(), "AKIDEXAMPLE".into());
        options
            .env
            .insert("AWS_SECRET_ACCESS_KEY".into(), "secret".into());
        options.env.insert("AWS_REGION".into(), "us-east-1".into());
    }

    #[test]
    fn resolves_region_and_endpoint_without_home_credentials() {
        let mut options = BedrockOptions::default();
        options
            .stream
            .env
            .insert("AWS_REGION".into(), "eu-west-1".into());
        let endpoint = resolve_endpoint(
            &model("https://bedrock-runtime.us-east-1.amazonaws.com".into()),
            &options,
        )
        .expect("resolve endpoint");
        assert_eq!(endpoint.region, "eu-west-1");
        assert_eq!(
            endpoint.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );

        let arn_model = Model {
            id: "arn:aws-us-gov:bedrock:us-gov-west-1:123:inference-profile/test".into(),
            base_url: String::new(),
            ..model(String::new())
        };
        assert_eq!(
            resolve_endpoint(&arn_model, &BedrockOptions::default())
                .expect("ARN endpoint")
                .region,
            "us-gov-west-1"
        );
    }

    #[test]
    fn sigv4_matches_aws_documentation_fixture() {
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let mut headers = HeaderMap::new();
        insert_header(
            &mut headers,
            "content-type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .unwrap();
        sign_sigv4(
            "POST",
            "https://iam.amazonaws.com/",
            "Action=ListUsers&Version=2010-05-08",
            &mut headers,
            &credentials,
            "us-east-1",
            "20150830T123600Z",
        )
        .expect("sign fixture");
        let authorization = headers["authorization"].to_str().unwrap();
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"
        ));
        assert!(
            authorization
                .contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date")
        );
        assert!(authorization.ends_with(
            "Signature=291943665513055348280c1560ef7172ad1653cf60fef35db72ea52a78939a7c"
        ));
        assert_eq!(
            headers["x-amz-content-sha256"],
            "b6359072c78d70ebee1e81adcbab4f01bf2c23245fa365ef83fe8f1f955085e2"
        );
    }

    #[test]
    fn bearer_auth_wins_and_reserved_headers_are_ignored() {
        let mut native = BedrockOptions::default();
        native.stream.api_key = Some("bearer-secret".into());
        native
            .stream
            .headers
            .insert("Authorization".into(), "evil".into());
        native
            .stream
            .headers
            .insert("X-Amz-Date".into(), "evil".into());
        native.stream.headers.insert("X-Custom".into(), "ok".into());
        let model = model("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        let endpoint = resolve_endpoint(&model, &native).unwrap();
        let auth = resolve_auth(&model, &native).unwrap();
        let request =
            prepare_request(&model, &endpoint, &json!({}), &auth, &native, Utc::now()).unwrap();
        assert_eq!(request.headers["authorization"], "Bearer bearer-secret");
        assert_eq!(request.headers["x-custom"], "ok");
        assert!(request.headers.get("x-amz-date").is_none());
    }

    #[test]
    fn auth_inputs_are_case_insensitive_and_explicit_only() {
        let model = model("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        let mut bearer = BedrockOptions::default();
        bearer
            .stream
            .headers
            .insert("aUtHoRiZaTiOn".into(), "bEaReR header-token".into());
        assert!(matches!(
            resolve_auth(&model, &bearer).expect("mixed-case bearer"),
            BedrockAuth::Bearer(token) if token == "header-token"
        ));

        let mut sigv4 = BedrockOptions::default();
        sigv4
            .stream
            .env
            .insert("aws_access_key_id".into(), "AKID".into());
        sigv4
            .stream
            .env
            .insert("Aws_Secret_Access_Key".into(), "secret".into());
        sigv4
            .stream
            .env
            .insert("aws_session_token".into(), "session".into());
        assert!(matches!(
            resolve_auth(&model, &sigv4).expect("mixed-case SigV4 env"),
            BedrockAuth::SigV4(AwsCredentials { access_key_id, session_token: Some(token), .. })
                if access_key_id == "AKID" && token == "session"
        ));

        let model_with_auth_header = Model {
            headers: Some(HashMap::from([(
                "Authorization".into(),
                "Bearer model-secret".into(),
            )])),
            ..model
        };
        assert!(resolve_auth(&model_with_auth_header, &BedrockOptions::default()).is_err());
    }

    #[test]
    fn payload_translates_images_tools_thinking_and_cache() {
        let model = model("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        let endpoint = resolve_endpoint(&model, &BedrockOptions::default()).unwrap();
        let context = Context {
            system_prompt: "system".into(),
            messages: vec![
                Message::User(crate::UserMessage {
                    content: vec![
                        ContentBlock::text("look"),
                        ContentBlock::Image {
                            data: base64::engine::general_purpose::STANDARD.encode(b"img"),
                            mime_type: "image/png".into(),
                        },
                    ],
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::Thinking {
                        thinking: "reason".into(),
                        thinking_signature: Some("signature".into()),
                        redacted: false,
                    }],
                    ..AssistantMessage::pending(&model)
                }),
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read".into(),
                parameters: crate::Schema::object(HashMap::new(), vec![]),
                constrained_sampling: None,
            }],
        };
        let options = BedrockOptions {
            reasoning: Some(ThinkingLevel::High),
            stream: StreamOptions {
                cache_retention: CacheRetention::Long,
                ..StreamOptions::default()
            },
            ..BedrockOptions::default()
        };
        let payload = build_payload(&model, &context, &endpoint, &options).expect("payload");
        assert_eq!(
            payload.pointer("/system/1/cachePoint/ttl"),
            Some(&json!("1h"))
        );
        assert_eq!(
            payload.pointer("/messages/0/content/1/image/format"),
            Some(&json!("png"))
        );
        assert_eq!(
            payload.pointer("/messages/1/content/0/reasoningContent/reasoningText/signature"),
            Some(&json!("signature"))
        );
        assert_eq!(
            payload.pointer("/toolConfig/tools/0/toolSpec/name"),
            Some(&json!("read"))
        );
        assert_eq!(
            payload.pointer("/additionalModelRequestFields/thinking/type"),
            Some(&json!("adaptive"))
        );
    }

    #[test]
    fn event_stream_decoder_handles_fragmented_frames_and_checks_crc() {
        let frame = event_frame("messageStart", json!({"role":"assistant"}));
        let mut decoder = EventStreamDecoder::default();
        assert!(decoder.push(&frame[..7]).unwrap().is_empty());
        let decoded = decoder.push(&frame[7..]).expect("decode remainder");
        assert_eq!(decoded.len(), 1);
        assert_eq!(
            frame_header_string(&decoded[0], ":event-type"),
            Some("messageStart")
        );
        decoder.finish().unwrap();

        let mut corrupt = frame;
        corrupt[12] ^= 1;
        assert!(
            decode_event_frame(&corrupt)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
    }

    #[tokio::test]
    async fn fixture_stream_emits_unified_lifecycle_and_usage() {
        let (base_url, request_rx) = spawn_server(fixture_stream());
        let mut native = BedrockOptions::default();
        explicit_credentials(&mut native.stream);
        let stream = stream_bedrock(model(base_url), Context::default(), native).await;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        let output = stream.result().await.expect("result");
        let request_bytes = request_rx.recv().expect("request");
        let request = String::from_utf8_lossy(&request_bytes);
        assert!(
            request.contains("POST /model/anthropic.claude-sonnet-4-6/converse-stream HTTP/1.1")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: aws4-hmac-sha256")
        );
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("request body");
        let body: Value = serde_json::from_str(body).expect("Bedrock request JSON");
        assert_eq!(body.pointer("/inferenceConfig/maxTokens"), Some(&json!(64000)));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Start { .. }))
        );
        assert!(events.iter().any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "think")));
        assert!(events.iter().any(|event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "hello")));
        assert!(events.iter().any(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { tool_call, .. } if tool_call.arguments == json!({"path":"x"}))));
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(output.usage.input, 12);
        assert_eq!(output.usage.output, 8);
        assert_eq!(output.usage.cache_read, 3);
        assert_eq!(output.usage.cache_write, 2);
    }

    #[tokio::test]
    async fn stream_simple_catalog_model_smoke() {
        crate::providers::register_builtins();
        assert!(crate::get_api_provider(API_BEDROCK_CONVERSE_STREAM).is_some());
        let (base_url, _) = spawn_server(fixture_stream());
        let mut catalog_model =
            crate::builtin_model("amazon-bedrock", "us.anthropic.claude-sonnet-4-6")
                .expect("catalog Bedrock model");
        catalog_model.base_url = base_url;
        let mut options = SimpleStreamOptions::default();
        explicit_credentials(&mut options.stream);
        options.reasoning = Some(ThinkingLevel::High);
        let stream = stream_simple_bedrock(catalog_model, Context::default(), options).await;
        let output = stream.result().await.expect("catalog model result");
        assert_eq!(output.api, API_BEDROCK_CONVERSE_STREAM);
        assert_eq!(output.provider, "amazon-bedrock");
        assert_eq!(output.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn retries_retryable_response_then_streams_successfully() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("local address");
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = socket.read(&mut buffer).expect("read request");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests_tx.send(request).expect("record request");
                if attempt == 0 {
                    socket
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after-ms: 0\r\n\r\n")
                        .expect("write retry response");
                } else {
                    let body = fixture_stream();
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {BEDROCK_EVENT_STREAM}\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(headers.as_bytes()).expect("write headers");
                    socket.write_all(&body).expect("write body");
                }
            }
        });

        let mut native = BedrockOptions::default();
        explicit_credentials(&mut native.stream);
        native.stream.max_retries = 1;
        let stream = stream_bedrock(
            model(format!("http://{address}")),
            Context::default(),
            native,
        )
        .await;
        let output = stream.result().await.expect("retry result");
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        let first_request = requests_rx.recv().expect("first request");
        let second_request = requests_rx.recv().expect("second request");
        let first = String::from_utf8_lossy(&first_request);
        let second = String::from_utf8_lossy(&second_request);
        assert!(first.to_ascii_lowercase().contains("authorization: aws4-hmac-sha256"));
        assert!(second.to_ascii_lowercase().contains("authorization: aws4-hmac-sha256"));
    }

    #[tokio::test]
    async fn provider_error_redacts_explicit_bearer_token() {
        let secret = "never-print-bedrock-token";
        let body = format!(
            r#"{{"error":{{"message":"bad token {secret}","code":"AccessDeniedException"}}}}"#
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            socket.write_all(format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(), body
            ).as_bytes()).unwrap();
        });
        let mut native = BedrockOptions::default();
        native.stream.api_key = Some(secret.into());
        let stream = stream_bedrock(
            model(format!("http://{address}")),
            Context::default(),
            native,
        )
        .await;
        let output = stream.result().await.expect("terminal error");
        let message = output.error_message.expect("error message");
        assert_eq!(output.stop_reason, StopReason::Error);
        assert!(!message.contains(secret), "secret leaked: {message}");
        assert!(message.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn cancellation_during_event_body_is_aborted_and_secret_is_redacted() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: {BEDROCK_EVENT_STREAM}\r\ntransfer-encoding: chunked\r\n\r\n").as_bytes()).unwrap();
            socket.write_all(b"1\r\n0\r\n").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let token = CancellationToken::new();
        let secret = "sensitive-bedrock-token";
        let mut native = BedrockOptions::default();
        native.stream.api_key = Some(secret.into());
        native.stream.abort_signal = Some(token.clone());
        let stream = stream_bedrock(
            model(format!("http://{address}")),
            Context::default(),
            native,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token.cancel();
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .expect("cancellation returns")
            .expect("terminal message");
        assert_eq!(output.stop_reason, StopReason::Aborted);
        assert!(
            !output
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains(secret)
        );
    }
}
