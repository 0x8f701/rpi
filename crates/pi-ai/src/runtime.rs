use std::collections::HashSet;

use serde::Serialize;

use crate::{
    ContentBlock, ContentList, Context, Message, Model, StopReason, ToolDefinition, Usage,
};

const EXTENDED_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const CHARS_PER_TOKEN: i64 = 4;
const ESTIMATED_IMAGE_CHARS: i64 = 4800;
const CONTEXT_SAFETY_TOKENS: i64 = 4096;
const MIN_MAX_TOKENS: i64 = 1;

/// Returns the provider-neutral thinking levels supported by a model.
///
/// Explicit `None` map entries disable a level. `xhigh` and `max` are opt-in;
/// the other levels are available by default for reasoning models.
pub fn supported_thinking_levels(model: &Model) -> Vec<&'static str> {
    if !model.reasoning {
        return vec!["off"];
    }

    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let entry = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(*level));
            if entry.is_some_and(Option::is_none) {
                return false;
            }
            if matches!(*level, "xhigh" | "max") && entry.is_none() {
                return false;
            }
            true
        })
        .collect()
}

/// Clamps a provider-neutral thinking level to the nearest supported level.
///
/// When two supported levels surround a gap, the higher level wins, matching
/// upstream pi's ordered search. Unknown levels fall back to the first level.
pub fn clamp_thinking_level(model: &Model, requested: &str) -> &'static str {
    let available = supported_thinking_levels(model);
    if let Some(level) = available.iter().copied().find(|level| *level == requested) {
        return level;
    }

    let Some(requested_index) = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|level| *level == requested)
    else {
        return available.first().copied().unwrap_or("off");
    };

    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }

    available.first().copied().unwrap_or("off")
}

/// Token estimate for a complete provider-neutral context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: i64,
    pub usage_tokens: i64,
    pub trailing_tokens: i64,
    pub last_usage_index: Option<usize>,
}

/// Estimates context tokens with pi's UTF-16 character heuristic.
///
/// The most recent valid assistant usage anchors the estimate. Without an
/// anchor, the system prompt, tool definitions, and all messages are estimated.
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages(&context.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let mut added_names = HashSet::new();
        for message in &context.messages[last_usage_index + 1..] {
            if let Message::ToolResult(result) = message {
                for name in &result.added_tool_names {
                    added_names.insert(name.as_str());
                }
            }
        }
        let added_tools: Vec<&ToolDefinition> = context
            .tools
            .iter()
            .filter(|tool| added_names.contains(tool.name.as_str()))
            .collect();
        let added_tool_tokens = estimate_tools_tokens(&added_tools);
        return ContextUsageEstimate {
            tokens: estimate.tokens + added_tool_tokens,
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
            last_usage_index: estimate.last_usage_index,
        };
    }

    let system_tokens = if context.system_prompt.is_empty() {
        0
    } else {
        estimate_text_tokens(&context.system_prompt)
    };
    let prefix_tokens = system_tokens + estimate_tools_tokens(&context.tools);
    ContextUsageEstimate {
        tokens: estimate.tokens + prefix_tokens,
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens + prefix_tokens,
        last_usage_index: estimate.last_usage_index,
    }
}

/// Caps requested output tokens to the model context window with pi's safety
/// reserve. Unknown context windows only apply the one-token output floor.
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: i64) -> i64 {
    if model.context_window <= 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }

    let available =
        model.context_window - estimate_context_tokens(context).tokens - CONTEXT_SAFETY_TOKENS;
    max_tokens.min(available.max(MIN_MAX_TOKENS))
}

fn utf16_len(value: &str) -> i64 {
    value.encode_utf16().count() as i64
}

fn estimate_text_tokens(text: &str) -> i64 {
    (utf16_len(text) + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
}

fn estimate_content_tokens(content: &ContentList) -> i64 {
    let chars = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => utf16_len(text),
            ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum::<i64>();
    (chars + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
}

fn estimate_message_tokens(message: &Message) -> i64 {
    match message {
        Message::User(user) => estimate_content_tokens(&user.content),
        Message::ToolResult(result) => estimate_content_tokens(&result.content),
        Message::Assistant(assistant) => {
            let chars = assistant
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text, .. } => utf16_len(text),
                    ContentBlock::Thinking { thinking, .. } => utf16_len(thinking),
                    ContentBlock::ToolCall(call) => {
                        utf16_len(&call.name) + serialized_utf16_len(&call.arguments)
                    }
                    ContentBlock::Image { .. } => 0,
                })
                .sum::<i64>();
            (chars + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
        }
        Message::BashExecution(bash) => {
            if bash.exclude_from_context == Some(true) {
                0
            } else {
                estimate_text_tokens(&crate::bash_execution_to_text(bash))
            }
        }
        Message::Custom(custom) => estimate_content_tokens(&custom.content.to_blocks()),
        Message::BranchSummary(summary) => {
            estimate_message_tokens(&crate::branch_summary_to_user(summary))
        }
        Message::CompactionSummary(summary) => {
            estimate_message_tokens(&crate::compaction_summary_to_user(summary))
        }
    }
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some((last_usage_index, usage_tokens)) = last_assistant_usage(messages) {
        let trailing_tokens = messages[last_usage_index + 1..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(last_usage_index),
        };
    }

    let tokens = messages.iter().map(estimate_message_tokens).sum();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn last_assistant_usage(messages: &[Message]) -> Option<(usize, i64)> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut last = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let usage_tokens = usage_tokens(&assistant.usage);
            if assistant.timestamp >= latest_prefix_timestamp
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                )
                && usage_tokens > 0
            {
                last = Some((index, usage_tokens));
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message_timestamp(message));
    }
    last
}

fn usage_tokens(usage: &Usage) -> i64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn message_timestamp(message: &Message) -> i64 {
    match message {
        Message::User(user) => user.timestamp,
        Message::Assistant(assistant) => assistant.timestamp,
        Message::ToolResult(result) => result.timestamp,
        Message::BashExecution(bash) => bash.timestamp,
        Message::Custom(custom) => custom.timestamp,
        Message::BranchSummary(summary) => summary.timestamp,
        Message::CompactionSummary(summary) => summary.timestamp,
    }
}

fn estimate_tools_tokens<T: Serialize>(tools: &[T]) -> i64 {
    if tools.is_empty() {
        0
    } else {
        (serialized_utf16_len(tools) + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
    }
}

fn serialized_utf16_len<T: Serialize + ?Sized>(value: &T) -> i64 {
    serde_json::to_string(value)
        .map(|json| utf16_len(&json))
        .unwrap_or_else(|_| utf16_len("[unserializable]"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::{AssistantMessage, ContentBlock, StopReason, ToolCall, UserMessage};

    fn reasoning_model() -> Model {
        Model {
            reasoning: true,
            ..Model::default()
        }
    }

    fn assistant(content: ContentList, usage: Usage, timestamp: i64) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp,
        })
    }

    #[test]
    fn xhigh_clamps_down_without_an_explicit_map_entry() {
        let model = reasoning_model();
        assert_eq!(
            supported_thinking_levels(&model),
            vec!["off", "minimal", "low", "medium", "high"]
        );
        assert_eq!(clamp_thinking_level(&model, "xhigh"), "high");
        assert_eq!(clamp_thinking_level(&model, "max"), "high");
    }

    #[test]
    fn explicit_off_and_minimal_mappings_control_support() {
        let mut model = reasoning_model();
        model.thinking_level_map = Some(HashMap::from([
            ("off".to_owned(), None),
            ("minimal".to_owned(), Some("low".to_owned())),
        ]));

        assert_eq!(
            supported_thinking_levels(&model),
            vec!["minimal", "low", "medium", "high"]
        );
        assert_eq!(clamp_thinking_level(&model, "off"), "minimal");
        assert_eq!(clamp_thinking_level(&model, "minimal"), "minimal");
    }

    #[test]
    fn ordinary_levels_pass_through_without_a_map() {
        let model = reasoning_model();
        for level in ["off", "minimal", "low", "medium", "high"] {
            assert_eq!(clamp_thinking_level(&model, level), level);
        }
        assert_eq!(clamp_thinking_level(&Model::default(), "high"), "off");
    }

    #[test]
    fn estimates_text_tool_call_and_image_tokens() {
        let text = Message::user_text("hello", 1);
        assert_eq!(estimate_message_tokens(&text), 2);

        let tool = assistant(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "run".to_owned(),
                arguments: json!({"a": 1}),
                thought_signature: None,
            })],
            Usage::default(),
            2,
        );
        assert_eq!(estimate_message_tokens(&tool), 3);

        let image = Message::User(UserMessage {
            content: vec![ContentBlock::Image {
                data: "ignored".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            timestamp: 3,
        });
        assert_eq!(estimate_message_tokens(&image), 1200);

        let estimate = estimate_context_tokens(&Context {
            messages: vec![text, tool, image],
            ..Context::default()
        });
        assert_eq!(estimate.tokens, 1205);
    }
    #[test]
    fn estimates_only_context_visible_bash_projection() {
        let visible = Message::BashExecution(crate::BashExecutionMessage {
            command: "echo ok".into(),
            output: "ok".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1,
            exclude_from_context: None,
        });
        let excluded = Message::BashExecution(crate::BashExecutionMessage {
            command: "secret".into(),
            output: "hidden".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 2,
            exclude_from_context: Some(true),
        });
        assert_eq!(
            estimate_message_tokens(&visible),
            estimate_text_tokens("Ran `echo ok`\n```\nok\n```")
        );
        assert_eq!(estimate_message_tokens(&excluded), 0);
    }

    #[test]
    fn uses_utf16_lengths_and_the_latest_valid_usage_anchor() {
        assert_eq!(estimate_message_tokens(&Message::user_text("a😀b", 1)), 1);

        let anchored = assistant(
            vec![ContentBlock::text("ok")],
            Usage {
                total_tokens: 1000,
                ..Usage::default()
            },
            2,
        );
        let estimate = estimate_context_tokens(&Context {
            system_prompt: "not added after an anchor".to_owned(),
            messages: vec![
                Message::user_text("hi", 1),
                anchored,
                Message::user_text("xxxxxxxx", 3),
            ],
            ..Context::default()
        });
        assert_eq!(
            estimate,
            ContextUsageEstimate {
                tokens: 1002,
                usage_tokens: 1000,
                trailing_tokens: 2,
                last_usage_index: Some(1),
            }
        );
    }

    #[test]
    fn clamps_max_tokens_to_context_bounds() {
        let no_window = Model {
            context_window: 0,
            max_tokens: 8000,
            ..Model::default()
        };
        assert_eq!(
            clamp_max_tokens_to_context(&no_window, &Context::default(), 8000),
            8000
        );
        assert_eq!(
            clamp_max_tokens_to_context(&no_window, &Context::default(), 0),
            1
        );

        let roomy = Model {
            context_window: 128_000,
            max_tokens: 16_384,
            ..Model::default()
        };
        let small = Context {
            system_prompt: "sys".to_owned(),
            messages: vec![Message::user_text("hi", 1)],
            ..Context::default()
        };
        assert_eq!(
            clamp_max_tokens_to_context(&roomy, &small, roomy.max_tokens),
            16_384
        );

        let constrained = Model {
            context_window: 10_000,
            max_tokens: 8000,
            ..Model::default()
        };
        let large = Context {
            messages: vec![Message::user_text("x".repeat(8000), 1)],
            ..Context::default()
        };
        assert_eq!(
            clamp_max_tokens_to_context(&constrained, &large, constrained.max_tokens),
            3904
        );
        assert_eq!(
            clamp_max_tokens_to_context(&constrained, &large, 7000),
            3904
        );

        let over_budget = Model {
            context_window: 100,
            max_tokens: 8000,
            ..Model::default()
        };
        assert_eq!(clamp_max_tokens_to_context(&over_budget, &large, 8000), 1);
    }
}
