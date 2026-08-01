use std::collections::HashMap;

use crate::{ContentBlock, Context, Message, Role};

const GITHUB_COPILOT_PROVIDER: &str = "github-copilot";

pub(crate) fn dynamic_headers(
    provider: &str,
    context: &Context,
) -> Option<HashMap<String, String>> {
    if provider != GITHUB_COPILOT_PROVIDER {
        return None;
    }
    let initiator = if context
        .messages
        .last()
        .is_some_and(|message| message.role() != Role::User)
    {
        "agent"
    } else {
        "user"
    };
    let mut headers = HashMap::from([
        ("X-Initiator".to_owned(), initiator.to_owned()),
        ("Openai-Intent".to_owned(), "conversation-edits".to_owned()),
    ]);
    if has_vision_input(&context.messages) {
        headers.insert("Copilot-Vision-Request".to_owned(), "true".to_owned());
    }
    Some(headers)
}

fn has_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => user.content.iter().any(is_image),
        Message::ToolResult(result) => result.content.iter().any(is_image),
        _ => false,
    })
}

fn is_image(content: &ContentBlock) -> bool {
    matches!(content, ContentBlock::Image { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolResultMessage, UserMessage};

    fn image() -> ContentBlock {
        ContentBlock::Image {
            data: "aW1n".into(),
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn infers_initiator_and_marks_only_image_requests() {
        let user = Context {
            messages: vec![Message::user_text("hello", 1)],
            ..Context::default()
        };
        let user_headers =
            dynamic_headers(GITHUB_COPILOT_PROVIDER, &user).expect("Copilot headers");
        assert_eq!(
            user_headers.get("X-Initiator").map(String::as_str),
            Some("user")
        );
        assert_eq!(
            user_headers.get("Openai-Intent").map(String::as_str),
            Some("conversation-edits")
        );
        assert!(!user_headers.contains_key("Copilot-Vision-Request"));

        let image_request = Context {
            messages: vec![
                Message::User(UserMessage {
                    content: vec![image()],
                    timestamp: 1,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call".into(),
                    tool_name: "view".into(),
                    content: vec![image()],
                    usage: None,
                    details: None,
                    added_tool_names: vec![],
                    is_error: false,
                    timestamp: 2,
                }),
            ],
            ..Context::default()
        };
        let agent_headers =
            dynamic_headers(GITHUB_COPILOT_PROVIDER, &image_request).expect("Copilot headers");
        assert_eq!(
            agent_headers.get("X-Initiator").map(String::as_str),
            Some("agent")
        );
        assert_eq!(
            agent_headers
                .get("Copilot-Vision-Request")
                .map(String::as_str),
            Some("true")
        );
        assert!(dynamic_headers("openai", &image_request).is_none());
    }
}
