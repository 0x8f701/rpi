use super::common;
use crate::*;
use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Default)]
pub struct PiMessagesOptions {
    pub stream: StreamOptions,
    pub reasoning: Option<ThinkingLevel>,
    pub tool_choice: Option<Value>,
    pub debug: bool,
}
impl From<StreamOptions> for PiMessagesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiMessagesRewriteImpact {
    pub policy_id: String,
    pub policy_version: i64,
    pub changed: bool,
    pub token_count_change: i64,
    pub message_count_change: i64,
    pub system_prompt_changed: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        i: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        i: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        i: usize,
        content: String,
        #[serde(default, rename = "contentSignature")]
        signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        i: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        i: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        i: usize,
        content: String,
        #[serde(default, rename = "contentSignature")]
        signature: Option<String>,
        #[serde(default)]
        redacted: bool,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        i: usize,
        id: String,
        #[serde(rename = "toolName")]
        name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        i: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        i: usize,
        #[serde(rename = "toolCall")]
        call: ToolCall,
    },
    Done {
        reason: StopReason,
        usage: Usage,
        #[serde(default, rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<PiMessagesRewriteImpact>,
    },
    Error {
        reason: StopReason,
        usage: Usage,
        #[serde(default, rename = "errorMessage")]
        message: Option<String>,
        #[serde(default, rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<PiMessagesRewriteImpact>,
    },
}
struct State {
    out: AssistantMessage,
    json: HashMap<usize, String>,
    terminal: bool,
}
fn slot(v: &mut Vec<ContentBlock>, i: usize, b: ContentBlock) {
    if v.len() <= i {
        v.resize_with(i + 1, || ContentBlock::text(""));
    }
    v[i] = b
}
fn partial_json(s: &str) -> Value {
    if let Some(v) = parse_json_with_repair(s).filter(Value::is_object) {
        return v;
    }
    let mut x = s.to_string();
    let mut n = 0;
    let (mut q, mut e) = (false, false);
    for b in s.bytes() {
        if q {
            if e {
                e = false
            } else if b == b'\\' {
                e = true
            } else if b == b'"' {
                q = false
            }
        } else if b == b'"' {
            q = true
        } else if b == b'{' {
            n += 1
        } else if b == b'}' {
            n -= 1
        }
    }
    if q {
        x.push('"')
    }
    for _ in 0..n {
        x.push('}')
    }
    parse_json_with_repair(&x)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}
impl State {
    fn new(m: &Model) -> Self {
        Self {
            out: AssistantMessage::pending(m),
            json: HashMap::new(),
            terminal: false,
        }
    }
    fn apply(
        &mut self,
        e: WireEvent,
        tx: &tokio::sync::mpsc::UnboundedSender<AssistantMessageEvent>,
    ) -> Result<()> {
        match e {
            WireEvent::Start => tx.send(AssistantMessageEvent::Start {
                partial: self.out.clone(),
            })?,
            WireEvent::TextStart { i } => {
                slot(&mut self.out.content, i, ContentBlock::text(""));
                tx.send(AssistantMessageEvent::TextStart {
                    content_index: i,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::TextDelta { i, delta } => {
                let Some(ContentBlock::Text { text, .. }) = self.out.content.get_mut(i) else {
                    return Err(anyhow!("text delta without start"));
                };
                text.push_str(&delta);
                tx.send(AssistantMessageEvent::TextDelta {
                    content_index: i,
                    delta,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::TextEnd {
                i,
                content,
                signature,
            } => {
                let Some(ContentBlock::Text {
                    text,
                    text_signature,
                }) = self.out.content.get_mut(i)
                else {
                    return Err(anyhow!("text end without start"));
                };
                *text = content.clone();
                *text_signature = signature;
                tx.send(AssistantMessageEvent::TextEnd {
                    content_index: i,
                    content,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ThinkingStart { i } => {
                slot(&mut self.out.content, i, ContentBlock::thinking(""));
                tx.send(AssistantMessageEvent::ThinkingStart {
                    content_index: i,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ThinkingDelta { i, delta } => {
                let Some(ContentBlock::Thinking { thinking, .. }) = self.out.content.get_mut(i)
                else {
                    return Err(anyhow!("thinking delta without start"));
                };
                thinking.push_str(&delta);
                tx.send(AssistantMessageEvent::ThinkingDelta {
                    content_index: i,
                    delta,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ThinkingEnd {
                i,
                content,
                signature,
                redacted,
            } => {
                let Some(ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    redacted: r,
                }) = self.out.content.get_mut(i)
                else {
                    return Err(anyhow!("thinking end without start"));
                };
                *thinking = content.clone();
                *thinking_signature = signature;
                *r = redacted;
                tx.send(AssistantMessageEvent::ThinkingEnd {
                    content_index: i,
                    content,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ToolCallStart { i, id, name } => {
                slot(
                    &mut self.out.content,
                    i,
                    ContentBlock::ToolCall(ToolCall {
                        id,
                        name,
                        arguments: json!({}),
                        thought_signature: None,
                    }),
                );
                self.json.insert(i, String::new());
                tx.send(AssistantMessageEvent::ToolCallStart {
                    content_index: i,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ToolCallDelta { i, delta } => {
                let s = self.json.entry(i).or_default();
                s.push_str(&delta);
                let Some(ContentBlock::ToolCall(c)) = self.out.content.get_mut(i) else {
                    return Err(anyhow!("tool delta without start"));
                };
                c.arguments = partial_json(s);
                tx.send(AssistantMessageEvent::ToolCallDelta {
                    content_index: i,
                    delta,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::ToolCallEnd { i, call } => {
                slot(
                    &mut self.out.content,
                    i,
                    ContentBlock::ToolCall(call.clone()),
                );
                self.json.remove(&i);
                tx.send(AssistantMessageEvent::ToolCallEnd {
                    content_index: i,
                    tool_call: call,
                    partial: self.out.clone(),
                })?
            }
            WireEvent::Done {
                reason,
                usage,
                response_id,
                rewrite,
            } => {
                if !matches!(
                    reason,
                    StopReason::Stop | StopReason::Length | StopReason::ToolUse
                ) {
                    return Err(anyhow!("invalid done reason"));
                }
                self.terminal = true;
                self.out.stop_reason = reason;
                self.out.usage = usage;
                self.out.response_id = response_id;
                append_rewrite_diagnostic(&mut self.out, rewrite);
            }
            WireEvent::Error {
                reason,
                usage,
                message,
                response_id,
                rewrite,
            } => {
                if !matches!(reason, StopReason::Error | StopReason::Aborted) {
                    return Err(anyhow!("invalid error reason"));
                }
                self.terminal = true;
                self.out.stop_reason = reason;
                self.out.usage = usage;
                self.out.error_message = message;
                self.out.response_id = response_id;
                append_rewrite_diagnostic(&mut self.out, rewrite);
            }
        }
        Ok(())
    }
}
fn append_rewrite_diagnostic(
    message: &mut AssistantMessage,
    rewrite: Option<PiMessagesRewriteImpact>,
) {
    let Some(rewrite) = rewrite else { return; };
    let details = serde_json::to_string(&rewrite).unwrap_or_else(|_| "{}".into());
    message.diagnostics.push(Diagnostic {
        message: format!("pi_messages_rewrite: {details}"),
        code: Some("pi_messages_rewrite".into()),
    });
}
fn level(v: ThinkingLevel) -> &'static str {
    match v {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}
fn payload(m: &Model, c: &Context, o: &PiMessagesOptions) -> Value {
    let mut n = Map::new();
    if let Some(v) = o.stream.temperature {
        n.insert("temperature".into(), json!(v));
    }
    if let Some(v) = o.stream.max_tokens {
        n.insert("maxTokens".into(), json!(v));
    }
    if let Some(v) = o.reasoning {
        n.insert("reasoning".into(), json!(level(v)));
    }
    match o.stream.cache_retention {
        CacheRetention::None => {}
        CacheRetention::Short => {
            n.insert("cacheRetention".into(), json!("short"));
        }
        CacheRetention::Long => {
            n.insert("cacheRetention".into(), json!("long"));
        }
    }
    if let Some(v) = &o.stream.session_id {
        n.insert("sessionId".into(), json!(v));
    }
    if let Some(v) = &o.tool_choice {
        n.insert("toolChoice".into(), v.clone());
    }
    json!({"model":m.id,"context":c,"options":n})
}
fn redact(s: &str, key: &str, model: &Model, options: &StreamOptions) -> String {
    let mut secrets = vec![key.trim().to_owned()];
    if let Some(headers) = &model.headers {
        collect_sensitive_header_values(headers, &mut secrets);
    }
    collect_sensitive_header_values(&options.headers, &mut secrets);
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets.into_iter().fold(s.to_owned(), |sanitized, secret| {
        if secret.is_empty() {
            sanitized
        } else {
            let sanitized = sanitized.replace(&format!("Bearer {secret}"), "Bearer [REDACTED]");
            sanitized.replace(&secret, "[REDACTED]")
        }
    })
}
fn request_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    common::insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    common::insert_header(&mut headers, "accept", "text/event-stream")?;
    common::insert_header(&mut headers, "content-type", "application/json")?;
    if let Some(model_headers) = &model.headers {
        common::insert_header_map(&mut headers, model_headers)?;
    }
    common::insert_header_map(&mut headers, &options.headers)?;
    Ok(headers)
}
fn collect_sensitive_header_values(headers: &HashMap<String, String>, secrets: &mut Vec<String>) {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("cf-aig-authorization")
        {
            let value = value.trim();
            if !value.is_empty() {
                secrets.push(value.to_owned());
                if let Some(token) = value.strip_prefix("Bearer ").map(str::trim).filter(|token| !token.is_empty()) {
                    secrets.push(token.to_owned());
                }
            }
        }
    }
}

pub fn stream_pi_messages(
    model: Model,
    context: Context,
    options: PiMessagesOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let sink = stream.clone();
    tokio::spawn(async move {
        let Some(key) = options
            .stream
            .api_key
            .as_deref()
            .filter(|v| !v.trim().is_empty())
        else {
            common::fail(
                &sink,
                AssistantMessage::pending(&model),
                format!("No API key provided for provider \"{}\"", model.provider),
                false,
            )
            .await;
            return;
        };
        let base = match resolve_base_url(model.base_url.trim(), &options.stream.env) {
            Ok(v) => v,
            Err(e) => {
                common::fail(
                    &sink,
                    AssistantMessage::pending(&model),
                    e.to_string(),
                    false,
                )
                .await;
                return;
            }
        };
        let url = format!(
            "{}/messages{}",
            base.trim_end_matches('/'),
            if options.debug { "?debug=1" } else { "" }
        );
        let body = match common::apply_provider_request(
            payload(&model, &context, &options),
            &model,
            &options.stream,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                common::fail(
                    &sink,
                    AssistantMessage::pending(&model),
                    e.to_string(),
                    false,
                )
                .await;
                return;
            }
        };
        let headers = match request_headers(&model, &options.stream, key) {
            Ok(headers) => match common::apply_provider_headers(headers, &model, &options.stream).await {
                Ok(headers) => headers,
                Err(error) => {
                    common::fail(
                        &sink,
                        AssistantMessage::pending(&model),
                        error.to_string(),
                        false,
                    )
                    .await;
                    return;
                }
            },
            Err(error) => {
                common::fail(
                    &sink,
                    AssistantMessage::pending(&model),
                    error.to_string(),
                    false,
                )
                .await;
                return;
            }
        };
        let client = match common::client(&options.stream) {
            Ok(v) => v,
            Err(e) => {
                common::fail(
                    &sink,
                    AssistantMessage::pending(&model),
                    e.to_string(),
                    false,
                )
                .await;
                return;
            }
        };
        let response = match common::send_with_retry(&options.stream, || {
            client.post(&url).headers(headers.clone()).json(&body)
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                common::fail(
                    &sink,
                    AssistantMessage::pending(&model),
                    redact(&e.to_string(), key, &model, &options.stream),
                    common::is_aborted(&options.stream),
                )
                .await;
                return;
            }
        };
        if let Err(e) = common::notify_response(&options.stream, &response, &model).await {
            common::fail(
                &sink,
                AssistantMessage::pending(&model),
                e.to_string(),
                false,
            )
            .await;
            return;
        }
        if !response.status().is_success() {
            let msg = common::error_body("pi-messages", response, &options.stream)
                .await
                .unwrap_or_else(|e| e.to_string());
            common::fail(
                &sink,
                AssistantMessage::pending(&model),
                redact(&msg, key, &model, &options.stream),
                common::is_aborted(&options.stream),
            )
            .await;
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let drain = sink.clone();
        let drainer = tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                drain.push(e).await;
            }
        });
        let mut state = State::new(&model);
        let result = common::consume_sse(response, &options.stream, |_, data| {
            if data == "[DONE]" {
                return Ok(());
            }
            state.apply(
                serde_json::from_str(data)
                    .map_err(|e| anyhow!("Invalid pi-messages event: {e}"))?,
                &tx,
            )
        })
        .await;
        drop(tx);
        let _ = drainer.await;
        if let Err(e) = result {
            common::fail(
                &sink,
                state.out,
                redact(&e.to_string(), key, &model, &options.stream),
                common::is_aborted(&options.stream),
            )
            .await;
            return;
        }
        if !state.terminal {
            common::fail(
                &sink,
                state.out,
                "pi-messages stream ended without a terminal event",
                false,
            )
            .await;
            return;
        }
        let out = state.out;
        if matches!(out.stop_reason, StopReason::Error | StopReason::Aborted) {
            sink.push(AssistantMessageEvent::Error {
                reason: out.stop_reason,
                error: out.clone(),
            })
            .await
        } else {
            sink.push(AssistantMessageEvent::Done {
                reason: out.stop_reason,
                message: out.clone(),
            })
            .await
        }
        sink.end(Some(out)).await;
    });
    stream
}
pub fn stream_simple_pi_messages(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let reasoning = options.reasoning;
    let mut stream = options.stream;
    stream.max_tokens = Some(clamp_max_tokens_to_context(
        &model,
        &context,
        stream.max_tokens.unwrap_or(model.max_tokens),
    ));
    stream_pi_messages(
        model,
        context,
        PiMessagesOptions {
            stream,
            reasoning,
            tool_choice: None,
            debug: false,
        },
    )
}
pub fn register_pi_messages() {
    register_api_provider(
        ApiProvider {
            api: API_PI_MESSAGES.into(),
            stream: Arc::new(|m, c, o| async move { stream_pi_messages(m, c, o.into()) }.boxed()),
            stream_simple: Arc::new(|m, c, o| {
                async move { stream_simple_pi_messages(m, c, o) }.boxed()
            }),
            generate_image: None,
        },
        None,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_tool_json() {
        assert_eq!(partial_json("{\"q\":\"rad"), json!({"q":"rad"}))
    }
}
