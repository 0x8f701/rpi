use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, Schema,
    SimpleStreamOptions, ToolCall, ToolDefinition, ToolResultMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AbortSignal;

pub type AgentMessage = Message;
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
pub type StreamFn = Arc<
    dyn Fn(Model, Context, SimpleStreamOptions) -> BoxFuture<pi_ai::AssistantMessageEventStream>
        + Send
        + Sync,
>;
pub type ToolUpdateFn = Arc<dyn Fn(AgentToolResult) + Send + Sync>;
pub type PrepareArgumentsFn = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;
pub type ToolExecuteFn =
    Arc<dyn Fn(ToolCallContext) -> BoxFuture<Result<AgentToolResult>> + Send + Sync>;
pub type ConvertToLlmFn = Arc<dyn Fn(Vec<AgentMessage>) -> Result<Vec<Message>> + Send + Sync>;
pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, AbortSignal) -> BoxFuture<Result<Vec<AgentMessage>>> + Send + Sync,
>;
pub type BeforeAgentStartFn = Arc<
    dyn Fn(BeforeAgentStartContext) -> BoxFuture<Result<BeforeAgentStartResult>> + Send + Sync,
>;
pub type TransformMessageFn = Arc<
    dyn Fn(AgentMessage, AbortSignal) -> BoxFuture<Result<AgentMessage>> + Send + Sync,
>;
pub type GetApiKeyFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type BeforeToolCallFn =
    Arc<dyn Fn(BeforeToolCallContext) -> BoxFuture<Result<BeforeToolCallResult>> + Send + Sync>;
pub type AfterToolCallFn =
    Arc<dyn Fn(AfterToolCallContext) -> BoxFuture<Result<AfterToolCallResult>> + Send + Sync>;
pub type ShouldStopAfterTurnFn = Arc<dyn Fn(&ShouldStopAfterTurnContext) -> bool + Send + Sync>;
pub type PrepareNextTurnFn =
    Arc<dyn Fn(&ShouldStopAfterTurnContext) -> Option<AgentLoopTurnUpdate> + Send + Sync>;
pub type MessageQueueFn = Arc<dyn Fn() -> BoxFuture<Vec<AgentMessage>> + Send + Sync>;
pub type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<Result<()>> + Send + Sync>;
pub type Listener = Arc<dyn Fn(AgentEvent, AbortSignal) -> BoxFuture<Result<()>> + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExecutionMode {
    #[default]
    Default,
    Sequential,
    Parallel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    /// Highest effort level. Only some model families accept it; callers should
    /// clamp via model metadata before issuing a provider request.
    Max,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_ai::Usage>,
    #[serde(default)]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    #[serde(default)]
    pub terminate: bool,
}

impl AgentToolResult {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            usage: None,
            details: Value::Object(Default::default()),
            added_tool_names: Vec::new(),
            terminate: false,
        }
    }
}

#[derive(Clone)]
pub struct ToolCallContext {
    pub tool_call_id: String,
    pub arguments: Value,
    pub on_update: ToolUpdateFn,
    pub abort: AbortSignal,
    /// Model selected for the current turn. Manual/standalone tool calls may omit it.
    pub model: Option<Model>,
}

#[derive(Clone)]
pub struct AgentTool {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Schema,
    pub execution_mode: ToolExecutionMode,
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    pub prompt_guidelines: Vec<String>,
    pub constrained_sampling: Option<pi_ai::ConstrainedSampling>,
    pub execute: ToolExecuteFn,
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("execution_mode", &self.execution_mode)
            .field("prompt_guidelines", &self.prompt_guidelines)
            .field("constrained_sampling", &self.constrained_sampling)
            .finish_non_exhaustive()
    }
}

impl AgentTool {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Schema,
        execute: F,
    ) -> Self
    where
        F: Fn(ToolCallContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<AgentToolResult>> + Send + 'static,
    {
        let name = name.into();
        Self {
            label: name.clone(),
            name,
            description: description.into(),
            parameters,
            execution_mode: ToolExecutionMode::Default,
            prepare_arguments: None,
            prompt_guidelines: Vec::new(),
            constrained_sampling: None,
            execute: Arc::new(move |context| Box::pin(execute(context))),
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    #[must_use]
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }

    #[must_use]
    pub fn with_prepare_arguments<F>(mut self, prepare: F) -> Self
    where
        F: Fn(Value) -> Result<Value> + Send + Sync + 'static,
    {
        self.prepare_arguments = Some(Arc::new(prepare));
        self
    }

    #[must_use]
    pub fn with_prompt_guidelines(mut self, guidelines: Vec<String>) -> Self {
        self.prompt_guidelines = guidelines;
        self
    }

    #[must_use]
    pub fn as_tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            constrained_sampling: self.constrained_sampling.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<AgentTool>,
}

#[derive(Clone, Debug)]
pub struct BeforeAgentStartContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
}

#[derive(Clone, Debug)]
pub struct BeforeAgentStartResult {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
}

#[derive(Clone, Debug)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub arguments: Value,
    pub context: AgentContext,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub arguments: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub arguments: Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<pi_ai::Usage>,
    pub terminate: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<AgentMessage>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Clone, Default)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub reasoning: ThinkingLevel,
    pub stream_options: SimpleStreamOptions,
    pub tool_execution: ToolExecutionMode,
    pub skip_initial_steering: bool,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<MessageQueueFn>,
    pub get_follow_up_messages: Option<MessageQueueFn>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentLoopConfig")
            .field("model", &self.model)
            .field("reasoning", &self.reasoning)
            .field("stream_options", &self.stream_options)
            .field("tool_execution", &self.tool_execution)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AssistantMessage,
        #[serde(rename = "toolResults")]
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Tool-call arguments. Serialized as `args` to match pi-agent-core.
        #[serde(rename = "args")]
        arguments: Value,
    },
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Tool-call arguments. Serialized as `args` to match pi-agent-core.
        #[serde(rename = "args")]
        arguments: Value,
        #[serde(rename = "partialResult")]
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        result: AgentToolResult,
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

impl AgentEvent {
    #[must_use]
    pub const fn event_type(&self) -> AgentEventType {
        match self {
            Self::AgentStart => AgentEventType::AgentStart,
            Self::AgentEnd { .. } => AgentEventType::AgentEnd,
            Self::TurnStart => AgentEventType::TurnStart,
            Self::TurnEnd { .. } => AgentEventType::TurnEnd,
            Self::MessageStart { .. } => AgentEventType::MessageStart,
            Self::MessageUpdate { .. } => AgentEventType::MessageUpdate,
            Self::MessageEnd { .. } => AgentEventType::MessageEnd,
            Self::ToolExecutionStart { .. } => AgentEventType::ToolExecutionStart,
            Self::ToolExecutionUpdate { .. } => AgentEventType::ToolExecutionUpdate,
            Self::ToolExecutionEnd { .. } => AgentEventType::ToolExecutionEnd,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventType {
    AgentStart,
    AgentEnd,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
}
