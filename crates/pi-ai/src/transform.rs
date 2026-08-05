use std::collections::{HashMap, HashSet};

use crate::{
    AssistantMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    ContentBlock, CustomMessage, Message, Model, StopReason, ToolCall, ToolResultMessage,
    UserMessage, now_millis,
};

const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[must_use]
pub fn custom_message_to_user(message: &CustomMessage) -> Message {
    Message::User(UserMessage {
        content: message.content.to_blocks(),
        timestamp: message.timestamp,
    })
}

#[must_use]
pub fn branch_summary_to_user(message: &BranchSummaryMessage) -> Message {
    Message::user_text(
        format!(
            "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
            message.summary
        ),
        message.timestamp,
    )
}

#[must_use]
pub fn compaction_summary_to_user(message: &CompactionSummaryMessage) -> Message {
    Message::user_text(
        format!(
            "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
            message.summary
        ),
        message.timestamp,
    )
}

fn replace_images(content: &[ContentBlock], placeholder: &str) -> Vec<ContentBlock> {
    let mut out = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            ContentBlock::Image { .. } => {
                if !previous_was_placeholder {
                    out.push(ContentBlock::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            ContentBlock::Text { text, .. } => {
                previous_was_placeholder = text == placeholder;
                out.push(block.clone());
            }
            _ => {
                previous_was_placeholder = false;
                out.push(block.clone());
            }
        }
    }
    out
}
/// Renders a coding-session bash execution exactly as upstream exposes it to
/// the LLM. The original `bashExecution` message remains available to session
/// and RPC consumers; provider requests receive only this user-text projection.
#[must_use]
pub fn bash_execution_to_text(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(&message.output);
        text.push_str("\n```");
    }
    if message.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if message.exit_code.is_some_and(|code| code != 0) {
        text.push_str(&format!(
            "\n\nCommand exited with code {}",
            message.exit_code.unwrap_or_default()
        ));
    }
    if message.truncated {
        if let Some(path) = message.full_output_path.as_deref() {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
    }
    text
}

/// Projects serializable session messages to the provider-compatible role set.
/// `excludeFromContext: true` bash messages are omitted; all other bash messages
/// become explicit user text and are never passed through as an unknown role.
#[must_use]
pub fn messages_to_llm(messages: &[Message]) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::User(user) => result.push(Message::User(user.clone())),
            Message::Assistant(assistant) => result.push(Message::Assistant(assistant.clone())),
            Message::ToolResult(tool_result) => {
                result.push(Message::ToolResult(tool_result.clone()))
            }
            Message::BashExecution(bash) if bash.exclude_from_context == Some(true) => {}
            Message::BashExecution(bash) => result.push(Message::user_text(
                bash_execution_to_text(bash),
                bash.timestamp,
            )),
            Message::Custom(custom) => result.push(custom_message_to_user(custom)),
            Message::BranchSummary(summary) => result.push(branch_summary_to_user(summary)),
            Message::CompactionSummary(summary) => result.push(compaction_summary_to_user(summary)),
        }
    }
    result
}

fn flush_pending(
    result: &mut Vec<Message>,
    pending: &mut Vec<ToolCall>,
    found: &mut HashSet<String>,
) {
    for call in pending.drain(..) {
        if !found.contains(&call.id) {
            result.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id,
                tool_name: call.name,
                content: vec![ContentBlock::text("No result provided")],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp: now_millis(),
            }));
        }
    }
    found.clear();
}

/// Normalizes messages for replay through `model`. The callback is invoked only for
/// cross-model tool calls and receives the original id, target model, and source message.
pub fn transform_messages<F>(
    messages: &[Message],
    model: &Model,
    mut normalize_id: F,
) -> Vec<Message>
where
    F: FnMut(&str, &Model, &AssistantMessage) -> String,
{
    let messages = messages_to_llm(messages);
    let images = model.input.iter().any(|input| input == "image");
    let mut id_map = HashMap::<String, String>::new();
    let mut first_pass = Vec::with_capacity(messages.len());
    for message in &messages {
        match message {
            Message::User(user) => {
                let mut copy = user.clone();
                if !images {
                    copy.content = replace_images(&copy.content, USER_IMAGE_PLACEHOLDER);
                }
                first_pass.push(Message::User(copy));
            }
            Message::ToolResult(tool_result) => {
                let mut copy = tool_result.clone();
                if !images {
                    copy.content = replace_images(&copy.content, TOOL_IMAGE_PLACEHOLDER);
                }
                if let Some(id) = id_map.get(&copy.tool_call_id) {
                    copy.tool_call_id.clone_from(id);
                }
                first_pass.push(Message::ToolResult(copy));
            }
            Message::Assistant(assistant) => {
                let same = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut content = Vec::with_capacity(assistant.content.len());
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted {
                                if same {
                                    content.push(block.clone());
                                }
                            } else if same
                                && thinking_signature.as_deref().is_some_and(|s| !s.is_empty())
                            {
                                content.push(block.clone());
                            } else if !thinking.trim().is_empty() {
                                if same {
                                    content.push(block.clone());
                                } else {
                                    content.push(ContentBlock::text(thinking));
                                }
                            }
                        }
                        ContentBlock::Text { text, .. } => {
                            if same {
                                content.push(block.clone());
                            } else {
                                content.push(ContentBlock::text(text));
                            }
                        }
                        ContentBlock::ToolCall(call) => {
                            let mut copy = call.clone();
                            if !same {
                                copy.thought_signature = None;
                                let normalized = normalize_id(&copy.id, model, assistant);
                                if normalized != copy.id {
                                    id_map.insert(copy.id.clone(), normalized.clone());
                                    copy.id = normalized;
                                }
                            }
                            content.push(ContentBlock::ToolCall(copy));
                        }
                        ContentBlock::Image { .. } => content.push(block.clone()),
                    }
                }
                let mut copy = assistant.clone();
                copy.content = content;
                first_pass.push(Message::Assistant(copy));
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("session messages are projected before provider transforms")
            }
        }
    }

    let mut result = Vec::with_capacity(first_pass.len());
    let mut pending = Vec::<ToolCall>::new();
    let mut found = HashSet::<String>::new();
    for message in first_pass {
        match message {
            Message::Assistant(assistant) => {
                flush_pending(&mut result, &mut pending, &mut found);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                pending = assistant
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolCall(c) = b {
                            Some(c.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                if pending
                    .iter()
                    .any(|call| call.id == tool_result.tool_call_id)
                {
                    found.insert(tool_result.tool_call_id.clone());
                    result.push(Message::ToolResult(tool_result));
                } else {
                    flush_pending(&mut result, &mut pending, &mut found);
                    result.push(Message::User(UserMessage {
                        content: tool_result.content,
                        timestamp: tool_result.timestamp,
                    }));
                }
            }
            Message::User(user) => {
                flush_pending(&mut result, &mut pending, &mut found);
                result.push(Message::User(user));
            }
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                unreachable!("session messages are projected before provider transforms")
            }
        }
    }
    flush_pending(&mut result, &mut pending, &mut found);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{API_ANTHROPIC_MESSAGES, API_OPENAI_RESPONSES, Usage};
    use serde_json::json;

    fn model(provider: &str, api: &str, id: &str, images: bool) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: api.into(),
            provider: provider.into(),
            input: if images {
                vec!["text".into(), "image".into()]
            } else {
                vec!["text".into()]
            },
            ..Model::default()
        }
    }
    fn assistant(provider: &str, api: &str, model: &str, content: Vec<ContentBlock>) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 2,
        })
    }
    fn call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
        })
    }
    fn result(id: &str, content: Vec<ContentBlock>) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content,
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 3,
        })
    }

    #[test]
    fn bash_messages_are_projected_or_excluded_before_provider_transforms() {
        let target = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", true);
        let included = Message::BashExecution(BashExecutionMessage {
            command: "false".into(),
            output: "bad".into(),
            exit_code: Some(1),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/full.log".into()),
            timestamp: 7,
            exclude_from_context: None,
        });
        let excluded = Message::BashExecution(BashExecutionMessage {
            command: "secret".into(),
            output: "hidden".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 8,
            exclude_from_context: Some(true),
        });
        let out = transform_messages(&[included, excluded], &target, |id, _, _| id.into());
        assert_eq!(
            out,
            vec![Message::user_text(
                "Ran `false`\n```\nbad\n```\n\nCommand exited with code 1\n\n[Output truncated. Full output: /tmp/full.log]",
                7,
            )]
        );
    }
    #[test]
    fn extensible_messages_project_to_user_content_independent_of_display() {
        let image = ContentBlock::Image {
            data: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        };
        let messages = vec![
            Message::Custom(CustomMessage {
                custom_type: "visible".into(),
                content: "shown to model".into(),
                display: true,
                details: Some(json!({"not":"projected"})),
                timestamp: 1,
            }),
            Message::Custom(CustomMessage {
                custom_type: "hidden".into(),
                content: vec![ContentBlock::text("hidden from UI"), image.clone()].into(),
                display: false,
                details: None,
                timestamp: 2,
            }),
            Message::BranchSummary(BranchSummaryMessage {
                summary: "branch".into(),
                from_id: "old-leaf".into(),
                details: None,
                usage: None,
                from_hook: None,
                timestamp: 3,
            }),
            Message::CompactionSummary(CompactionSummaryMessage {
                summary: "compact".into(),
                tokens_before: 9000,
                details: None,
                usage: None,
                from_hook: None,
                timestamp: 4,
            }),
        ];
        assert_eq!(
            messages_to_llm(&messages),
            vec![
                Message::User(UserMessage {
                    content: vec![ContentBlock::text("shown to model")],
                    timestamp: 1
                }),
                Message::User(UserMessage {
                    content: vec![ContentBlock::text("hidden from UI"), image],
                    timestamp: 2
                }),
                Message::user_text(
                    format!("{BRANCH_SUMMARY_PREFIX}branch{BRANCH_SUMMARY_SUFFIX}"),
                    3
                ),
                Message::user_text(
                    format!("{COMPACTION_SUMMARY_PREFIX}compact{COMPACTION_SUMMARY_SUFFIX}"),
                    4
                ),
            ]
        );
    }

    #[test]
    fn inserts_orphan_result() {
        let m = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", true);
        let out = transform_messages(
            &[
                assistant(
                    "anthropic",
                    API_ANTHROPIC_MESSAGES,
                    "claude",
                    vec![call("c")],
                ),
                Message::user_text("next", 4),
            ],
            &m,
            |id, _, _| id.into(),
        );
        assert!(matches!(&out[1],Message::ToolResult(r) if r.tool_call_id=="c"&&r.is_error));
    }
    #[test]
    fn normalizes_call_and_result_ids() {
        let m = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", true);
        let out = transform_messages(
            &[
                assistant("openai", API_OPENAI_RESPONSES, "gpt", vec![call("c|f")]),
                result("c|f", vec![ContentBlock::text("ok")]),
            ],
            &m,
            |id, target, source| {
                assert_eq!(target.id, "claude");
                assert_eq!(source.provider, "openai");
                id.replace('|', "_")
            },
        );
        assert!(
            matches!(&out[0],Message::Assistant(a) if matches!(&a.content[0],ContentBlock::ToolCall(c) if c.id=="c_f"))
        );
        assert!(matches!(&out[1],Message::ToolResult(r) if r.tool_call_id=="c_f"));
    }
    #[test]
    fn repairs_leading_tool_result() {
        let m = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", true);
        let content = vec![ContentBlock::text("kept")];
        let out = transform_messages(&[result("missing", content.clone())], &m, |id, _, _| {
            id.into()
        });
        assert_eq!(
            out,
            vec![Message::User(UserMessage {
                content,
                timestamp: 3
            })]
        );
    }
    #[test]
    fn downgrades_unsupported_images() {
        let m = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", false);
        let image = ContentBlock::Image {
            data: "x".into(),
            mime_type: "image/png".into(),
        };
        let out = transform_messages(
            &[
                Message::User(UserMessage {
                    content: vec![image.clone(), image.clone()],
                    timestamp: 1,
                }),
                assistant(
                    "anthropic",
                    API_ANTHROPIC_MESSAGES,
                    "claude",
                    vec![call("c")],
                ),
                result("c", vec![image]),
            ],
            &m,
            |id, _, _| id.into(),
        );
        assert!(
            matches!(&out[0],Message::User(u) if u.content==vec![ContentBlock::text(USER_IMAGE_PLACEHOLDER)])
        );
        assert!(
            matches!(&out[2],Message::ToolResult(r) if r.content==vec![ContentBlock::text(TOOL_IMAGE_PLACEHOLDER)])
        );
    }
    #[test]
    fn preserves_and_removes_thinking_signatures() {
        let source = model("openai", API_OPENAI_RESPONSES, "gpt", true);
        let target = model("anthropic", API_ANTHROPIC_MESSAGES, "claude", true);
        let content = vec![
            ContentBlock::Thinking {
                thinking: "reason".into(),
                thinking_signature: Some("sig".into()),
                redacted: false,
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: Some("encrypted".into()),
                redacted: false,
            },
            ContentBlock::Thinking {
                thinking: "opaque".into(),
                thinking_signature: Some("redacted".into()),
                redacted: true,
            },
            ContentBlock::Text {
                text: "answer".into(),
                text_signature: Some("text-sig".into()),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
                thought_signature: Some("tool-sig".into()),
            }),
        ];
        let messages = [
            assistant("openai", API_OPENAI_RESPONSES, "gpt", content.clone()),
            result("c", vec![ContentBlock::text("ok")]),
        ];
        let same = transform_messages(&messages, &source, |_, _, _| panic!());
        assert_eq!(same[0].as_assistant().unwrap().content, content);
        let cross = transform_messages(&messages, &target, |id, _, _| id.into());
        assert_eq!(
            cross[0].as_assistant().unwrap().content,
            vec![
                ContentBlock::text("reason"),
                ContentBlock::text("answer"),
                call("c")
            ]
        );
    }
}
