//! Human/TUI presentation for typed orchestration IRC custom messages.
//!
//! Wraps the public `pi_coding` view API with display labels and never exposes
//! the raw `<orchestration-message>` XML wrapper.

use std::borrow::Cow;

use pi_ai::{ContentBlock, CustomMessage};
use pi_coding::{orchestration_message_view, ORCHESTRATION_MESSAGE_TYPE};

pub use pi_coding::ORCHESTRATION_MESSAGE_TYPE as ORCHESTRATION_IRC_TYPE;

/// Display-ready fields for one orchestration IRC message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrchestrationIrcView<'a> {
    pub id: Cow<'a, str>,
    pub from: Cow<'a, str>,
    pub to: Cow<'a, str>,
    pub body: Cow<'a, str>,
    pub reply_to: Option<Cow<'a, str>>,
}

impl OrchestrationIrcView<'_> {
    /// `IRC · <from> → <to>` using already-resolved display labels.
    #[must_use]
    pub fn label(&self, from_label: &str, to_label: &str) -> String {
        format!("IRC · {from_label} → {to_label}")
    }

    /// Optional muted reply metadata row.
    #[must_use]
    pub fn reply_metadata(&self) -> Option<String> {
        self.reply_to
            .as_ref()
            .map(|reply_to| format!("reply to {reply_to}"))
    }

    /// Body content blocks for transcript rows (never the XML envelope).
    #[must_use]
    pub fn body_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        if !self.body.is_empty() {
            blocks.push(ContentBlock::text(self.body.as_ref()));
        }
        if let Some(metadata) = self.reply_metadata() {
            blocks.push(ContentBlock::text(metadata));
        }
        blocks
    }
}

/// Prefer the coding-crate view helper (plain body, no XML wrapper).
#[must_use]
pub fn orchestration_irc_view(message: &CustomMessage) -> Option<OrchestrationIrcView<'static>> {
    let view = orchestration_message_view(message)?;
    Some(OrchestrationIrcView {
        id: Cow::Owned(view.id),
        from: Cow::Owned(view.from),
        to: Cow::Owned(view.to),
        body: Cow::Owned(view.body),
        reply_to: view.reply_to.map(Cow::Owned),
    })
}

/// Build a view directly from a delivered mailbox projection (Main-bound live IRC).
#[must_use]
pub fn orchestration_irc_view_from_mailbox<'a>(
    id: &'a str,
    from: &'a str,
    to: &'a str,
    body: &'a str,
    reply_to: Option<&'a str>,
) -> OrchestrationIrcView<'a> {
    OrchestrationIrcView {
        id: Cow::Borrowed(id),
        from: Cow::Borrowed(from),
        to: Cow::Borrowed(to),
        body: Cow::Borrowed(body),
        reply_to: reply_to.map(Cow::Borrowed),
    }
}

/// True when the custom message is a typed orchestration IRC payload.
#[must_use]
pub fn is_orchestration_irc_message(message: &CustomMessage) -> bool {
    message.custom_type == ORCHESTRATION_MESSAGE_TYPE
        || orchestration_message_view(message).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn custom(
        from: &str,
        to: &str,
        body_xml: &str,
        reply_to: Option<&str>,
        details_body: Option<&str>,
    ) -> CustomMessage {
        let mut details = json!({
            "id": "msg-1",
            "from": from,
            "to": to,
            "replyTo": reply_to,
        });
        if let Some(body) = details_body {
            details
                .as_object_mut()
                .expect("object")
                .insert("body".to_owned(), json!(body));
        }
        CustomMessage {
            custom_type: ORCHESTRATION_MESSAGE_TYPE.to_owned(),
            content: body_xml.into(),
            display: true,
            details: Some(details),
            timestamp: 1,
        }
    }

    #[test]
    fn extracts_plain_body_without_raw_xml_or_reply_trailer() {
        let message = custom(
            "Main",
            "Child",
            "<orchestration-message id=\"msg-1\" from=\"Main\">\nhello child\nReplying to message: parent-9\n</orchestration-message>",
            Some("parent-9"),
            None,
        );
        let view = orchestration_irc_view(&message).expect("view");
        assert_eq!(view.from.as_ref(), "Main");
        assert_eq!(view.to.as_ref(), "Child");
        assert_eq!(view.body.as_ref(), "hello child");
        assert_eq!(view.reply_to.as_deref(), Some("parent-9"));
        assert!(!view.body.contains("orchestration-message"));
        assert!(!view.body.contains("Replying to message"));
        assert_eq!(view.label("Main", "task: child"), "IRC · Main → task: child");
        assert_eq!(view.reply_metadata().as_deref(), Some("reply to parent-9"));
    }

    #[test]
    fn prefers_details_body_when_present() {
        let message = custom(
            "Alpha",
            "Beta",
            "<orchestration-message id=\"msg-1\" from=\"Alpha\">\nignored xml\n</orchestration-message>",
            None,
            Some("plain body"),
        );
        let view = orchestration_irc_view(&message).expect("view");
        assert_eq!(view.body.as_ref(), "plain body");
        assert!(view.reply_metadata().is_none());
    }

    #[test]
    fn ignores_unrelated_custom_types() {
        let message = CustomMessage {
            custom_type: "release-note".to_owned(),
            content: "hi".into(),
            display: true,
            details: Some(json!({"id":"x","from":"a","to":"b"})),
            timestamp: 1,
        };
        assert!(orchestration_irc_view(&message).is_none());
    }
}
