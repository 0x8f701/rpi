use crate::Schema;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

pub type Api = String;
pub type ProviderId = String;
pub type ThinkingLevelMap = HashMap<String, Option<String>>;
pub const API_OPENAI_COMPLETIONS: &str = "openai-completions";
pub const API_OPENAI_RESPONSES: &str = "openai-responses";
pub const API_AZURE_OPENAI_RESPONSES: &str = "azure-openai-responses";
pub const API_OPENAI_CODEX_RESPONSES: &str = "openai-codex-responses";
pub const API_ANTHROPIC_MESSAGES: &str = "anthropic-messages";
pub const API_BEDROCK_CONVERSE_STREAM: &str = "bedrock-converse-stream";
pub const API_GOOGLE_GENERATIVE_AI: &str = "google-generative-ai";
pub const API_GOOGLE_VERTEX: &str = "google-vertex";
pub const API_MISTRAL_CONVERSATIONS: &str = "mistral-conversations";
pub const API_PI_MESSAGES: &str = "pi-messages";
pub const API_FAUX: &str = "faux";
/// All statically-cataloged model APIs. Every entry MUST be registered by
/// [`crate::providers::register_builtins`] and every catalog model MUST use
/// one of these. `pi-messages` is dynamic (Radius) and absent from the embedded
/// catalog until a validated refresh.
pub const KNOWN_CATALOG_APIS: &[&str] = &[
    API_OPENAI_COMPLETIONS,
    API_OPENAI_RESPONSES,
    API_AZURE_OPENAI_RESPONSES,
    API_OPENAI_CODEX_RESPONSES,
    API_ANTHROPIC_MESSAGES,
    API_BEDROCK_CONVERSE_STREAM,
    API_GOOGLE_GENERATIVE_AI,
    API_GOOGLE_VERTEX,
    API_MISTRAL_CONVERSATIONS,
    API_PI_MESSAGES,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Sse,
    WebSocket,
    WebSocketCached,
    Auto,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    User,
    Assistant,
    ToolResult,
    BashExecution,
    Custom,
    BranchSummary,
    CompactionSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(
            default,
            rename = "textSignature",
            alias = "text_signature",
            skip_serializing_if = "Option::is_none"
        )]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        #[serde(
            default,
            rename = "thinkingSignature",
            alias = "thinking_signature",
            skip_serializing_if = "Option::is_none"
        )]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn empty_object() -> Value {
    Value::Object(Default::default())
}
impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_signature: None,
        }
    }
    pub fn thinking(thinking: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: false,
        }
    }
}
pub type Content = ContentBlock;
pub type ContentList = Vec<ContentBlock>;

/// Deserialize `UserMessage.content` from either the canonical content-block
/// array or an upstream plain string (which becomes a single text block).
/// Serialization always emits the canonical array form; this only loosens input.
fn deserialize_user_content<'de, D>(deserializer: D) -> Result<ContentList, D::Error>
where
    D: serde::Deserializer<'de>,
{
    CustomMessageContent::deserialize(deserializer).map(CustomMessageContent::into_blocks)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default = "empty_object")]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(deserialize_with = "deserialize_user_content")]
    pub content: ContentList,
    pub timestamp: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    #[serde(default)]
    pub cache_write_1h: i64,
    #[serde(default)]
    pub reasoning: i64,
    pub total_tokens: i64,
    #[serde(default)]
    pub cost: CostBreakdown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: ContentList,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    pub timestamp: i64,
}
impl AssistantMessage {
    pub fn pending(model: &Model) -> Self {
        Self {
            content: vec![],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            timestamp: now_millis(),
        }
    }
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text { text, .. } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: ContentList,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    #[serde(default)]
    pub is_error: bool,
    pub timestamp: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomMessageContent {
    Text(String),
    Blocks(ContentList),
}

impl CustomMessageContent {
    #[must_use]
    pub fn into_blocks(self) -> ContentList {
        match self {
            Self::Text(text) => vec![ContentBlock::text(text)],
            Self::Blocks(content) => content,
        }
    }

    #[must_use]
    pub fn to_blocks(&self) -> ContentList {
        self.clone().into_blocks()
    }
}

impl From<String> for CustomMessageContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for CustomMessageContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<ContentList> for CustomMessageContent {
    fn from(value: ContentList) -> Self {
        Self::Blocks(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: CustomMessageContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    #[serde(rename = "bashExecution")]
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    #[serde(rename = "branchSummary")]
    BranchSummary(BranchSummaryMessage),
    #[serde(rename = "compactionSummary")]
    CompactionSummary(CompactionSummaryMessage),
}
impl Message {
    pub fn user_text(text: impl Into<String>, timestamp: i64) -> Self {
        Self::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp,
        })
    }
    pub fn role(&self) -> Role {
        match self {
            Self::User(_) => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::ToolResult(_) => Role::ToolResult,
            Self::BashExecution(_) => Role::BashExecution,
            Self::Custom(_) => Role::Custom,
            Self::BranchSummary(_) => Role::BranchSummary,
            Self::CompactionSummary(_) => Role::CompactionSummary,
        }
    }
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        if let Self::Assistant(m) = self {
            Some(m)
        } else {
            None
        }
    }
}
impl From<AssistantMessage> for Message {
    fn from(v: AssistantMessage) -> Self {
        Self::Assistant(v)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub input_tokens_above: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(default)]
    pub tiers: Vec<ModelCostTier>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    pub context_window: i64,
    pub max_tokens: i64,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub compat: Option<Value>,
}
impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            api: String::new(),
            provider: String::new(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            context_window: 128000,
            max_tokens: 16384,
            headers: None,
            compat: None,
        }
    }
}
pub fn calculate_cost(model: &Model, usage: &mut Usage) {
    let n = usage.input + usage.cache_read + usage.cache_write;
    let r = model
        .cost
        .tiers
        .iter()
        .filter(|t| n > t.input_tokens_above)
        .max_by_key(|t| t.input_tokens_above);
    let (i, o, cr, cw) = r
        .map(|t| (t.input, t.output, t.cache_read, t.cache_write))
        .unwrap_or((
            model.cost.input,
            model.cost.output,
            model.cost.cache_read,
            model.cost.cache_write,
        ));
    usage.cost.input = usage.input as f64 * i / 1e6;
    usage.cost.output = usage.output as f64 * o / 1e6;
    usage.cost.cache_read = usage.cache_read as f64 * cr / 1e6;
    usage.cost.cache_write = ((usage.cache_write - usage.cache_write_1h) as f64 * cw
        + usage.cache_write_1h as f64 * i * 2.0)
        / 1e6;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GrammarVariants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_lark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_regex: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConstrainedSamplingStrictness {
    #[default]
    Prefer,
    Require,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstrainedSampling {
    Disabled,
    JsonSchema {
        strict: ConstrainedSamplingStrictness,
    },
    Grammar {
        variants: GrammarVariants,
    },
}

impl Default for ConstrainedSampling {
    fn default() -> Self {
        Self::Disabled
    }
}

impl Serialize for ConstrainedSampling {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::JsonSchema { strict } => serde_json::json!({
                "type": "json_schema",
                "strict": strict,
            })
            .serialize(serializer),
            Self::Grammar { variants } => serde_json::json!({
                "type": "grammar",
                "variants": variants,
            })
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ConstrainedSampling {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value == Value::Bool(false) || value == Value::Null {
            return Ok(Self::Disabled);
        }
        let object = value.as_object().ok_or_else(|| {
            serde::de::Error::custom(
                "constrainedSampling must be false or an object with type json_schema or grammar",
            )
        })?;
        match object.get("type").and_then(Value::as_str) {
            Some("json_schema") => {
                let strict = object
                    .get("strict")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(serde::de::Error::custom)?
                    .unwrap_or_default();
                Ok(Self::JsonSchema { strict })
            }
            Some("grammar") => {
                let variants = object
                    .get("variants")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::missing_field("variants"))
                    .and_then(|variants| {
                        serde_json::from_value(variants).map_err(serde::de::Error::custom)
                    })?;
                Ok(Self::Grammar { variants })
            }
            Some(kind) => Err(serde::de::Error::custom(format!(
                "unknown constrained sampling type {kind:?}, want json_schema or grammar"
            ))),
            None => Err(serde::de::Error::missing_field("type")),
        }
    }
}

impl ConstrainedSampling {
    pub fn json_schema(strict: ConstrainedSamplingStrictness) -> Self {
        Self::JsonSchema { strict }
    }

    pub fn grammar(variants: GrammarVariants) -> Self {
        Self::Grammar { variants }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Schema,
    #[serde(default)]
    pub constrained_sampling: Option<ConstrainedSampling>,
}
pub type Tool = ToolDefinition;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
}
pub type ProviderHeaders = BTreeMap<String, Option<String>>;
pub type ProviderHookFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
pub type PayloadHook = Arc<dyn Fn(Value, &Model) -> Result<Value> + Send + Sync>;
pub type ResponseHook = Arc<dyn Fn(ProviderResponse, &Model) -> Result<()> + Send + Sync>;
pub type BeforeProviderRequestHook =
    Arc<dyn Fn(Value, Model) -> ProviderHookFuture<Value> + Send + Sync>;
pub type BeforeProviderHeadersHook =
    Arc<dyn Fn(ProviderHeaders, Model) -> ProviderHookFuture<ProviderHeaders> + Send + Sync>;
pub type AfterProviderResponseHook =
    Arc<dyn Fn(ProviderResponse, Model) -> ProviderHookFuture<()> + Send + Sync>;
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub api_key: Option<String>,
    pub transport: Transport,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub on_payload: Option<PayloadHook>,
    pub on_response: Option<ResponseHook>,
    pub before_provider_request: Option<BeforeProviderRequestHook>,
    pub before_provider_headers: Option<BeforeProviderHeadersHook>,
    pub after_provider_response: Option<AfterProviderResponseHook>,
    pub headers: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub max_retries: usize,
    pub max_retry_delay_ms: Option<u64>,
    pub metadata: Option<Value>,
    pub env: HashMap<String, String>,
    pub abort_signal: Option<CancellationToken>,
}
impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("transport", &self.transport)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("timeout_ms", &self.timeout_ms)
            .field(
                "websocket_connect_timeout_ms",
                &self.websocket_connect_timeout_ms,
            )
            .field("max_retries", &self.max_retries)
            .finish()
    }
}
#[derive(Debug, Clone, Default)]
pub struct ThinkingBudgets {
    pub minimal: Option<i64>,
    pub low: Option<i64>,
    pub medium: Option<i64>,
    pub high: Option<i64>,
}
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    pub stream: StreamOptions,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}
impl From<StreamOptions> for SimpleStreamOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            reasoning: None,
            thinking_budgets: None,
        }
    }
}
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost_model() -> Model {
        Model {
            id: "cost-test".into(),
            name: "Cost Test".into(),
            api: API_FAUX.into(),
            provider: "test".into(),
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 1.5,
                cache_write: 0.5,
                ..ModelCost::default()
            },
            ..Model::default()
        }
    }

    #[test]
    fn prices_1h_cache_writes_at_2x_input_cost() {
        let model = cost_model();
        let mut usage = Usage {
            input: 1000,
            output: 2000,
            cache_read: 3000,
            cache_write: 4000,
            cache_write_1h: 1000,
            total_tokens: 10000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(usage.cost.input, 0.003);
        assert_eq!(usage.cost.output, 0.03);
        assert_eq!(usage.cost.cache_read, 0.0045);
        assert_eq!(usage.cost.cache_write, 0.0075);
        assert_eq!(usage.cost.total, 0.045);
    }

    #[test]
    fn cost_without_1h_cache_writes_is_unchanged() {
        let model = cost_model();
        let mut usage = Usage {
            input: 1000,
            output: 2000,
            cache_read: 3000,
            cache_write: 4000,
            total_tokens: 10000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(usage.cost.input, 0.003);
        assert_eq!(usage.cost.output, 0.03);
        assert_eq!(usage.cost.cache_read, 0.0045);
        assert_eq!(usage.cost.cache_write, 0.002);
        assert_eq!(usage.cost.total, 0.0395);
    }

    #[test]
    fn tier_input_cost_prices_1h_cache_writes_at_2x() {
        let model = Model {
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 1.5,
                cache_write: 0.5,
                tiers: vec![ModelCostTier {
                    input: 6.0,
                    output: 20.0,
                    cache_read: 2.0,
                    cache_write: 1.0,
                    input_tokens_above: 5000,
                }],
            },
            ..cost_model()
        };
        let mut usage = Usage {
            input: 6000,
            output: 1000,
            cache_read: 1000,
            cache_write: 2000,
            cache_write_1h: 500,
            total_tokens: 10000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(usage.cost.input, 0.036);
        assert_eq!(usage.cost.output, 0.02);
        assert_eq!(usage.cost.cache_read, 0.002);
        assert_eq!(usage.cost.cache_write, 0.0075);
        assert_eq!(usage.cost.total, 0.0655);
    }
    #[test]
    fn bash_execution_message_round_trips_upstream_shape() {
        let message = Message::BashExecution(BashExecutionMessage {
            command: "printf '<ok>'".into(),
            output: "<ok>".into(),
            exit_code: Some(7),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/full.log".into()),
            timestamp: 1234,
            exclude_from_context: Some(true),
        });
        let encoded = serde_json::to_value(&message).expect("serialize bash execution");
        assert_eq!(
            encoded,
            serde_json::json!({
                "role": "bashExecution",
                "command": "printf '<ok>'",
                "output": "<ok>",
                "exitCode": 7,
                "cancelled": false,
                "truncated": true,
                "fullOutputPath": "/tmp/full.log",
                "timestamp": 1234,
                "excludeFromContext": true,
            })
        );
        let decoded: Message = serde_json::from_value(encoded).expect("deserialize bash execution");
        assert_eq!(decoded, message);

        let without_optional: Message = serde_json::from_value(serde_json::json!({
            "role": "bashExecution",
            "command": "pwd",
            "output": "",
            "cancelled": true,
            "truncated": false,
            "timestamp": 9,
        }))
        .expect("deserialize omitted optional fields");
        assert!(matches!(
            without_optional,
            Message::BashExecution(BashExecutionMessage {
                exit_code: None,
                full_output_path: None,
                exclude_from_context: None,
                ..
            })
        ));
    }

    #[test]
    fn extensible_session_messages_round_trip_upstream_shapes() {
        let messages = [
            Message::Custom(CustomMessage {
                custom_type: "example.notice".into(),
                content: CustomMessageContent::Text("hidden reminder".into()),
                display: false,
                details: Some(serde_json::json!({"nested":{"kept":true}})),
                timestamp: 10,
            }),
            Message::BranchSummary(BranchSummaryMessage {
                summary: "alternate work".into(),
                from_id: "entry-1".into(),
                details: Some(serde_json::json!({"readFiles":["src/lib.rs"]})),
                usage: Some(Usage {
                    input: 10,
                    output: 4,
                    total_tokens: 14,
                    ..Usage::default()
                }),
                from_hook: Some(true),
                timestamp: 11,
            }),
            Message::CompactionSummary(CompactionSummaryMessage {
                summary: "prior work".into(),
                tokens_before: 12_345,
                details: Some(serde_json::json!({"modifiedFiles":["src/main.rs"]})),
                usage: Some(Usage {
                    input: 20,
                    output: 5,
                    total_tokens: 25,
                    ..Usage::default()
                }),
                from_hook: Some(false),
                timestamp: 12,
            }),
        ];
        let expected = [
            serde_json::json!({
                "role":"custom", "customType":"example.notice", "content":"hidden reminder",
                "display":false, "details":{"nested":{"kept":true}}, "timestamp":10
            }),
            serde_json::json!({
                "role":"branchSummary", "summary":"alternate work", "fromId":"entry-1",
                "details":{"readFiles":["src/lib.rs"]},
                "usage":{"input":10,"output":4,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,"totalTokens":14,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
                "fromHook":true, "timestamp":11
            }),
            serde_json::json!({
                "role":"compactionSummary", "summary":"prior work", "tokensBefore":12345,
                "details":{"modifiedFiles":["src/main.rs"]},
                "usage":{"input":20,"output":5,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,"totalTokens":25,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
                "fromHook":false, "timestamp":12
            }),
        ];
        for (message, expected) in messages.into_iter().zip(expected) {
            let encoded = serde_json::to_value(&message).expect("serialize extensible message");
            assert_eq!(encoded, expected);
            let decoded: Message =
                serde_json::from_value(encoded).expect("deserialize extensible message");
            assert_eq!(decoded, message);
        }

        let legacy_branch: Message = serde_json::from_value(serde_json::json!({
            "role":"branchSummary", "summary":"legacy", "fromId":"root", "timestamp":1
        }))
        .expect("deserialize legacy branch summary");
        assert!(matches!(legacy_branch, Message::BranchSummary(BranchSummaryMessage {
            details: None, usage: None, from_hook: None, ..
        })));
        let legacy_compaction: Message = serde_json::from_value(serde_json::json!({
            "role":"compactionSummary", "summary":"legacy", "tokensBefore":7, "timestamp":2
        }))
        .expect("deserialize legacy compaction summary");
        assert!(matches!(legacy_compaction, Message::CompactionSummary(CompactionSummaryMessage {
            details: None, usage: None, from_hook: None, ..
        })));
    }

    #[test]
    fn constrained_sampling_serialization_preserves_all_modes() {
        let cases = [
            (ConstrainedSampling::Disabled, serde_json::json!(false)),
            (
                ConstrainedSampling::json_schema(ConstrainedSamplingStrictness::Prefer),
                serde_json::json!({"type":"json_schema", "strict":"prefer"}),
            ),
            (
                ConstrainedSampling::json_schema(ConstrainedSamplingStrictness::Require),
                serde_json::json!({"type":"json_schema", "strict":"require"}),
            ),
            (
                ConstrainedSampling::grammar(GrammarVariants {
                    openai_lark: Some("start: /.+/".into()),
                    openai_regex: None,
                }),
                serde_json::json!({"type":"grammar", "variants":{"openai_lark":"start: /.+/"}}),
            ),
        ];

        for (sampling, expected) in cases {
            let encoded = serde_json::to_value(&sampling).expect("serialize constrained sampling");
            assert_eq!(encoded, expected);
            let decoded: ConstrainedSampling =
                serde_json::from_value(encoded).expect("deserialize constrained sampling");
            assert_eq!(decoded, sampling);
        }
    }

    #[test]
    fn json_schema_without_strict_defaults_to_prefer() {
        let sampling: ConstrainedSampling = serde_json::from_value(serde_json::json!({
            "type": "json_schema"
        }))
        .expect("deserialize default strictness");
        assert_eq!(
            sampling,
            ConstrainedSampling::json_schema(ConstrainedSamplingStrictness::Prefer)
        );
        assert_eq!(
            serde_json::to_value(sampling).expect("serialize default strictness"),
            serde_json::json!({"type":"json_schema", "strict":"prefer"})
        );
    }

    #[test]
    fn constrained_sampling_rejects_true_and_unknown_types() {
        assert!(serde_json::from_value::<ConstrainedSampling>(serde_json::json!(true)).is_err());
        assert!(
            serde_json::from_value::<ConstrainedSampling>(serde_json::json!({"type":"bogus"}))
                .is_err()
        );
    }

    #[test]
    fn user_message_plain_string_becomes_single_text_block() {
        let json = serde_json::json!({"content": "hello world", "timestamp": 7});
        let msg: UserMessage = serde_json::from_value(json).expect("plain string content");
        assert_eq!(msg.content, vec![ContentBlock::text("hello world")]);
        assert_eq!(msg.timestamp, 7);
    }

    #[test]
    fn user_message_array_content_parses_unchanged() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "hi"}, {"type": "image", "data": "b348", "mimeType": "image/png"}],
            "timestamp": 9
        });
        let msg: UserMessage = serde_json::from_value(json).expect("array content");
        assert_eq!(
            msg.content,
            vec![
                ContentBlock::text("hi"),
                ContentBlock::Image {
                    data: "b348".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ]
        );
        assert_eq!(msg.timestamp, 9);
    }

    #[test]
    fn content_block_fields_use_camel_case_and_accept_legacy_snake_case() {
        let canonical = serde_json::to_value(ContentBlock::Image {
            data: "b348".to_owned(),
            mime_type: "image/png".to_owned(),
        })
        .expect("serialize image block");
        assert_eq!(
            canonical,
            serde_json::json!({"type": "image", "data": "b348", "mimeType": "image/png"})
        );

        let legacy: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "b348",
            "mime_type": "image/png"
        }))
        .expect("deserialize legacy image block");
        assert_eq!(
            legacy,
            ContentBlock::Image {
                data: "b348".to_owned(),
                mime_type: "image/png".to_owned(),
            }
        );

        let signed_text: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "text",
            "text": "hi",
            "text_signature": "legacy"
        }))
        .expect("deserialize legacy text signature");
        assert_eq!(
            serde_json::to_value(signed_text).expect("serialize text signature"),
            serde_json::json!({"type": "text", "text": "hi", "textSignature": "legacy"})
        );

        let signed_thinking: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "thinking",
            "thinking": "private reasoning",
            "thinking_signature": "legacy-thinking",
            "redacted": true
        }))
        .expect("deserialize legacy thinking signature");
        assert_eq!(
            serde_json::to_value(signed_thinking).expect("serialize thinking signature"),
            serde_json::json!({
                "type": "thinking",
                "thinking": "private reasoning",
                "thinkingSignature": "legacy-thinking",
                "redacted": true
            })
        );
    }

    #[test]
    fn user_message_string_derived_serializes_canonical_array() {
        let msg: UserMessage = serde_json::from_value(serde_json::json!({
            "content": "plain",
            "timestamp": 1
        }))
        .expect("plain string content");
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "content": [{"type": "text", "text": "plain"}],
                "timestamp": 1
            })
        );
    }

    #[test]
    fn user_message_empty_array_parses() {
        let msg: UserMessage =
            serde_json::from_value(serde_json::json!({"content": [], "timestamp": 0}))
                .expect("empty array content");
        assert!(msg.content.is_empty());
    }

    #[test]
    fn user_message_rejects_non_string_or_invalid_block_content() {
        for content in [
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(7),
            serde_json::json!({"text": "not a content block"}),
            serde_json::json!([{"type": "image", "data": "missing mime type"}]),
        ] {
            let value = serde_json::json!({"content": content, "timestamp": 1});
            assert!(
                serde_json::from_value::<UserMessage>(value).is_err(),
                "accepted invalid user content"
            );
        }
    }
}
