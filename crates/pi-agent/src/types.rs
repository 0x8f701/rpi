use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

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

#[must_use]
pub fn compose_before_tool_call(
    first: Option<BeforeToolCallFn>,
    second: Option<BeforeToolCallFn>,
) -> Option<BeforeToolCallFn> {
    match (first, second) {
        (None, None) => None,
        (Some(hook), None) | (None, Some(hook)) => Some(hook),
        (Some(first), Some(second)) => Some(Arc::new(move |context| {
            let first = first.clone();
            let second = second.clone();
            Box::pin(async move {
                let first_result = first(context.clone()).await?;
                if first_result.block {
                    return Ok(first_result);
                }
                let mut next_context = context;
                if let Some(arguments) = first_result.arguments.clone() {
                    next_context.arguments = arguments;
                }
                let second_result = second(next_context).await?;
                Ok(BeforeToolCallResult {
                    block: second_result.block,
                    reason: second_result.reason.or(first_result.reason),
                    arguments: second_result.arguments.or(first_result.arguments),
                })
            })
        })),
    }
}

#[cfg(test)]
mod before_tool_call_composition_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_ai::{AssistantMessage, Model, ToolCall};
    use serde_json::json;

    use super::*;

    fn context() -> BeforeToolCallContext {
        BeforeToolCallContext {
            assistant_message: AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call".to_owned(),
                name: "tool".to_owned(),
                arguments: json!({}),
                thought_signature: None,
            },
            arguments: json!({}),
            context: AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn blocked_first_hook_skips_second() {
        let calls = Arc::new(AtomicUsize::new(0));
        let second_calls = calls.clone();
        let first: BeforeToolCallFn = Arc::new(|_| {
            Box::pin(async {
                Ok(BeforeToolCallResult {
                    block: true,
                    reason: Some("denied".to_owned()),
                    arguments: None,
                })
            })
        });
        let second: BeforeToolCallFn = Arc::new(move |_| {
            second_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let result = compose_before_tool_call(Some(first), Some(second))
            .unwrap()(context())
            .await
            .unwrap();
        assert!(result.block);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
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
#[serde(rename_all = "lowercase")]
pub enum ToolCapability {
    Read,
    Write,
    #[default]
    Exec,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Yolo,
    Write,
    Ask,
}

impl ApprovalMode {
    #[must_use]
    pub const fn requires_approval(self, capability: ToolCapability) -> bool {
        match self {
            Self::Yolo => false,
            Self::Write => matches!(capability, ToolCapability::Exec),
            Self::Ask => true,
        }
    }
}

#[cfg(test)]
mod approval_mode_tests {
    use super::*;

    #[test]
    fn policy_matrix_is_capability_only() {
        for (mode, capability, asks) in [
            (ApprovalMode::Yolo, ToolCapability::Read, false),
            (ApprovalMode::Yolo, ToolCapability::Write, false),
            (ApprovalMode::Yolo, ToolCapability::Exec, false),
            (ApprovalMode::Write, ToolCapability::Read, false),
            (ApprovalMode::Write, ToolCapability::Write, false),
            (ApprovalMode::Write, ToolCapability::Exec, true),
            (ApprovalMode::Ask, ToolCapability::Read, true),
            (ApprovalMode::Ask, ToolCapability::Write, true),
            (ApprovalMode::Ask, ToolCapability::Exec, true),
        ] {
            assert_eq!(mode.requires_approval(capability), asks, "{mode:?} {capability:?}");
        }
    }

    #[test]
    fn default_and_wire_values_are_compatibly_yolo_and_lowercase() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::Yolo);
        for (mode, wire) in [
            (ApprovalMode::Yolo, "\"yolo\""),
            (ApprovalMode::Write, "\"write\""),
            (ApprovalMode::Ask, "\"ask\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ApprovalMode>(wire).unwrap(), mode);
        }
        assert!(serde_json::from_str::<ApprovalMode>("\"WRITE\"").is_err());
    }
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
    pub capability: ToolCapability,
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
            .field("capability", &self.capability)
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
            capability: ToolCapability::default(),
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
    pub fn with_capability(mut self, capability: ToolCapability) -> Self {
        self.capability = capability;
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

/// Executor for the interactive `ask` tool: given the parsed `question`,
/// performs the user round trip (publish the prompt, await the answer) and
/// returns the tool result. The `abort` signal fires when the surrounding run
/// is cancelled, so the round trip must observe it instead of hanging. Hosts
/// wire the actual UI; a non-interactive host must surface an actionable
/// error from the executor.
pub type AskToolExecutorFn =
    Arc<dyn Fn(String, AbortSignal) -> BoxFuture<Result<AgentToolResult>> + Send + Sync>;

/// The JSON schema for the interactive `ask` tool: a single required
/// `question` string property.
#[must_use]
pub fn ask_tool_schema() -> Schema {
    Schema {
        schema_type: Some(serde_json::json!("object")),
        properties: HashMap::from([(
            "question".to_owned(),
            Schema {
                schema_type: Some(serde_json::json!("string")),
                description: Some(
                    "The question to ask the user; answer is returned verbatim as the tool result"
                        .to_owned(),
                ),
                ..Schema::default()
            },
        )]),
        property_order: vec!["question".to_owned()],
        required: vec!["question".to_owned()],
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    }
}

/// The interactive `ask` tool: lets the model ask the user a question mid-task
/// and receives the typed answer as the tool result. Marked `Read` capability
/// because the tool reads user input — it must never itself trigger the
/// approval dialog. The user round trip is delegated to `executor` so hosts
/// can wire their own frontend (interactive TUI prompt, test harness, ...).
#[must_use]
pub fn create_ask_tool(executor: AskToolExecutorFn) -> AgentTool {
    AgentTool::new(
        "ask",
        "Ask the user a question and wait for their answer. Use sparingly to confirm ambiguous requirements, choices, or risky actions the user should decide on before you proceed.",
        ask_tool_schema(),
        move |context: ToolCallContext| {
            let executor = executor.clone();
            async move {
                let question = context
                    .arguments
                    .get("question")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Invalid question: ask requires a string `question` argument")
                    })?;
                executor(question, context.abort).await
            }
        },
    )
    .with_capability(ToolCapability::Read)
    .with_prompt_guidelines(vec![
        "Use ask only when the user's decision materially changes the next step; \
         prefer proceeding with a documented assumption when the question is trivial."
            .to_owned(),
        "Ask one focused question per call; the answer arrives as plain text."
            .to_owned(),
    ])
}

#[cfg(test)]
mod ask_tool_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::AbortController;

    #[test]
    fn ask_tool_schema_requires_single_question_string() {
        let schema = ask_tool_schema();
        assert_eq!(schema.schema_type, Some(serde_json::json!("object")));
        assert_eq!(schema.required, vec!["question".to_owned()]);
        assert_eq!(schema.property_order, vec!["question".to_owned()]);
        assert_eq!(schema.properties.len(), 1);
        let question = schema.properties.get("question").expect("question property");
        assert_eq!(question.schema_type, Some(serde_json::json!("string")));
        assert!(question.description.is_some());
    }

    #[tokio::test]
    async fn ask_tool_routes_question_to_executor_and_returns_answer() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_abort = observed.clone();
        let tool = create_ask_tool(Arc::new(move |question, abort| {
            let observed = observed_abort.clone();
            Box::pin(async move {
                assert_eq!(question, "proceed?");
                assert!(!abort.is_aborted());
                observed.store(true, Ordering::SeqCst);
                Ok(AgentToolResult::text("the answer"))
            })
        }));
        assert_eq!(tool.name, "ask");
        assert_eq!(tool.label, "ask");
        assert_eq!(tool.capability, ToolCapability::Read);
        assert_eq!(tool.execution_mode, ToolExecutionMode::Default);
        let (_, abort) = AbortController::new();
        let result = (tool.execute)(ToolCallContext {
            tool_call_id: "call-1".to_owned(),
            arguments: serde_json::json!({ "question": "proceed?" }),
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
        })
        .await
        .expect("execute");
        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(result.content, vec![ContentBlock::text("the answer")]);
    }

    #[tokio::test]
    async fn ask_tool_rejects_missing_or_non_string_question() {
        let tool = create_ask_tool(Arc::new(|_, _| {
            Box::pin(async { unreachable!("executor must not run for a bad schema") })
        }));
        for arguments in [serde_json::json!({}), serde_json::json!({ "question": 42 })] {
            let (_, abort) = AbortController::new();
            let result = (tool.execute)(ToolCallContext {
                tool_call_id: "call-1".to_owned(),
                arguments: arguments.clone(),
                on_update: Arc::new(|_| {}),
                abort,
                model: None,
            })
            .await
            .expect_err("missing question must reject");
            assert!(
                result.to_string().contains("question"),
                "rejection names the argument: {result}"
            );
        }
    }
}

#[cfg(test)]
mod agent_tool_capability_tests {
    use super::*;

    fn tool_named(name: &str) -> AgentTool {
        AgentTool::new(name, "test tool", Schema::default(), |_| async {
            Ok(AgentToolResult::text("ok"))
        })
    }

    #[test]
    fn generic_agent_tools_default_to_exec_without_name_heuristics() {
        assert_eq!(tool_named("read").capability, ToolCapability::Exec);
        assert_eq!(tool_named("unknown_custom_tool").capability, ToolCapability::Exec);
    }

    #[test]
    fn capability_builder_and_debug_output_expose_metadata() {
        let tool = tool_named("custom").with_capability(ToolCapability::Read);
        assert_eq!(tool.capability, ToolCapability::Read);
        assert!(format!("{tool:?}").contains("capability: Read"));
    }

    #[test]
    fn tool_capability_uses_strict_lowercase_wire_values() {
        for (capability, wire) in [
            (ToolCapability::Read, "\"read\""),
            (ToolCapability::Write, "\"write\""),
            (ToolCapability::Exec, "\"exec\""),
        ] {
            assert_eq!(serde_json::to_string(&capability).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ToolCapability>(wire).unwrap(), capability);
        }
        assert!(serde_json::from_str::<ToolCapability>("\"READ\"").is_err());
        assert!(serde_json::from_str::<ToolCapability>("\"network\"").is_err());
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
